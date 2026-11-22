//! Index access-range construction — the `util/ranger` subset SealDB needs for
//! an index key prefix.
//!
//! TiDB's detacher pulls every comparison on the index prefix out of the cop
//! Selection and hands the merged bounds to `IndexRangeScan`, which keeps only
//! rows inside `range:…`. SealDB seeks with the same bounds at execution time,
//! so the planner exposes the very same key ranges to EXPLAIN, to the
//! cardinality estimate, and to the executor.
use super::physical_plans::IndexScanRange;
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_statistics::pseudo;
use sealdb_types::{Datum, EnumValue};

pub struct IndexAccessRange {
    /// `range:…` text + estRows for EXPLAIN.
    pub range: IndexScanRange,
    /// Conjuncts fully encoded by the range (they leave the cop Selection).
    pub consumed: Vec<ExprNode>,
    /// Coerced seek bounds of every range in the set.
    pub ranges: Vec<pseudo::Bounds>,
    /// Composite seek ranges (TiDB `ranger.Range`), one per access path.
    pub key_ranges: Vec<pseudo::KeyRange>,
}

/// One key part of an index key: the column the seek encodes plus its type.
struct KeyPart {
    column: String,
    field_type: sealdb_types::FieldType,
}

/// Key parts of `index`, in seek order.
///
/// A secondary index key carries the row handle after the index columns — the
/// `_tidb_rowid` of a rowid table or the remaining PRIMARY KEY columns of a
/// common handle — so a range on a handle column (`index c2(c2)` with
/// `c1 > 1 and c2 = 1`) still seeks `(c2, c1)`. A unique index stores the
/// handle in the row value instead, so its key stops at the index columns.
fn index_key_parts(info: &TableInfo, index: &IndexInfo) -> Option<Vec<KeyPart>> {
    let mut parts: Vec<KeyPart> = Vec::new();
    for part in &index.columns {
        let column = super::expr_index::part_key(info, part);
        let column_info = match super::expr_index::hidden_column(info, part) {
            Some(hidden) => hidden,
            None => info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(&column))?,
        };
        parts.push(KeyPart {
            column,
            field_type: column_info.field_type.clone(),
        });
    }
    if index.unique {
        return Some(parts);
    }
    let covered = |parts: &[KeyPart], column: &str| {
        parts
            .iter()
            .any(|part| part.column.eq_ignore_ascii_case(column))
    };
    if info.common_handle {
        let pk = info.indices.iter().find(|idx| idx.primary)?;
        for part in &pk.columns {
            let name = super::expr_index::part_key(info, part);
            if covered(&parts, &name) {
                continue;
            }
            let column_info = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(&name))?;
            parts.push(KeyPart {
                column: name,
                field_type: column_info.field_type.clone(),
            });
        }
    } else if info.pk_is_handle {
        // Only an integer PRIMARY KEY has a row handle named after a column.
        // `handle_column()` falls back to the first INT column for legacy
        // tables, which a heap table must not use: its key carries the hidden
        // `_tidb_rowid` (so `KEY(name)` + `id > 1` stays a row filter).
        if let Some(handle) = info.handle_column()
            && !covered(&parts, handle.name.o())
        {
            parts.push(KeyPart {
                column: handle.name.o().to_string(),
                field_type: handle.field_type.clone(),
            });
        }
    } else if !covered(&parts, "_tidb_rowid") {
        parts.push(KeyPart {
            column: "_tidb_rowid".to_string(),
            field_type: sealdb_types::FieldType::new_longlong(),
        });
    }
    Some(parts)
}

/// Number of key parts of `index`, including the appended row handle of a
/// non-unique index (`index_key_parts` order).
pub fn index_key_part_count(info: &TableInfo, index: &IndexInfo) -> Option<usize> {
    index_key_parts(info, index).map(|parts| parts.len())
}

/// Coerce a seek key datum to the storage type of key part `pos`.
///
/// Ranger converts every literal bound to the index column's type before
/// building the seek key, so `c = 1` on a FLOAT part must encode the same
/// `3.0` the writer stored (`c` in `key mvi(c, (cast(j as unsigned array)))`).
pub fn coerce_key_datum(
    info: &TableInfo,
    index: &IndexInfo,
    pos: usize,
    datum: &sealdb_types::Datum,
) -> Option<sealdb_types::Datum> {
    let parts = index_key_parts(info, index)?;
    let part = parts.get(pos)?;
    coerce_bound(&part.field_type, Some((datum.clone(), true))).map(|(value, _)| value)
}

/// Composite seek ranges of `index` decided by `conjuncts` alone (no
/// statistics), or `None` when no range can be built.
pub struct KeyRangeAccess {
    pub ranges: Vec<pseudo::KeyRange>,
    /// Conjuncts the ranges fully encode.
    pub consumed: Vec<ExprNode>,
}

pub fn index_key_ranges(
    info: &TableInfo,
    index: &IndexInfo,
    conjuncts: &[ExprNode],
) -> Option<KeyRangeAccess> {
    let conjunct_access = conjunction_key_ranges(info, index, conjuncts);
    if conjunct_access.is_some() {
        return conjunct_access;
    }
    // `(c1=1 and c2=2) or (c1=3 and c2=4)` over one index: every arm pins a
    // common key prefix, so TiDB's ranger seeks the index once with one point
    // range per arm (`range:[0 1,0 1], [1 3,1 3]`).
    let [single] = conjuncts else {
        return None;
    };
    let ranges = dnf_point_ranges(info, index, single)?;
    Some(KeyRangeAccess {
        ranges,
        consumed: vec![single.clone()],
    })
}

/// One point range per DNF arm when every arm pins the same key prefix.
fn dnf_point_ranges(
    info: &TableInfo,
    index: &IndexInfo,
    conjunct: &ExprNode,
) -> Option<Vec<pseudo::KeyRange>> {
    let arms = conjunct.split_dnf();
    if arms.len() < 2 {
        return None;
    }
    let mut ranges: Vec<pseudo::KeyRange> = Vec::with_capacity(arms.len());
    let mut width: Option<usize> = None;
    for arm in &arms {
        // Each arm is a conjunction: split it so every equality can pin a key
        // part (`c1=0 and c2=1` → `[0 1,0 1]`).
        let arm_access = index_key_ranges(info, index, &arm.split_cnf())?;
        let [range] = arm_access.ranges.as_slice() else {
            return None;
        };
        // The arm has to pin a fully-inclusive prefix of the key, so the range
        // is a single key (`[0 1,0 1]`) rather than an open interval.
        if range.low_open || range.high_open || range.low.len() != range.high.len() {
            return None;
        }
        let is_point = range.low.iter().zip(range.high.iter()).all(|(low, high)| {
            low.1 && high.1 && low.0.compare(&high.0) == std::cmp::Ordering::Equal
        });
        if !is_point {
            return None;
        }
        match width {
            Some(prev) if prev != range.low.len() => return None,
            _ => width = Some(range.low.len()),
        }
        ranges.push(range.clone());
    }
    Some(ranges)
}

fn conjunction_key_ranges(
    info: &TableInfo,
    index: &IndexInfo,
    conjuncts: &[ExprNode],
) -> Option<KeyRangeAccess> {
    let parts = index_key_parts(info, index)?;
    let leading = parts.first()?;
    // TIMESTAMP index keys are stored in UTC, so session-local bounds cannot be
    // pushed as a seek; the executor keeps the full scan + Selection instead.
    if leading.field_type.tp == sealdb_types::TYPE_TIMESTAMP {
        return None;
    }
    let mut pinned: Vec<sealdb_types::Datum> = Vec::new();
    let mut consumed: Vec<ExprNode> = Vec::new();
    let mut ranges: Vec<pseudo::KeyRange> = Vec::new();
    for part in &parts {
        // Bounds every conjunct imposes on this key part, intersected.
        let mut bounds_set: Vec<pseudo::Bounds> = Vec::new();
        let mut part_conjuncts: Vec<ExprNode> = Vec::new();
        for conjunct in conjuncts {
            if consumed
                .iter()
                .any(|used| used.restore_string() == conjunct.restore_string())
            {
                continue;
            }
            // A `LIKE 'prefix%'` only bounds the key; the pattern still has to
            // filter the rows the range returns, so it is never consumed.
            // Check LIKE before `conjunct_bound_sets` — that helper also
            // models prefix LIKE for selectivity, which would mark it encoded.
            let (name, bounds, encoded) =
                if let Some((name, low, high)) = pseudo::like_prefix_bound(conjunct) {
                    (name, vec![(Some((low, true)), Some((high, false)))], false)
                } else if let Some((name, bounds)) = pseudo::conjunct_bound_sets(conjunct) {
                    (name, bounds, true)
                } else {
                    continue;
                };
            if !name.eq_ignore_ascii_case(&part.column) {
                continue;
            }
            // Ranger converts each bound to the key column's type before
            // building the key (`a < 5429` on a DECIMAL column).
            let bounds: Vec<pseudo::Bounds> = bounds
                .into_iter()
                .map(|(low, high)| {
                    (
                        coerce_bound(&part.field_type, low),
                        coerce_bound(&part.field_type, high),
                    )
                })
                .collect();
            // Ranger converts each bound to the column charset; a binary
            // literal that is not valid UTF-8 must surface the conversion
            // error from the cop Selection instead of being seeked as bytes.
            if !bounds
                .iter()
                .all(|(low, high)| bounds_fit_column(&part.field_type, low, high))
            {
                continue;
            }
            if bounds_set.is_empty() {
                bounds_set = bounds;
            } else {
                pseudo::intersect_sets(&mut bounds_set, &bounds);
                if bounds_set.is_empty() {
                    return None;
                }
            }
            if encoded {
                part_conjuncts.push(conjunct.clone());
            }
        }
        if bounds_set.is_empty() {
            break;
        }
        // `PrefixEqualLen`: a part pinned by `col = const` (or an `IN` list
        // folded to a single value) extends the key prefix.
        let point = bounds_set.len() == 1
            && matches!(
                (&bounds_set[0].0, &bounds_set[0].1),
                (Some((low, true)), Some((high, true)))
                    if low.compare(high) == std::cmp::Ordering::Equal
            );
        if point {
            pinned.push(bounds_set[0].0.clone()?.0);
            consumed.extend(part_conjuncts);
            continue;
        }
        // A comparison on an `ENUM` column selects whole members
        // (`a > 'a'` → members `'b'`, `'c'`; `a > 1` → every member after the
        // first), so the ranger seeks one point range per surviving member
        // instead of an open interval over the stored labels.
        if let Some(members) = enum_member_points(&part.field_type, &bounds_set) {
            for member in members {
                let mut values: Vec<(sealdb_types::Datum, bool)> =
                    pinned.iter().cloned().map(|value| (value, true)).collect();
                values.push((member, true));
                ranges.push(pseudo::KeyRange {
                    low: values.clone(),
                    high: values,
                    low_open: false,
                    high_open: false,
                });
            }
            consumed.extend(part_conjuncts);
            break;
        }
        // The range stops at this part: every remaining bound becomes a range
        // on the pinned prefix (`(1 1,1 +inf]` for `c2 = 1 and c1 > 1`).
        for (low, high) in &bounds_set {
            let mut low_vals: Vec<(sealdb_types::Datum, bool)> =
                pinned.iter().cloned().map(|value| (value, true)).collect();
            let mut high_vals = low_vals.clone();
            let low_open = match low {
                Some((value, inclusive)) => {
                    low_vals.push((value.clone(), *inclusive));
                    false
                }
                None => true,
            };
            let high_open = match high {
                Some((value, inclusive)) => {
                    high_vals.push((value.clone(), *inclusive));
                    false
                }
                None => true,
            };
            ranges.push(pseudo::KeyRange {
                low: low_vals,
                high: high_vals,
                low_open,
                high_open,
            });
        }
        consumed.extend(part_conjuncts);
        break;
    }
    if ranges.is_empty() {
        // Every predicate pinned an equality prefix and no part past it is
        // constrained: the range covers the whole pinned prefix (`[1,1]`).
        if pinned.is_empty() {
            return None;
        }
        let values: Vec<(sealdb_types::Datum, bool)> =
            pinned.iter().cloned().map(|value| (value, true)).collect();
        ranges.push(pseudo::KeyRange {
            low: values.clone(),
            high: values,
            low_open: false,
            high_open: false,
        });
    }
    // `col IN (2, 2)` repeats a range; a seek over it must not emit the same
    // row twice (TiDB merges equal point ranges).
    let mut unique: Vec<pseudo::KeyRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if !unique.contains(&range) {
            unique.push(range);
        }
    }
    Some(KeyRangeAccess {
        ranges: unique,
        consumed,
    })
}

/// Statistics snapshot of a table, as TIckDB's DataSource carries it.
pub type TableStats = sealdb_statistics::table::Table;

/// Build the index prefix access range of `index` from `conjuncts`.
pub fn index_access_range(
    info: &TableInfo,
    index: &IndexInfo,
    conjuncts: &[ExprNode],
    table_rows: f64,
    table_stats: Option<&TableStats>,
) -> Option<IndexAccessRange> {
    let leading = index.columns.first()?;
    // An expression key part is seeked through its generating expression
    // (`tidb_shard(a) = 8` → `range:[8,8]`), not through the hidden column.
    let column = super::expr_index::part_key(info, leading);
    let hidden = super::expr_index::hidden_column(info, leading);
    let column_info = match hidden {
        Some(hidden) => hidden,
        None => info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&column))?,
    };
    // Statistics are keyed by the column behind the key part: the hidden
    // generated column for an expression part.
    let stats_column = column_info.name.o().to_ascii_lowercase();
    let key_access = index_key_ranges(info, index, conjuncts)?;
    let key_len = index_key_parts(info, index).map(|parts| parts.len())?;
    let key_ranges = key_access.ranges;
    // The leading-column bounds drive the histogram lookup and the index-merge
    // arms; the composite ranges drive the seek itself.
    let mut ranges: Vec<pseudo::Bounds> = Vec::new();
    let mut consumed = key_access.consumed;
    // Conjuncts that bounded the leading key. A `LIKE 'prefix%'` stays in the
    // cop filter (the range is not exact) but its selectivity is already part
    // of the range estimate, so it must not shrink the filter's row count.
    let mut bound_conjuncts: Vec<ExprNode> = Vec::new();
    for conjunct in conjuncts {
        // Prefer LIKE so prefix patterns stay residual (see index_key_ranges).
        let (name, bounds, encoded) =
            if let Some((name, low, high)) = pseudo::like_prefix_bound(conjunct) {
                (name, vec![(Some((low, true)), Some((high, false)))], false)
            } else if let Some((name, bounds)) = pseudo::conjunct_bound_sets(conjunct) {
                (name, bounds, true)
            } else {
                continue;
            };
        if !name.eq_ignore_ascii_case(&column) {
            continue;
        }
        // Ranger converts each bound to the index column's type before building
        // the key (`a < 5429` on a DECIMAL column seeks with a decimal key).
        let bounds: Vec<pseudo::Bounds> = bounds
            .into_iter()
            .map(|(low, high)| {
                (
                    coerce_bound(&column_info.field_type, low),
                    coerce_bound(&column_info.field_type, high),
                )
            })
            .collect();
        // Ranger converts each bound to the column charset; a binary literal
        // that is not valid UTF-8 must surface the conversion error from the
        // cop Selection instead of being seeked as raw bytes.
        if !bounds
            .iter()
            .all(|(low, high)| bounds_fit_column(&column_info.field_type, low, high))
        {
            return None;
        }
        // A `<>` conjunct splits into two disjoint ranges; an intersection with
        // an existing range set keeps the empty result (TiDB drops the scan).
        if ranges.is_empty() {
            ranges = bounds;
        } else {
            pseudo::intersect_sets(&mut ranges, &bounds);
            if ranges.is_empty() {
                return None;
            }
        }
        bound_conjuncts.push(conjunct.clone());
        if encoded
            && !consumed
                .iter()
                .any(|used| used.restore_string() == conjunct.restore_string())
        {
            consumed.push(conjunct.clone());
        }
    }
    // A DNF of point arms pins the leading column once per arm; the histogram
    // lookup sums those points.
    if ranges.is_empty() && key_ranges.len() > 1 {
        ranges = key_ranges
            .iter()
            .filter_map(|range| {
                let low = range.low.first()?.clone();
                let high = range.high.first()?.clone();
                Some((Some(low), Some(high)))
            })
            .collect();
    }
    let rows = pseudo::index_key_ranges_rows(table_rows, &key_ranges, key_len, index.unique);
    // TiDB sizes the scan from the index statistics (`IndexStats.lessRowCount`
    // on the encoded keys); `stats:pseudo` disappears once the histogram exists.
    let stat_ranges: Vec<sealdb_statistics::range_est::StatsRange> = ranges
        .iter()
        .map(|(low, high)| sealdb_statistics::range_est::StatsRange {
            low: low.clone(),
            high: high.clone(),
        })
        .collect();
    let (rows, pseudo_scan) = match table_stats.and_then(|stats| {
        stats
            .index_range_estimate(index.name.o(), &column_info.field_type, &stat_ranges)
            .map(|rows| rows.max(0.0))
    }) {
        Some(rows) => (rows, false),
        // ANALYZE fills column histograms only, so a plain index range falls
        // back to its key column's histogram (TiDB's index stats mirror it).
        None => match table_stats.and_then(|stats| {
            stats.column_range_estimate(
                &stats_column,
                sealdb_statistics::kind_of(&column_info.field_type),
                &stat_ranges,
            )
        }) {
            Some(rows) => (rows, false),
            None => (rows, true),
        },
    };
    // TiDB `stats:partial[…]`: the index histogram is present but the column
    // statistics of the key column is not, so EXPLAIN marks the scan. A hidden
    // generated column behind an expression key part is reported through the
    // index statistics instead, never as a missing column.
    let partial_columns = table_stats.and_then(|stats| {
        if hidden.is_some() || stats.column(&stats_column).is_some() {
            return None;
        }
        Some(column.to_ascii_lowercase())
    });
    // Residual conjuncts (columns beyond the leading one) keep TiDB's
    // per-column selectivity on top of the range cardinality.
    // Both the encoded conjuncts the ranger consumed (a DNF of point arms is
    // consumed by `index_key_ranges`, not by the leading-column loop) and the
    // `LIKE 'prefix%'` bounds already feed the range estimate.
    let residual: Vec<ExprNode> = conjuncts
        .iter()
        .filter(|conjunct| {
            !consumed
                .iter()
                .chain(bound_conjuncts.iter())
                .any(|used| used.restore_string() == conjunct.restore_string())
        })
        .cloned()
        .collect();
    let filter_rows = if residual.is_empty() {
        rows
    } else {
        pseudo::filter_rows(&residual, rows)
    };
    Some(IndexAccessRange {
        range: IndexScanRange {
            text: pseudo::key_ranges_text(&key_ranges),
            rows,
            filter_rows,
            pseudo: pseudo_scan,
            partial: partial_columns,
            keep_order: false,
        },
        consumed,
        ranges,
        key_ranges,
    })
}

/// Convert one range bound to the index column's type (TiDB `ranger` keeps a
/// typed key so the encoded bound can be compared with the histogram's keys).
/// `ENUM` member datum for a 1-based ordinal (`a = 2` on `enum('a','b')`).
fn enum_datum_for_number(field_type: &sealdb_types::FieldType, number: i64) -> Option<Datum> {
    if number <= 0 {
        return None;
    }
    let label = field_type.elems.as_ref()?.get(number as usize - 1)?;
    Some(Datum::Enum(EnumValue::new(
        label.as_bytes().to_vec(),
        number as u64,
    )))
}

/// Point values of the `ENUM` members a bound set selects.
///
/// ENUM index keys hold member *labels*, so ordinal ordering can disagree with
/// the key order (`enum('c','b','a')` sorts `c < b < a` by ordinal but
/// `a < b < c` by bytes). Label comparisons (`a > 'a'`) and ordinal ones on a
/// non-monotonic ENUM (`a > 1`) therefore enumerate the selected members and
/// seek one point range per member instead of relying on an interval. A
/// monotonic ENUM keeps the interval shape (`("a" "a","a" +inf]`), because
/// there the two orderings agree.
fn enum_member_points(
    field_type: &sealdb_types::FieldType,
    bounds_set: &[pseudo::Bounds],
) -> Option<Vec<Datum>> {
    if field_type.tp != sealdb_types::field_type::TYPE_ENUM {
        return None;
    }
    let elems = field_type.elems.as_ref()?;
    // Ordinal bounds (`a > 1`) on an ENUM whose labels ascend with the member
    // index are equivalent to an open interval over the labels, and that is the
    // shape TiDB's ranger produces (`("a" "a","a" +inf]`). Only non-monotonic
    // labels (`enum('c','b','a')`) need explicit point seeks.
    let has_label_bound = bounds_set
        .iter()
        .any(|(low, high)| bounds_have_label(low, high));
    let monotonic = elems
        .windows(2)
        .all(|pair| pair[0].as_bytes() < pair[1].as_bytes());
    if !has_label_bound && monotonic {
        return None;
    }
    let mut points: Vec<Datum> = Vec::new();
    for (pos, label) in elems.iter().enumerate() {
        let member = Datum::Enum(EnumValue::new(label.as_bytes().to_vec(), pos as u64 + 1));
        let selected = bounds_set.iter().any(|(low, high)| {
            // Ordinal bounds (`e > 2`) were coerced to an ENUM datum and
            // compare by member index; label bounds stay strings and compare
            // by label, matching TiDB's `e > 'b'` semantics.
            let above = match low {
                Some((value, inclusive)) => match compare_member(&member, value) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => *inclusive,
                    std::cmp::Ordering::Less => false,
                },
                None => true,
            };
            let below = match high {
                Some((value, inclusive)) => match compare_member(&member, value) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => *inclusive,
                    std::cmp::Ordering::Greater => false,
                },
                None => true,
            };
            above && below
        });
        if selected && !points.contains(&member) {
            points.push(member);
        }
    }
    (!points.is_empty()).then_some(points)
}

/// Does either side of a bound come from a string label (`e > 'b'`) rather
/// than an ENUM ordinal (`e > 2`)?
fn bounds_have_label(low: &Option<(Datum, bool)>, high: &Option<(Datum, bool)>) -> bool {
    let is_label = |bound: &Option<(Datum, bool)>| matches!(bound, Some((Datum::Bytes(_), _)));
    is_label(low) || is_label(high)
}

/// Compare an ENUM member with an index bound. An ENUM bound is an ordinal
/// bound (`e > 2`); anything else compares as a string label (`e > 'b'`).
fn compare_member(member: &Datum, bound: &Datum) -> std::cmp::Ordering {
    match (member, bound) {
        (Datum::Enum(a), Datum::Enum(b)) => a.number.cmp(&b.number),
        _ => member.compare(bound),
    }
}

fn coerce_bound(
    field_type: &sealdb_types::FieldType,
    bound: Option<(sealdb_types::Datum, bool)>,
) -> Option<(sealdb_types::Datum, bool)> {
    let (value, inclusive) = bound?;
    let value = match field_type.tp {
        // An `ENUM` key part stores the member label, so a numeric bound is
        // the member's ordinal (`a > 1` seeks past the first member).
        sealdb_types::field_type::TYPE_ENUM => match &value {
            sealdb_types::Datum::Enum(_) | sealdb_types::Datum::Bytes(_) => value,
            other => enum_datum_for_number(field_type, other.as_i64().ok()?)?,
        },
        // A REAL key part is a float: `e = 3` seeks the encoded `3.0`, so an
        // integer (or decimal) literal has to take the column's type first or
        // the encoded bounds never match the stored keys.
        sealdb_types::TYPE_FLOAT => match &value {
            sealdb_types::Datum::F64(_) => value,
            other => sealdb_types::Datum::F64(f64::from(other.as_f64().ok()? as f32)),
        },
        sealdb_types::TYPE_DOUBLE => match &value {
            sealdb_types::Datum::F64(_) => value,
            other => sealdb_types::Datum::F64(other.as_f64().ok()?),
        },
        sealdb_types::TYPE_DECIMAL | sealdb_types::TYPE_NEW_DECIMAL => match &value {
            sealdb_types::Datum::Decimal(_) => value,
            sealdb_types::Datum::I64(v) => {
                sealdb_types::Datum::Decimal(sealdb_types::MyDecimal::from_i64(*v))
            }
            sealdb_types::Datum::U64(v) => {
                sealdb_types::Datum::Decimal(sealdb_types::MyDecimal::parse(&v.to_string()).ok()?)
            }
            sealdb_types::Datum::F64(v) => {
                sealdb_types::Datum::Decimal(sealdb_types::MyDecimal::parse(&format!("{v}")).ok()?)
            }
            sealdb_types::Datum::Bytes(b) | sealdb_types::Datum::Bit(b) => {
                sealdb_types::Datum::Decimal(
                    sealdb_types::MyDecimal::parse(std::str::from_utf8(b).ok()?.trim()).ok()?,
                )
            }
            // NULL bound on a NOT NULL column: keep the datum as-is.
            _ => value,
        },
        // Integer key parts: `x'd6'` is the number 214 in a numeric context.
        sealdb_types::TYPE_TINY
        | sealdb_types::TYPE_SHORT
        | sealdb_types::TYPE_INT24
        | sealdb_types::TYPE_LONG
        | sealdb_types::TYPE_LONG_LONG
        | sealdb_types::TYPE_YEAR => match &value {
            sealdb_types::Datum::Bit(bytes) => sealdb_types::Datum::U64(
                bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
            ),
            _ => value,
        },
        _ => value,
    };
    Some((value, inclusive))
}

/// Can both bounds be converted to `field_type` like TiDB `ranger` does?
fn bounds_fit_column(
    field_type: &sealdb_types::FieldType,
    low: &Option<(sealdb_types::Datum, bool)>,
    high: &Option<(sealdb_types::Datum, bool)>,
) -> bool {
    let fits = |bound: &Option<(sealdb_types::Datum, bool)>| match bound {
        // A hex/bit literal is a binary string: ranger converts it to the key
        // type, and a value wider than a BIT column fails that conversion
        // (`col1 = 0x30` on `bit(1)`), so the predicate stays in the cop
        // Selection instead of becoming a seek key.
        Some((sealdb_types::Datum::Bit(bytes), _))
            if field_type.tp == sealdb_types::field_type::TYPE_BIT =>
        {
            let width = field_type.flen.max(1) as u32;
            let value = bytes
                .iter()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
            width >= 64 || value >> width == 0
        }
        // In a numeric context a hex/bit literal is the number it spells
        // (`x'd6'` on `bigint` seeks 214), so it always converts.
        Some((sealdb_types::Datum::Bit(_), _))
            if matches!(
                field_type.eval_type(),
                sealdb_types::field_type::EvalType::Int
            ) =>
        {
            true
        }
        // Binary values: string literals, and hex/bit literals (which arrive as
        // `Datum::Bit`) — both need a charset conversion to the column.
        Some((sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes), _)) => {
            field_type.charset == "binary" || std::str::from_utf8(bytes).is_ok()
        }
        _ => true,
    };
    fits(low) && fits(high)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_meta::{ColumnInfo, IndexColumn};
    use sealdb_parser::Parser;
    use sealdb_types::FieldType;

    fn int_column(name: &str, offset: i32) -> ColumnInfo {
        ColumnInfo::new(offset as i64 + 1, offset, name, FieldType::new_longlong())
    }

    fn table_info(
        columns: Vec<ColumnInfo>,
        index_columns: &[&str],
        unique: bool,
    ) -> (TableInfo, IndexInfo) {
        let mut info = TableInfo::new(1, "t", columns);
        let cols: Vec<IndexColumn> = index_columns
            .iter()
            .enumerate()
            .map(|(offset, name)| IndexColumn::new(*name, offset as i32))
            .collect();
        let mut index = IndexInfo::new(1, "idx", "t", cols);
        index.unique = unique;
        info.indices = vec![index.clone()];
        (info, index)
    }

    fn selection(sql: &str) -> Vec<ExprNode> {
        let stmt = Parser::new()
            .parse_one_stmt(&format!("SELECT * FROM t WHERE {sql}"))
            .expect("parse");
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!("not a select");
        };
        select.selection.expect("selection").split_cnf()
    }

    #[test]
    fn test_less_than_becomes_range() {
        let (info, index) = table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        let access =
            index_access_range(&info, &index, &selection("a < 3"), 10_000.0, None).expect("range");
        assert_eq!(access.range.text, "[-inf,3)");
        assert!((access.range.rows - (10_000.0 / 3.0 - 10.0)).abs() < 1e-6);
        assert_eq!(access.consumed.len(), 1);
    }

    #[test]
    fn test_composite_prefix_only_consumes_leading_column() {
        let (info, index) = table_info(
            vec![int_column("a", 0), int_column("b", 1)],
            &["a", "b"],
            false,
        );
        let access =
            index_access_range(&info, &index, &selection("a < 3 and b < 3"), 10_000.0, None)
                .expect("range");
        assert_eq!(access.range.text, "[-inf,3)");
        assert_eq!(access.consumed.len(), 1);
        assert!((access.range.rows - (10_000.0 / 3.0 - 10.0)).abs() < 1e-6);
    }

    #[test]
    fn test_unrelated_column_has_no_range() {
        let (info, index) = table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        assert!(index_access_range(&info, &index, &selection("b < 3"), 10_000.0, None).is_none());
    }

    /// Table with an integer clustered PRIMARY KEY, like `explain_easy_stats.t1`.
    fn handle_table(columns: Vec<ColumnInfo>, index_columns: &[&str]) -> (TableInfo, IndexInfo) {
        let mut info = TableInfo::new(1, "t", columns);
        info.pk_is_handle = true;
        let pk_cols: Vec<IndexColumn> = vec![IndexColumn::new("c1", 0)];
        let mut primary = IndexInfo::new(1, "PRIMARY", "t", pk_cols);
        primary.primary = true;
        primary.unique = true;
        let cols: Vec<IndexColumn> = index_columns
            .iter()
            .enumerate()
            .map(|(offset, name)| IndexColumn::new(*name, offset as i32))
            .collect();
        let mut index = IndexInfo::new(2, "c2", "t", cols);
        index.unique = false;
        info.indices = vec![primary, index.clone()];
        (info, index)
    }

    #[test]
    fn test_equality_prefix_pins_the_row_handle() {
        // `c1 > 1 and c2 = 1` on `index c2(c2)`: the index key is `(c2, c1)`,
        // so the equality pins `c2` and the range continues on the handle.
        let (info, index) = handle_table(
            vec![
                int_column("c1", 0),
                int_column("c2", 1),
                int_column("c3", 2),
            ],
            &["c2"],
        );
        let access = index_access_range(
            &info,
            &index,
            &selection("c1 > 1 and c2 = 1"),
            10_000.0,
            None,
        )
        .expect("range");
        assert_eq!(access.range.text, "(1 1,1 +inf]");
        assert_eq!(access.key_ranges.len(), 1);
        // Both predicates are encoded by the seek, so neither stays a filter.
        assert_eq!(access.consumed.len(), 2);
        assert_eq!(
            access
                .ranges
                .iter()
                .map(|(low, high)| pseudo::range_text(low.as_ref(), high.as_ref()))
                .collect::<Vec<_>>(),
            vec!["[1,1]".to_string()],
            "the leading-column bounds still drive the histogram lookup"
        );
    }

    #[test]
    fn test_columns_beyond_the_range_stay_filters() {
        let (info, index) = handle_table(
            vec![
                int_column("c1", 0),
                int_column("c2", 1),
                int_column("c3", 2),
            ],
            &["c2"],
        );
        let access = index_access_range(
            &info,
            &index,
            &selection("c1 > 1 and c2 = 1 and c3 < 1"),
            10_000.0,
            None,
        )
        .expect("range");
        assert_eq!(access.range.text, "(1 1,1 +inf]");
        assert_eq!(access.consumed.len(), 2);
    }

    #[test]
    fn test_duplicate_in_ranges_merge() {
        let (info, index) = table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        let access = index_access_range(&info, &index, &selection("a in (1, 1)"), 10_000.0, None)
            .expect("range");
        assert_eq!(access.range.text, "[1,1]");
    }
}

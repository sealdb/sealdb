//! Index access-range construction — the `util/ranger` subset SealDB needs for
//! a single leading index column.
//!
//! TiDB's detacher pulls every comparison on the index prefix out of the cop
//! Selection and hands the merged bounds to `IndexRangeScan`, which keeps only
//! rows inside `range:…`. SealDB already seeks with the same bounds at
//! execution time, so the planner only has to expose the range to EXPLAIN and
//! to the cardinality estimate.
use super::physical_plans::IndexScanRange;
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_statistics::pseudo;

pub struct IndexAccessRange {
    /// `range:…` text + estRows for EXPLAIN.
    pub range: IndexScanRange,
    /// Conjuncts fully encoded by the range (they leave the cop Selection).
    pub consumed: Vec<ExprNode>,
}

/// Statistics snapshot of a table, as TIckDB's DataSource carries it.
pub type TableStats = sealdb_statistics::table::Table;

/// Build the leading-column access range of `index` from `conjuncts`.
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
    let column_info = match super::expr_index::hidden_column(info, leading) {
        Some(hidden) => hidden,
        None => info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&column))?,
    };
    // TIMESTAMP index keys are stored in UTC, so session-local bounds cannot be
    // pushed as a seek; the executor keeps the full scan + Selection instead.
    if column_info.field_type.tp == sealdb_types::TYPE_TIMESTAMP {
        return None;
    }
    let mut ranges: Vec<pseudo::Bounds> = Vec::new();
    let mut consumed = Vec::new();
    for conjunct in conjuncts {
        let Some((name, bounds)) = pseudo::conjunct_bound_sets(conjunct) else {
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
        consumed.push(conjunct.clone());
    }
    if ranges.is_empty() {
        return None;
    }
    // `PrefixEqualLen`: the leading column is pinned only by `col = const`
    // bounds, which ranger folds to an inclusive point range.
    let pinned = ranges.len() == 1
        && matches!(
            (&ranges[0].0, &ranges[0].1),
            (Some((low, true)), Some((high, true))) if low.compare(high) == std::cmp::Ordering::Equal
        );
    let rows = pseudo::index_ranges_rows(
        table_rows,
        &ranges,
        index.columns.len(),
        usize::from(pinned),
        index.unique,
    );
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
        None => (rows, true),
    };
    // TiDB `stats:partial[…]`: the index histogram is present but the column
    // statistics of the key column is not, so EXPLAIN marks the scan.
    let partial_columns = table_stats.and_then(|stats| {
        if stats.column(&column).is_some() {
            return None;
        }
        Some(column.to_ascii_lowercase())
    });
    // Residual conjuncts (columns beyond the leading one) keep TiDB's
    // per-column selectivity on top of the range cardinality.
    let residual: Vec<ExprNode> = conjuncts
        .iter()
        .filter(|conjunct| {
            !consumed
                .iter()
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
            text: pseudo::ranges_text(&ranges),
            rows,
            filter_rows,
            pseudo: pseudo_scan,
            partial: partial_columns,
        },
        consumed,
    })
}

/// Convert one range bound to the index column's type (TiDB `ranger` keeps a
/// typed key so the encoded bound can be compared with the histogram's keys).
fn coerce_bound(
    field_type: &sealdb_types::FieldType,
    bound: Option<(sealdb_types::Datum, bool)>,
) -> Option<(sealdb_types::Datum, bool)> {
    let (value, inclusive) = bound?;
    let value = match field_type.tp {
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
            sealdb_types::Datum::Bytes(b) | sealdb_types::Datum::Bit(b) => sealdb_types::Datum::Decimal(
                sealdb_types::MyDecimal::parse(std::str::from_utf8(b).ok()?.trim()).ok()?,
            ),
            // NULL bound on a NOT NULL column: keep the datum as-is.
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
    use sealdb_types::FieldType;
    use sealdb_parser::Parser;

    fn int_column(name: &str, offset: i32) -> ColumnInfo {
        ColumnInfo::new(
            offset as i64 + 1,
            offset,
            name,
            FieldType::new_longlong(),
        )
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
        select
            .selection
            .expect("selection")
            .split_cnf()
    }

    #[test]
    fn test_less_than_becomes_range() {
        let (info, index) =
            table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
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
        let (info, index) =
            table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        assert!(index_access_range(&info, &index, &selection("b < 3"), 10_000.0, None).is_none());
    }
}

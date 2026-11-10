//! Pseudo (unanalyzed) cardinality estimation — port of TiDB
//! `planner/cardinality/pseudo.go`.
//!
//! TiDB falls back to these rates whenever a column has no histogram: a range
//! condition keeps `1/3` of the rows, two-sided ranges `1/40`, equality
//! `1/1000`, and anything unrecognized `0.8`. Index ranges reuse the column
//! estimate and shrink `1/100` per leading equal-prefix column.
use std::cmp::Ordering;

use sealdb_parser::ast::expressions::{ColumnRange, ExprNode};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

pub const PSEUDO_EQUAL_RATE: f64 = 1000.0;
pub const PSEUDO_LESS_RATE: f64 = 3.0;
pub const PSEUDO_BETWEEN_RATE: f64 = 40.0;
/// TiDB `selectionFactor`: selectivity assumed for an unknown predicate.
pub const SELECTION_FACTOR: f64 = 0.8;

/// Inclusive/exclusive bounds of one range, `None` meaning TiDB `MinNotNull`
/// (lower) or `MaxValue` (upper) — the `-inf` / `+inf` sentinels.
pub type Bounds = (Option<(Datum, bool)>, Option<(Datum, bool)>);

/// TiDB `getPseudoRowCountByColumnRanges` for a single column range.
pub fn column_range_rows(table_rows: f64, low: Option<&(Datum, bool)>, high: Option<&(Datum, bool)>) -> f64 {
    let mut rows = match (low, high) {
        // `(MinNotNull, MaxValue)` — every non-NULL row.
        (None, None) => table_rows - table_rows / PSEUDO_EQUAL_RATE,
        // `(MinNotNull, v…]` — the NULL rows are already excluded.
        (None, Some(_)) => table_rows / PSEUDO_LESS_RATE - table_rows / PSEUDO_EQUAL_RATE,
        // `[v…, MaxValue)`.
        (Some(_), None) => table_rows / PSEUDO_LESS_RATE,
        (Some((low, _)), Some((high, _))) => {
            if low.compare(high) == Ordering::Equal {
                table_rows / PSEUDO_EQUAL_RATE
            } else {
                table_rows / PSEUDO_BETWEEN_RATE
            }
        }
    };
    if rows < 0.0 {
        rows = 0.0;
    }
    rows.min(table_rows)
}

/// TiDB `getPseudoRowCountByIndexRanges` for one range over the index prefix.
/// `equal_prefix_len` is TiDB `Range.PrefixEqualLen`; a unique index fully
/// pinned by inclusive equalities is exactly one row, and every remaining
/// equal-prefix column divides the column estimate by 100.
pub fn index_range_rows(
    table_rows: f64,
    low: Option<&(Datum, bool)>,
    high: Option<&(Datum, bool)>,
    index_columns_len: usize,
    equal_prefix_len: usize,
    unique: bool,
) -> f64 {
    if table_rows == 0.0 {
        return 0.0;
    }
    let inclusive = !low.is_some_and(|(_, inc)| !*inc) && !high.is_some_and(|(_, inc)| !*inc);
    if unique && inclusive && equal_prefix_len >= index_columns_len {
        return 1.0;
    }
    let mut rows = column_range_rows(table_rows, low, high);
    // A unique index already pins the prefix to one value per equality; TiDB
    // does not add the extra 1/100 shrink for it (`range:[1,1]` on ab(a,b) is 10).
    // TiDB `getPseudoRowCountByIndexRanges` divides once per pinned prefix
    // column *before* the last one (`i = min(PrefixEqualLen, len(LowVal)-1)`),
    // so a point that pins only the leading column (`idx(a, c)` with `a = 1`,
    // `LowVal = [1]`) shrinks nothing and stays at the 1/1000 equal rate: 10.
    if !unique {
        for _ in 0..equal_prefix_len
            .saturating_sub(1)
            .min(index_columns_len.saturating_sub(1))
        {
            rows /= 100.0;
        }
    }
    if rows > table_rows {
        rows = table_rows / PSEUDO_LESS_RATE;
    }
    rows
}

/// Per-column ranges of a predicate list (TiDB `ranger`), where `<>` splits
/// into the two disjoint ranges `(-inf,v)` and `(v,+inf)`.
pub fn column_bound_sets(expr: &ExprNode) -> Vec<(String, Vec<Bounds>)> {
    let mut out: Vec<(String, Vec<Bounds>)> = Vec::new();
    for conjunct in expr.split_cnf() {
        let Some((column, sets)) = conjunct_bound_sets(&conjunct) else {
            continue;
        };
        match out
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&column))
        {
            Some((_, current)) => intersect_sets(current, &sets),
            None => out.push((column, sets)),
        }
    }
    out
}

/// Ranges one conjunct imposes on one column.
pub fn conjunct_bound_sets(expr: &ExprNode) -> Option<(String, Vec<Bounds>)> {
    // DNF of equalities on the same column (`a = 1 OR a = 2`) → one multi-range
    // set. TiDB ranger builds these as `["1","1"], ["2","2"]` on a single index.
    if let Some(sets) = dnf_eq_bound_sets(expr) {
        return Some(sets);
    }
    // `a IN (1, 2)` is a DNF of equalities: TiDB ranger expands it into one
    // point range per value, so two IN lists on the same column intersect.
    if let Some(sets) = in_list_bound_sets(expr) {
        return Some(sets);
    }
    if let Some((column, value)) = expr.eq_key_const() {
        let point = (Some((value.clone(), true)), Some((value, true)));
        return Some((column, vec![point]));
    }
    if let Some((column, value)) = expr.ne_column_const() {
        // TiDB ranger: `a <> v` → `(-inf,v), (v,+inf)`.
        let below = (None, Some((value.clone(), false)));
        let above = (Some((value, false)), None);
        return Some((column, vec![below, above]));
    }
    if let Some((column, op, value)) = expr.column_cmp_const() {
        if op == Op::Ne {
            let below = (None, Some((value.clone(), false)));
            let above = (Some((value, false)), None);
            return Some((column, vec![below, above]));
        }
        return expr
            .column_ranges()
            .into_iter()
            .find(|range| range.column.eq_ignore_ascii_case(&column))
            .map(|range| (column, vec![(range.low, range.high)]));
    }
    if let Some((column, _, _)) = expr.between_column_const() {
        return expr
            .column_ranges()
            .into_iter()
            .find(|range| range.column.eq_ignore_ascii_case(&column))
            .map(|range| (column, vec![(range.low, range.high)]));
    }
    None
}

/// `a = 1 OR a = 2 …` over one column: point range per DNF arm.
fn dnf_eq_bound_sets(expr: &ExprNode) -> Option<(String, Vec<Bounds>)> {
    let arms = expr.split_dnf();
    if arms.len() < 2 {
        return None;
    }
    let mut column: Option<String> = None;
    let mut ranges = Vec::with_capacity(arms.len());
    for arm in &arms {
        let (name, value) = arm.eq_key_const()?;
        match &column {
            Some(have) if !have.eq_ignore_ascii_case(&name) => return None,
            _ => column = Some(name),
        }
        ranges.push((Some((value.clone(), true)), Some((value, true))));
    }
    Some((column?, ranges))
}

/// `a IN (1, 2, …)` over one bare column with literal values: TiDB's ranger
/// detaches it into one point range per value (`["1","1"], ["2","2"]`), which
/// lets a second IN list on the same column intersect down to a subrange.
/// `NOT IN` and non-literal lists stay opaque expression filters.
fn in_list_bound_sets(expr: &ExprNode) -> Option<(String, Vec<Bounds>)> {
    let ExprNode::InList(in_list) = expr else {
        return None;
    };
    if in_list.not || in_list.list.is_empty() {
        return None;
    }
    let ExprNode::Column(column) = in_list.expr.as_ref() else {
        return None;
    };
    let mut ranges = Vec::with_capacity(in_list.list.len());
    for item in &in_list.list {
        let ExprNode::Value(value) = item else {
            return None;
        };
        ranges.push((
            Some((value.datum.clone(), true)),
            Some((value.datum.clone(), true)),
        ));
    }
    Some((column.name.clone(), ranges))
}

/// Intersect two range sets over the same column (empty ranges drop out).
pub fn intersect_sets(target: &mut Vec<Bounds>, other: &[Bounds]) {
    let mut merged = Vec::new();
    for a in target.iter() {
        for b in other.iter() {
            if let Some(range) = intersect_bounds(a, b) {
                merged.push(range);
            }
        }
    }
    *target = merged;
}

/// Intersection of two ranges; `None` when the result is empty.
pub fn intersect_bounds(a: &Bounds, b: &Bounds) -> Option<Bounds> {
    let low = tighter_low(a.0.clone(), b.0.clone());
    let high = tighter_high(a.1.clone(), b.1.clone());
    if let (Some((low_value, low_inc)), Some((high_value, high_inc))) = (&low, &high) {
        match low_value.compare(high_value) {
            Ordering::Greater => return None,
            Ordering::Equal if !(*low_inc && *high_inc) => return None,
            _ => {}
        }
    }
    Some((low, high))
}

/// TiDB `ranger.Range.String()` joined for a range set:
/// `[-inf,2013-11-01 00:00:00), (2013-11-01 00:00:00,+inf]`.
pub fn ranges_text(ranges: &[Bounds]) -> String {
    ranges
        .iter()
        .map(|(low, high)| range_text(low.as_ref(), high.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sum of TiDB `getPseudoRowCountByIndexRanges` over a range set.
pub fn index_ranges_rows(
    table_rows: f64,
    ranges: &[Bounds],
    index_columns_len: usize,
    equal_prefix_len: usize,
    unique: bool,
) -> f64 {
    let mut rows: f64 = 0.0;
    for (low, high) in ranges {
        let mut range_rows = index_range_rows(
            table_rows,
            low.as_ref(),
            high.as_ref(),
            index_columns_len,
            equal_prefix_len,
            unique,
        );
        if low.is_none() && high.is_some() {
            // Both open-ended sides already subtract the NULL share; the split
            // ranges of `<>` must not double count it.
            range_rows = range_rows.max(0.0);
        }
        rows += range_rows;
    }
    rows.min(table_rows)
}

/// TiDB `ranger.Range.String()` for a single-column range: `[-inf,3)`,
/// `[1,1]`, `(3,+inf]`.
pub fn range_text(low: Option<&(Datum, bool)>, high: Option<&(Datum, bool)>) -> String {
    let (low_text, low_bracket) = match low {
        Some((value, inclusive)) => (
            format_datum(value),
            if *inclusive { "[" } else { "(" },
        ),
        None => ("-inf".to_string(), "["),
    };
    let (high_text, high_bracket) = match high {
        Some((value, inclusive)) => (
            format_datum(value),
            if *inclusive { "]" } else { ")" },
        ),
        None => ("+inf".to_string(), "]"),
    };
    format!("{low_bracket}{low_text},{high_text}{high_bracket}")
}

/// TiDB `formatDatum` for range bounds: integers plain, strings quoted,
/// temporal values in their canonical text form.
pub fn format_datum(value: &Datum) -> String {
    match value {
        Datum::Bytes(bytes) => format!("\"{}\"", String::from_utf8_lossy(bytes)),
        other => other.display(),
    }
}

/// Bounds of a single `col <op> const` / `BETWEEN` conjunct.
pub fn conjunct_column_range(expr: &ExprNode) -> Option<(String, ColumnRange)> {
    // `col = const` ranger folds to an inclusive point range `[v, v]`. Only a
    // bare column gets column selectivity: `cast(col AS …) = const` is an opaque
    // expression filter (TiDB keeps the 0.8 factor).
    if let Some((column, value)) = expr.eq_column_const() {
        return Some((
            column.clone(),
            ColumnRange {
                column,
                low: Some((value.clone(), true)),
                high: Some((value, true)),
            },
        ));
    }
    if let Some((column, _, _)) = expr.column_cmp_const() {
        return expr
            .column_ranges()
            .into_iter()
            .find(|range| range.column.eq_ignore_ascii_case(&column))
            .map(|range| (column, range));
    }
    if let Some((column, _, _)) = expr.between_column_const() {
        return expr
            .column_ranges()
            .into_iter()
            .find(|range| range.column.eq_ignore_ascii_case(&column))
            .map(|range| (column, range));
    }
    None
}

/// Merge two bounds over the same column (TiDB intersects ranges).
pub fn merge_bounds(target: &mut Bounds, other: &Bounds) {
    target.0 = tighter_low(target.0.take(), other.0.clone());
    target.1 = tighter_high(target.1.take(), other.1.clone());
}

fn tighter_low(current: Option<(Datum, bool)>, other: Option<(Datum, bool)>) -> Option<(Datum, bool)> {
    match (current, other) {
        (None, other) => other,
        (current, None) => current,
        (Some((a, a_inc)), Some((b, b_inc))) => Some(match a.compare(&b) {
            Ordering::Greater => (a, a_inc),
            Ordering::Less => (b, b_inc),
            Ordering::Equal => (a, a_inc && b_inc),
        }),
    }
}

fn tighter_high(current: Option<(Datum, bool)>, other: Option<(Datum, bool)>) -> Option<(Datum, bool)> {
    match (current, other) {
        (None, other) => other,
        (current, None) => current,
        (Some((a, a_inc)), Some((b, b_inc))) => Some(match a.compare(&b) {
            Ordering::Less => (a, a_inc),
            Ordering::Greater => (b, b_inc),
            Ordering::Equal => (a, a_inc && b_inc),
        }),
    }
}

/// TiDB `selectivity` over a CNF list with pseudo stats: each column's merged
/// range set keeps `range_rows / table_rows`, unrecognized predicates `0.8`.
/// Conjuncts that describe the same column intersect (TiDB `MergeDNF`), so
/// `a IN (1,2) AND a IN (1,3)` estimates as the single remaining point range.
pub fn filter_rows(conjuncts: &[ExprNode], table_rows: f64) -> f64 {
    let mut per_column: Vec<(String, Vec<Bounds>)> = Vec::new();
    let mut total = 1.0f64;
    for conjunct in conjuncts {
        let Some((column, ranges)) = conjunct_bound_sets(conjunct) else {
            total *= SELECTION_FACTOR;
            continue;
        };
        match per_column
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&column))
        {
            Some((_, current)) => intersect_sets(current, &ranges),
            None => per_column.push((column, ranges)),
        }
    }
    for (_, ranges) in &per_column {
        let rows: f64 = ranges
            .iter()
            .map(|bounds| column_range_rows(table_rows, bounds.0.as_ref(), bounds.1.as_ref()))
            .sum();
        total *= rows.min(table_rows) / table_rows;
    }
    (table_rows * total).clamp(0.0, table_rows)
}

/// TiDB `cardinality.Selectivity` over an unanalyzed table's pushed-down
/// filters, as a selectivity instead of a row count.
///
/// Columns keep `range_rows / table_rows` (a row handle's point range is
/// exactly one row), `not(isnull(col))` is the `(MinNotNull, MaxValue)` range,
/// and a DNF arm combines its terms under the independence assumption
/// (`1 - Π(1 - sᵢ)`). Predicates with no range keep `selectionFactor`.
/// Returns `None` for a DNF arm it cannot model, so callers can fall back.
pub fn selectivity(conjuncts: &[ExprNode], table_rows: f64, handle: Option<&str>) -> Option<f64> {
    if table_rows <= 0.0 {
        return Some(1.0);
    }
    let mut per_column: Vec<(String, Vec<Bounds>)> = Vec::new();
    let mut dnf: Vec<ExprNode> = Vec::new();
    let mut total = 1.0f64;
    for conjunct in conjuncts {
        let (column, bounds) = if let Some(column) = not_isnull_column(conjunct) {
            // `not(isnull(col))` seeks `(MinNotNull, MaxValue)` on the column.
            (column, vec![(None, None)])
        } else if let Some((column, ranges)) = conjunct_bound_sets(conjunct) {
            (column, ranges)
        } else if matches!(conjunct, ExprNode::Binary(bin) if bin.op == Op::LogicOr) {
            dnf.push(conjunct.clone());
            continue;
        } else {
            total *= SELECTION_FACTOR;
            continue;
        };
        match per_column
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&column))
        {
            Some((_, current)) => intersect_sets(current, &bounds),
            None => per_column.push((column, bounds)),
        }
    }
    for (column, ranges) in &per_column {
        let is_handle = handle.is_some_and(|name| name.eq_ignore_ascii_case(column));
        let rows: f64 = ranges
            .iter()
            .map(|bounds| {
                if is_handle {
                    handle_range_rows(table_rows, bounds.0.as_ref(), bounds.1.as_ref())
                } else {
                    column_range_rows(table_rows, bounds.0.as_ref(), bounds.1.as_ref())
                }
            })
            .sum();
        total *= rows.min(table_rows) / table_rows;
    }
    for cond in &dnf {
        let items = cond.split_dnf();
        // TiDB leaves a single-arm DNF (or a failed flatten) to the default
        // selection factor.
        if items.len() <= 1 {
            total *= SELECTION_FACTOR;
            continue;
        }
        let mut dnf_selectivity = 0.0f64;
        for item in items {
            let arm = selectivity(&item.split_cnf(), table_rows, handle)?;
            dnf_selectivity = dnf_selectivity + arm - dnf_selectivity * arm;
        }
        total *= dnf_selectivity;
    }
    Some(total)
}

/// Column of a `not(isnull(col))` conjunct (either `UnaryNot(IsNull)` form).
fn not_isnull_column(expr: &ExprNode) -> Option<String> {
    let (is_null, negated) = match expr {
        ExprNode::IsNull(is_null) => (is_null, false),
        ExprNode::Unary(unary) if unary.op == Op::Not => {
            let ExprNode::IsNull(is_null) = unary.expr.as_ref() else {
                return None;
            };
            (is_null, true)
        }
        _ => return None,
    };
    if is_null.not == negated {
        return None;
    }
    let ExprNode::Column(column) = is_null.expr.as_ref() else {
        return None;
    };
    Some(column.name.clone())
}

/// TiDB `getPseudoRowCountBySignedIntRanges`: a row handle is unique, so an
/// equality is exactly one row instead of `table_rows / pseudoEqualRate`.
fn handle_range_rows(
    table_rows: f64,
    low: Option<&(Datum, bool)>,
    high: Option<&(Datum, bool)>,
) -> f64 {
    let rows = match (low, high) {
        (None, None) => table_rows,
        (None, Some(_)) | (Some(_), None) => table_rows / PSEUDO_LESS_RATE,
        (Some((low, _)), Some((high, _))) => {
            if low.compare(high) == Ordering::Equal {
                1.0
            } else {
                table_rows / PSEUDO_BETWEEN_RATE
            }
        }
    };
    rows.min(table_rows)
}

/// TiDB `cardinality.Selectivity` of the residual predicates over a one-row
/// point get on an unanalyzed table.
///
/// TiDB derives the `Selection`'s cardinality from the table's pseudo column
/// statistics, so a range predicate keeps its pseudo range rate of the single
/// row (`gt(c2, 1)` → `1/3`). Equalities are already satisfied by the point
/// get itself (the access path consumed the handle / unique key), so they keep
/// the pinned row count.
pub fn point_get_range_selectivity(conjuncts: &[ExprNode], handle: Option<&str>) -> Option<f64> {
    let table_rows = 10_000.0;
    let mut sel = 1.0;
    let mut consumed = false;
    for conjunct in conjuncts {
        let Some((column, range)) = conjunct_column_range(conjunct) else {
            continue;
        };
        let low = range.low.as_ref();
        let high = range.high.as_ref();
        if let (Some((low, _)), Some((high, _))) = (low, high)
            && low.compare(high) == Ordering::Equal
        {
            continue;
        }
        let rows = if handle.is_some_and(|name| name.eq_ignore_ascii_case(&column)) {
            handle_range_rows(table_rows, low, high)
        } else {
            column_range_rows(table_rows, low, high)
        };
        sel *= rows / table_rows;
        consumed = true;
    }
    consumed.then_some(sel)
}

/// `IS NULL` / `NOT IS NULL` conjuncts carry no `ColumnRange`; the leading
/// index column can still only consume comparisons, so they stay residual.
pub fn is_null_predicate(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::IsNull(_))
        || matches!(expr, ExprNode::Unary(u) if u.op == Op::Not && matches!(u.expr.as_ref(), ExprNode::IsNull(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::sqlparser_bridge::parse_expr;

    fn int(v: i64) -> Datum {
        Datum::I64(v)
    }

    #[test]
    fn test_less_than_narrows_by_three_and_drops_nulls() {
        // TiDB `a < 3` on 10000 pseudo rows: 10000/3 - 10000/1000.
        let rows = column_range_rows(10_000.0, None, Some(&(int(3), false)));
        assert!((rows - 3323.333_333_333_333).abs() < 1e-6, "{rows}");
    }

    #[test]
    fn test_point_range_uses_equal_rate() {
        let rows = column_range_rows(10_000.0, Some(&(int(1), true)), Some(&(int(1), true)));
        assert_eq!(rows, 10.0);
    }

    #[test]
    fn test_between_uses_between_rate() {
        let rows = column_range_rows(10_000.0, Some(&(int(1), true)), Some(&(int(3), true)));
        assert_eq!(rows, 250.0);
    }

    #[test]
    fn test_not_isnull_keeps_all_but_pseudo_nulls() {
        // TiDB `select * from t where c is not null` on 10000 pseudo rows: 9990.
        let conds = [parse_expr("c is not null").expect("parsed")];
        let sel = selectivity(&conds, 10_000.0, Some("a")).expect("modeled");
        assert!((sel - 0.999).abs() < 1e-9, "{sel}");
    }

    #[test]
    fn test_dnf_over_handle_and_column_is_independent() {
        // TiDB: `not(isnull(c)) and (a = 1 or b = 1)` with `a` the int handle
        // over 10000 pseudo rows → 0.999 * (1 - (1 - 1/10000)(1 - 1/1000)).
        let conds = [
            parse_expr("c is not null").expect("parsed"),
            parse_expr("a = 1 or b = 1").expect("parsed"),
        ];
        let sel = selectivity(&conds, 10_000.0, Some("a")).expect("modeled");
        // TiDB reports 10.988001 rows for this filter list.
        assert!((sel * 10_000.0 - 10.988_001).abs() < 1e-5, "{}", sel * 10_000.0);
    }

    #[test]
    fn test_index_range_text() {
        assert_eq!(
            range_text(None, Some(&(int(3), false))),
            "[-inf,3)"
        );
        assert_eq!(
            range_text(Some(&(int(3), false)), None),
            "(3,+inf]"
        );
        assert_eq!(
            range_text(Some(&(int(1), true)), Some(&(int(1), true))),
            "[1,1]"
        );
        assert_eq!(
            range_text(
                Some(&(Datum::Bytes(b"x".to_vec()), true)),
                Some(&(Datum::Bytes(b"x".to_vec()), true))
            ),
            "[\"x\",\"x\"]"
        );
    }

    #[test]
    fn test_index_range_rows_unique_point() {
        assert_eq!(
            index_range_rows(
                10_000.0,
                Some(&(int(7), true)),
                Some(&(int(7), true)),
                1,
                1,
                true
            ),
            1.0
        );
    }

    #[test]
    fn test_index_range_rows_single_column_equality_keeps_equal_rate() {
        // A single-column index has no left-most equal prefix to divide by, so
        // `a = 7` keeps TiDB's 1/1000 pseudo rate.
        assert_eq!(
            index_range_rows(
                10_000.0,
                Some(&(int(7), true)),
                Some(&(int(7), true)),
                1,
                1,
                false
            ),
            10.0
        );
    }

    #[test]
    fn test_index_range_rows_multi_column_prefix_point_keeps_equal_rate() {
        // `a = 7` on `idx(a, c)` builds the ranger range `[1,1]` whose LowVal
        // holds only the pinned leading column, so TiDB's shrink loop runs
        // `min(prefixEqualLen, len(LowVal)-1) = 0` times and the arm stays at
        // the 1/1000 equal rate (TiDB v8.5.8: 10.00, not 0.10).
        assert_eq!(
            index_range_rows(
                10_000.0,
                Some(&(int(7), true)),
                Some(&(int(7), true)),
                2,
                1,
                false
            ),
            10.0
        );
    }
}

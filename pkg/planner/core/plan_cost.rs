use sealdb_parser::ast::TableName;
use sealdb_statistics::{Handle, Table};

pub const SCAN_FACTOR: f64 = 1.0;
pub const NET_FACTOR: f64 = 1.5;
pub const CPU_FACTOR: f64 = 0.5;
pub const HASH_FACTOR: f64 = 1.0;
pub const SORT_FACTOR: f64 = 2.0;

/// TiDB `stats.Table.Pseudo`: statistics only replace pseudo cardinality when
/// the table has been analyzed (`version > 0`) *and* holds rows. An analyzed
/// empty table stays pseudo, so EXPLAIN keeps `10000.00 … stats:pseudo`.
pub fn has_real_stats(t: &Table) -> bool {
    t.version > 0 && t.count > 0
}

pub fn table_row_count(stats: Option<&Handle>, from: &TableName, current_db: &str) -> f64 {
    let (db, table) = from.qualified(current_db);
    // DML `add_modify` may create a zero-count stub; only ANALYZE (`version > 0`)
    // replaces TiDB pseudo cardinality.
    stats
        .and_then(|handle| handle.table(&db, &table))
        .filter(|t| has_real_stats(t))
        .map(|t| t.count as f64)
        .unwrap_or(Table::PSEUDO_ROW_COUNT as f64)
}

pub fn eq_row_count(
    stats: Option<&Handle>,
    from: &TableName,
    current_db: &str,
    column: &str,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    stats
        .and_then(|handle| handle.table(&db, &table))
        .filter(|t| has_real_stats(t))
        .map(|t| t.eq_row_count(column) as f64)
}

/// Equality selectivity using TopN when the constant is known.
pub fn eq_row_count_value(
    stats: Option<&Handle>,
    from: &TableName,
    current_db: &str,
    column: &str,
    value: &sealdb_types::Datum,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    stats
        .and_then(|handle| handle.table(&db, &table))
        .filter(|t| has_real_stats(t))
        .map(|t| t.eq_row_count_value(column, value) as f64)
}

pub fn table_scan_cost(rows: f64) -> f64 {
    rows * SCAN_FACTOR
}

pub fn index_lookup_cost(rows: f64) -> f64 {
    rows * (SCAN_FACTOR + NET_FACTOR)
}

pub fn point_get_cost() -> f64 {
    1.0
}

/// TiDB-style HashJoin cost: `left` probes, `right` builds.
/// Building is charged higher than probing so the cheaper plan builds the
/// smaller side.
pub fn hash_join_cost(probe_rows: f64, build_rows: f64, probe_cost: f64, build_cost: f64) -> f64 {
    probe_cost + build_cost + probe_rows * HASH_FACTOR + build_rows * HASH_FACTOR * 2.0 + CPU_FACTOR
}

/// Full-sort cost ~ N log N.
pub fn sort_cost(rows: f64) -> f64 {
    if rows <= 1.0 {
        return rows * CPU_FACTOR;
    }
    rows * rows.log2().max(1.0) * SORT_FACTOR * CPU_FACTOR
}

/// Limit/offset streaming cost.
pub fn limit_cost(rows: f64, limit: u64) -> f64 {
    rows.min(limit as f64) * CPU_FACTOR
}

/// TopN heap cost ~ N log K (prefer over Sort+Limit when K is small).
pub fn topn_cost(rows: f64, limit: u64) -> f64 {
    let k = (limit as f64).max(1.0);
    if rows <= 1.0 {
        return rows * CPU_FACTOR;
    }
    rows * k.log2().max(1.0) * SORT_FACTOR * CPU_FACTOR
}

/// True when TopN is cheaper than Sort followed by Limit.
pub fn prefer_topn(rows: f64, limit: u64) -> bool {
    topn_cost(rows, limit) <= sort_cost(rows) + limit_cost(rows, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_cost_prefers_cheaper_build() {
        let small = 1.0;
        let big = 100.0;
        let small_cost = table_scan_cost(small);
        let big_cost = table_scan_cost(big);
        // probe big, build small
        let build_small = hash_join_cost(big, small, big_cost, small_cost);
        // probe small, build big
        let build_big = hash_join_cost(small, big, small_cost, big_cost);
        assert!(build_small < build_big);
    }

    #[test]
    fn test_topn_beats_sort_for_small_limit() {
        assert!(prefer_topn(10_000.0, 5));
        assert!(!prefer_topn(10.0, 10_000));
    }
}

/// Exact selectivity for a partition-table pushed filter.
///
/// TiDB estimates these scans with the partition pruning "detached ranges"
/// (point counts for `= / IN`, column ranges for `BETWEEN` and comparisons).
/// `ANALYZE` keeps exact per-value counts for narrow columns, so the same
/// numbers are reproducible without a CM sketch.
pub fn partition_filter_rows(
    table: &Table,
    filter: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<f64> {
    use sealdb_parser::ast::expressions::ExprNode;
    if table.count == 0 {
        return Some(0.0);
    }
    let rows = table.count as f64;
    let est = match filter {
        ExprNode::Value(_) => const_i64(filter).map(|v| if v == 0 { 0.0 } else { rows })?,
        ExprNode::IsNull(i) => {
            let col = as_column(&i.expr)?;
            let stats = table.column(col)?;
            if i.not {
                rows - stats.null_count as f64
            } else {
                stats.null_count as f64
            }
        }
        ExprNode::InList(list) => in_list_rows(table, list)?,
        ExprNode::Binary(bin) => match bin.op {
            Op::LogicAnd => cnf_rows(table, filter)?,
            Op::LogicOr => disjunction_rows(table, filter)?,
            op if is_cmp(op) => comparison_rows(table, op, &bin.left, &bin.right)?,
            _ => return None,
        },
        _ => return None,
    };
    Some(est.max(1.0).min(rows))
}

use sealdb_parser::opcode::Op;

fn is_cmp(op: Op) -> bool {
    matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)
}

fn flip_cmp(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        other => other,
    }
}

fn const_i64(expr: &sealdb_parser::ast::expressions::ExprNode) -> Option<i64> {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_types::Datum;
    match expr {
        ExprNode::Value(v) => match v.datum {
            Datum::I64(n) => Some(n),
            Datum::U64(n) => i64::try_from(n).ok(),
            _ => None,
        },
        ExprNode::Unary(u) if u.op == Op::UnaryMinus => const_i64(&u.expr).map(|n| -n),
        _ => None,
    }
}

fn const_datum(expr: &sealdb_parser::ast::expressions::ExprNode) -> Option<sealdb_types::Datum> {
    use sealdb_parser::ast::expressions::ExprNode;
    const_i64(expr).map(sealdb_types::Datum::I64).or_else(|| match expr {
        ExprNode::Value(v) => Some(v.datum.clone()),
        _ => None,
    })
}

fn as_column(expr: &sealdb_parser::ast::expressions::ExprNode) -> Option<&str> {
    use sealdb_parser::ast::expressions::ExprNode;
    match expr {
        ExprNode::Column(c) => Some(c.name.as_str()),
        _ => None,
    }
}

/// `column <op> const` (either side), returning the comparison column.
fn cmp_parts<'a>(
    op: Op,
    left: &'a sealdb_parser::ast::expressions::ExprNode,
    right: &'a sealdb_parser::ast::expressions::ExprNode,
) -> Option<(&'a str, sealdb_types::Datum, Op)> {
    if let (Some(col), Some(datum)) = (as_column(left), const_datum(right)) {
        return Some((col, datum, op));
    }
    if let (Some(datum), Some(col)) = (const_datum(left), as_column(right)) {
        return Some((col, datum, flip_cmp(op)));
    }
    None
}

fn comparison_rows(
    table: &Table,
    op: Op,
    left: &sealdb_parser::ast::expressions::ExprNode,
    right: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<f64> {
    let (col, datum, op) = cmp_parts(op, left, right)?;
    let stats = table.column(col)?;
    stats.value_counts.as_ref()?;
    let rows = table.count as f64;
    let point = || stats.exact_point_count(&datum).map(|c| c as f64);
    match op {
        Op::Eq => point(),
        Op::Ne => Some(rows - point()? - stats.null_count as f64),
        Op::Lt => Some(
            (stats.exact_range_count(None, Some((&datum, false)))? as f64)
                .clamp(1.0, rows),
        ),
        Op::Le => Some(
            (stats.exact_range_count(None, Some((&datum, true)))? as f64).clamp(1.0, rows),
        ),
        Op::Gt => Some(
            (stats.exact_range_count(Some((&datum, false)), None)? as f64).clamp(1.0, rows),
        ),
        Op::Ge => Some(
            (stats.exact_range_count(Some((&datum, true)), None)? as f64).clamp(1.0, rows),
        ),
        _ => None,
    }
}

fn in_list_rows(
    table: &Table,
    list: &sealdb_parser::ast::expressions::InListExpr,
) -> Option<f64> {
    let col = as_column(&list.expr)?;
    let stats = table.column(col)?;
    stats.value_counts.as_ref()?;
    let values: Vec<sealdb_types::Datum> = list
        .list
        .iter()
        .map(const_datum)
        .collect::<Option<Vec<_>>>()?;
    let rows = table.count as f64;
    let matched = stats.exact_points_count(&values)? as f64;
    Some(if list.not {
        // `NOT IN` is UNKNOWN for NULL keys, so they never satisfy it.
        rows - matched - stats.null_count as f64
    } else {
        matched
    })
}

/// Conjunction: intersect ranges on one column and subtract excluded values.
fn cnf_rows(
    table: &Table,
    filter: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<f64> {
    use sealdb_parser::ast::expressions::ExprNode;
    let mut column: Option<String> = None;
    let mut lo: Option<(sealdb_types::Datum, bool)> = None;
    let mut hi: Option<(sealdb_types::Datum, bool)> = None;
    let mut eq: Option<sealdb_types::Datum> = None;
    let mut excluded: Vec<sealdb_types::Datum> = Vec::new();
    for conjunct in filter.split_cnf_all() {
        match conjunct {
            ExprNode::Value(_) => match const_i64(&conjunct)? {
                0 => return Some(0.0),
                _ => continue,
            },
            ExprNode::InList(list) => {
                let col = as_column(&list.expr)?;
                set_column(&mut column, col)?;
                if list.not {
                    for item in &list.list {
                        excluded.push(const_datum(item)?);
                    }
                } else {
                    return None;
                }
            }
            ExprNode::Binary(bin) if is_cmp(bin.op) => {
                let (col, datum, op) = cmp_parts(bin.op, &bin.left, &bin.right)?;
                set_column(&mut column, col)?;
                match op {
                    Op::Eq => eq = Some(datum),
                    Op::Ne => excluded.push(datum),
                    Op::Lt => tighten_hi(&mut hi, datum, false),
                    Op::Le => tighten_hi(&mut hi, datum, true),
                    Op::Gt => tighten_lo(&mut lo, datum, false),
                    Op::Ge => tighten_lo(&mut lo, datum, true),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    let stats = table.column(column.as_deref()?)?;
    stats.value_counts.as_ref()?;
    if let Some(value) = eq {
        return Some(stats.exact_point_count(&value)? as f64);
    }
    let base = if lo.is_none() && hi.is_none() {
        // A bare `col != x` CNF excludes NULL keys (UNKNOWN is not TRUE).
        if excluded.is_empty() {
            table.count as f64
        } else {
            table.count as f64 - stats.null_count as f64
        }
    } else {
        stats.exact_range_count(
            lo.as_ref().map(|(d, inc)| (d, *inc)),
            hi.as_ref().map(|(d, inc)| (d, *inc)),
        )? as f64
    };
    let removed: u64 = excluded
        .iter()
        .filter(|value| datum_in_range(value, &lo, &hi))
        .map(|value| stats.exact_point_count(value).unwrap_or(0))
        .sum();
    Some(base - removed as f64)
}

/// Conjunctive bounds intersect: keep the tightest upper / lower bound.
fn tighten_hi(
    cur: &mut Option<(sealdb_types::Datum, bool)>,
    datum: sealdb_types::Datum,
    inc: bool,
) {
    match cur {
        None => *cur = Some((datum, inc)),
        Some((existing, _existing_inc)) => match datum.compare(existing) {
            std::cmp::Ordering::Less => *cur = Some((datum, inc)),
            std::cmp::Ordering::Equal if !inc => *cur = Some((existing.clone(), false)),
            _ => {}
        },
    }
}

fn tighten_lo(
    cur: &mut Option<(sealdb_types::Datum, bool)>,
    datum: sealdb_types::Datum,
    inc: bool,
) {
    match cur {
        None => *cur = Some((datum, inc)),
        Some((existing, _existing_inc)) => match datum.compare(existing) {
            std::cmp::Ordering::Greater => *cur = Some((datum, inc)),
            std::cmp::Ordering::Equal if !inc => *cur = Some((existing.clone(), false)),
            _ => {}
        },
    }
}

fn datum_in_range(
    value: &sealdb_types::Datum,
    lo: &Option<(sealdb_types::Datum, bool)>,
    hi: &Option<(sealdb_types::Datum, bool)>,
) -> bool {
    use std::cmp::Ordering;
    if let Some((bound, inclusive)) = lo {
        let ord = value.compare(bound);
        if ord == Ordering::Less || (ord == Ordering::Equal && !inclusive) {
            return false;
        }
    }
    if let Some((bound, inclusive)) = hi {
        let ord = value.compare(bound);
        if ord == Ordering::Greater || (ord == Ordering::Equal && !inclusive) {
            return false;
        }
    }
    true
}

fn set_column(slot: &mut Option<String>, column: &str) -> Option<()> {
    match slot {
        Some(existing) if !existing.eq_ignore_ascii_case(column) => None,
        Some(_) => Some(()),
        None => {
            *slot = Some(column.to_string());
            Some(())
        }
    }
}

/// Disjunction: sum point estimates, or `rows - est(complement)` for ranges.
fn disjunction_rows(
    table: &Table,
    filter: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<f64> {
    use sealdb_parser::ast::expressions::ExprNode;

    fn flatten<'a>(expr: &'a ExprNode, out: &mut Vec<&'a ExprNode>) {
        match expr {
            ExprNode::Binary(bin) if bin.op == Op::LogicOr => {
                flatten(&bin.left, out);
                flatten(&bin.right, out);
            }
            other => out.push(other),
        }
    }

    let mut leaves = Vec::new();
    flatten(filter, &mut leaves);
    // TiDB brackets the OR with ranger: merge the leaf ranges into disjoint
    // intervals, then sum their row counts (`a = 2 OR a > 3` → 1 + 6).
    let mut column: Option<String> = None;
    let mut intervals: Vec<Interval> = Vec::new();
    for leaf in &leaves {
        intervals.extend(leaf_intervals(leaf, &mut column)?);
    }
    let Some(col) = column else {
        return None;
    };
    let stats = table.column(&col)?;
    stats.value_counts.as_ref()?;
    Some(
        merge_intervals(intervals)
            .iter()
            .map(|interval| {
                stats
                    .exact_range_count(
                        interval.lo.as_ref().map(|(d, inc)| (d, *inc)),
                        interval.hi.as_ref().map(|(d, inc)| (d, *inc)),
                    )
                    .map(|count| count as f64)
            })
            .collect::<Option<Vec<f64>>>()?
            .into_iter()
            .sum::<f64>(),
    )
}

/// A half-open/closed datum interval; `None` bounds are `-inf` / `+inf`.
#[derive(Clone)]
struct Interval {
    lo: Option<(sealdb_types::Datum, bool)>,
    hi: Option<(sealdb_types::Datum, bool)>,
}

fn full_interval() -> Interval {
    Interval { lo: None, hi: None }
}

fn point_interval(datum: sealdb_types::Datum) -> Interval {
    Interval {
        lo: Some((datum.clone(), true)),
        hi: Some((datum, true)),
    }
}

fn leaf_intervals(
    leaf: &sealdb_parser::ast::expressions::ExprNode,
    column: &mut Option<String>,
) -> Option<Vec<Interval>> {
    use sealdb_parser::ast::expressions::ExprNode;
    match leaf {
        ExprNode::Value(_) => Some(if const_i64(leaf)? == 0 {
            Vec::new()
        } else {
            vec![full_interval()]
        }),
        ExprNode::InList(list) if !list.not => {
            let col = as_column(&list.expr)?;
            set_column(column, col)?;
            list.list
                .iter()
                .map(|item| const_datum(item).map(point_interval))
                .collect()
        }
        ExprNode::Binary(bin) if is_cmp(bin.op) => {
            let (col, datum, op) = cmp_parts(bin.op, &bin.left, &bin.right)?;
            set_column(column, col)?;
            Some(vec![comparison_interval(op, datum)])
        }
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            let mut lo = None;
            let mut hi = None;
            let mut point = None;
            if !conjunction_bounds(&bin.left, column, &mut lo, &mut hi, &mut point)?
                || !conjunction_bounds(&bin.right, column, &mut lo, &mut hi, &mut point)?
            {
                return None;
            }
            if let Some(datum) = point {
                return Some(vec![point_interval(datum)]);
            }
            Some(vec![Interval { lo, hi }])
        }
        _ => None,
    }
}

fn comparison_interval(op: Op, datum: sealdb_types::Datum) -> Interval {
    match op {
        Op::Eq => point_interval(datum),
        Op::Ne => full_interval(),
        Op::Lt => Interval {
            lo: None,
            hi: Some((datum, false)),
        },
        Op::Le => Interval {
            lo: None,
            hi: Some((datum, true)),
        },
        Op::Gt => Interval {
            lo: Some((datum, false)),
            hi: None,
        },
        Op::Ge => Interval {
            lo: Some((datum, true)),
            hi: None,
        },
        _ => full_interval(),
    }
}

fn conjunction_bounds(
    expr: &sealdb_parser::ast::expressions::ExprNode,
    column: &mut Option<String>,
    lo: &mut Option<(sealdb_types::Datum, bool)>,
    hi: &mut Option<(sealdb_types::Datum, bool)>,
    point: &mut Option<sealdb_types::Datum>,
) -> Option<bool> {
    use sealdb_parser::ast::expressions::ExprNode;
    match expr {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            let left = conjunction_bounds(&bin.left, column, lo, hi, point)?;
            let right = conjunction_bounds(&bin.right, column, lo, hi, point)?;
            Some(left && right)
        }
        ExprNode::Binary(bin) if is_cmp(bin.op) => {
            let (col, datum, op) = cmp_parts(bin.op, &bin.left, &bin.right)?;
            set_column(column, col)?;
            match op {
                Op::Eq => *point = Some(datum),
                Op::Ne => {}
                Op::Lt => tighten_hi(hi, datum, false),
                Op::Le => tighten_hi(hi, datum, true),
                Op::Gt => tighten_lo(lo, datum, false),
                Op::Ge => tighten_lo(lo, datum, true),
                _ => return None,
            }
            Some(true)
        }
        _ => Some(false),
    }
}

/// Sort and coalesce overlapping intervals, mirroring TiDB's range union.
fn merge_intervals(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.sort_by(|a, b| match (&a.lo, &b.lo) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some((x, _)), Some((y, _))) => x.compare(y),
    });
    let mut merged: Vec<Interval> = Vec::new();
    for interval in intervals {
        match merged.last_mut() {
            Some(current) if intervals_overlap(current, &interval) => {
                current.hi = max_hi(current.hi.take(), interval.hi);
            }
            _ => merged.push(interval),
        }
    }
    merged
}

fn intervals_overlap(left: &Interval, right: &Interval) -> bool {
    let Some((hi, hi_inc)) = left.hi.as_ref() else {
        return true;
    };
    let Some((lo, lo_inc)) = right.lo.as_ref() else {
        return true;
    };
    match lo.compare(hi) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => *hi_inc && *lo_inc,
        std::cmp::Ordering::Greater => false,
    }
}

fn max_hi(
    left: Option<(sealdb_types::Datum, bool)>,
    right: Option<(sealdb_types::Datum, bool)>,
) -> Option<(sealdb_types::Datum, bool)> {
    match (left, right) {
        (None, _) | (_, None) => None,
        (Some((l, l_inc)), Some((r, r_inc))) => match l.compare(&r) {
            std::cmp::Ordering::Less => Some((r, r_inc)),
            std::cmp::Ordering::Greater => Some((l, l_inc)),
            std::cmp::Ordering::Equal => Some((l, l_inc || r_inc)),
        },
    }
}

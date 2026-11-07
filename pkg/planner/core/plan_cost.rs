use sealdb_parser::ast::TableName;
use sealdb_statistics::{Handle, Table};

pub const SCAN_FACTOR: f64 = 1.0;
pub const NET_FACTOR: f64 = 1.5;
pub const CPU_FACTOR: f64 = 0.5;
pub const HASH_FACTOR: f64 = 1.0;
pub const SORT_FACTOR: f64 = 2.0;

pub fn table_row_count(stats: Option<&Handle>, from: &TableName, current_db: &str) -> f64 {
    let (db, table) = from.qualified(current_db);
    // DML `add_modify` may create a zero-count stub; only ANALYZE (`version > 0`)
    // replaces TiDB pseudo cardinality.
    stats
        .and_then(|handle| handle.table(&db, &table))
        .filter(|t| t.version > 0)
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
        .filter(|t| t.version > 0)
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
        .filter(|t| t.version > 0)
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

fn negate_cmp(op: Op) -> Option<Op> {
    Some(match op {
        Op::Eq => Op::Ne,
        Op::Ne => Op::Eq,
        Op::Lt => Op::Ge,
        Op::Le => Op::Gt,
        Op::Gt => Op::Le,
        Op::Ge => Op::Lt,
        _ => return None,
    })
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
        Op::Ne => Some(rows - point()?),
        Op::Lt => Some(stats.exact_range_count(None, Some((const_i64_of(&datum)?, false)))? as f64),
        Op::Le => Some(stats.exact_range_count(None, Some((const_i64_of(&datum)?, true)))? as f64),
        Op::Gt => Some(stats.exact_range_count(Some((const_i64_of(&datum)?, false)), None)? as f64),
        Op::Ge => Some(stats.exact_range_count(Some((const_i64_of(&datum)?, true)), None)? as f64),
        _ => None,
    }
}

fn const_i64_of(datum: &sealdb_types::Datum) -> Option<i64> {
    use sealdb_types::Datum;
    match datum {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => i64::try_from(*v).ok(),
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
    Some(if list.not { rows - matched } else { matched })
}

/// Conjunction: intersect ranges on one column and subtract excluded values.
fn cnf_rows(
    table: &Table,
    filter: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<f64> {
    use sealdb_parser::ast::expressions::ExprNode;
    let mut column: Option<String> = None;
    let mut lo: Option<(i64, bool)> = None;
    let mut hi: Option<(i64, bool)> = None;
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
                    Op::Lt => hi = Some((const_i64_of(&datum)?, false)),
                    Op::Le => hi = Some((const_i64_of(&datum)?, true)),
                    Op::Gt => lo = Some((const_i64_of(&datum)?, false)),
                    Op::Ge => lo = Some((const_i64_of(&datum)?, true)),
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
        table.count as f64
    } else {
        stats.exact_range_count(lo, hi)? as f64
    };
    let removed: u64 = excluded
        .iter()
        .map(|value| stats.exact_point_count(value).unwrap_or(0))
        .sum();
    Some(base - removed as f64)
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
    let rows = table.count as f64;
    let mut points = 0.0f64;
    let mut all_points = true;
    for leaf in &leaves {
        match leaf {
            ExprNode::Value(_) => match const_i64(leaf)? {
                0 => {}
                _ => return Some(rows),
            },
            ExprNode::Binary(bin) if bin.op == Op::Eq => {
                points += comparison_rows(table, Op::Eq, &bin.left, &bin.right)?;
            }
            _ => all_points = false,
        }
    }
    if all_points {
        return Some(points);
    }
    // `a < x OR a > y` → rows - rows(a >= x AND a <= y).
    let mut complements = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        complements.push(negate_predicate(table, leaf)?);
    }
    let complement = sealdb_parser::ast::expressions::ExprNode::compose_cnf(complements)?;
    Some(rows - cnf_rows(table, &complement)?)
}

fn negate_predicate(
    table: &Table,
    expr: &sealdb_parser::ast::expressions::ExprNode,
) -> Option<sealdb_parser::ast::expressions::ExprNode> {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode};
    match expr {
        ExprNode::Binary(bin) if is_cmp(bin.op) => Some(ExprNode::Binary(BinaryOpExpr {
            op: negate_cmp(bin.op)?,
            left: bin.left.clone(),
            right: bin.right.clone(),
            single_cnf_item: false,
        })),
        ExprNode::Value(_) => match const_i64(expr)? {
            0 => Some(ExprNode::Value(
                sealdb_parser::ast::expressions::ValueExpr {
                    datum: sealdb_types::Datum::I64(1),
                },
            )),
            _ => Some(ExprNode::Value(
                sealdb_parser::ast::expressions::ValueExpr {
                    datum: sealdb_types::Datum::I64(0),
                },
            )),
        },
        _ => {
            let _ = table;
            None
        }
    }
}

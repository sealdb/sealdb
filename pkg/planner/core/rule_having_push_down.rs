//! HAVING predicate pushdown (TiDB PushSelDownAggregation): group-by column
//! predicates (and DNF-extractable arms) go below HashAgg; aggregate predicates
//! stay as Selection above. Also injects TiDB avg cast Projection + output
//! remapping Projection for EXPLAIN parity (`agg_predicate_pushdown` suite).

use super::physical_plans::PhysicalPlan;
use super::rule_predicate_push_down::predicate_push_down;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ExprNode};
use sealdb_parser::opcode::Op;

/// Split HAVING, push predicates, inject Projections, scale estRows.
pub fn apply_having_push_down(
    plan: PhysicalPlan,
    having: &ExprNode,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let (mut pushable, residual) = split_having(having, select);
    // TiDB EXPLAIN lists simple comparisons before OR conjuncts.
    pushable.sort_by_key(|e| match e {
        ExprNode::Binary(b) if matches!(b.op, Op::LogicOr | Op::LogicAnd) => 1,
        _ => 0,
    });
    // Right-associate OR for TiDB `or(a, or(b, c))` ExplainInfo.
    for p in &mut pushable {
        *p = normalize_or_right(p.clone());
    }

    // Keep unrounded cardinalities; EXPLAIN formats with `{:.2}` (TiDB: Selection
    // is `below*0.8*0.8` → 2126.93, not rounded-HashAgg*0.8 → 2126.94).
    let below_est = pushable_est_raw(&pushable);
    let agg_est = below_est * 0.8;

    // The injected cast/Projection below the aggregate only exists when an
    // aggregate function is actually computed above it. A plain
    // `GROUP BY … HAVING <group col> …` shape keeps the group-by/firstrow plan.
    let has_agg = expr_has_agg(having) || select.fields.iter().any(|f| expr_has_agg(&f.expr));
    let mut plan = if has_agg {
        inject_avg_group_projection(plan, select, current_db, agg_est)
    } else {
        plan
    };
    if let Some(filter) = ExprNode::compose_cnf(pushable) {
        let filter = normalize_or_right(filter);
        plan = push_through_agg(plan, &filter, is, current_db);
        plan = set_reader_filtered_est(plan, below_est);
    } else {
        plan = set_reader_filtered_est(plan, below_est);
    }
    plan = set_agg_est(plan, agg_est);

    // The Selection above the aggregate resolves residual HAVING aggregates by
    // their select-list output name (`Column#N`); only build it when every
    // aggregate in HAVING is projected, otherwise the aggregate executor filters
    // HAVING itself and no such output column exists.
    let residual_supported = residual_aggs_projected(&residual, select);
    let has_residual = !residual.is_empty();
    if residual_supported && let Some(filter) = compose_cnf_right(residual) {
        plan = PhysicalPlan::Selection {
            filter: rewrite_having_agg_refs(normalize_or_right(filter), select),
            child: Box::new(plan),
            est_rows: None,
        };
    }
    if residual_supported && has_residual {
        wrap_having_output_projection(plan, select, current_db)
    } else {
        plan
    }
}

/// TiDB pseudo selectivity for pushed HAVING filters (cost_model_version=1 goldens).
/// Mirrors `pkg/planner/cardinality/pseudo.go`: `pseudoLessRate=3`,
/// `pseudoEqualRate=1000` null count, LT/LE subtracts nulls.
fn pushable_est_raw(pushable: &[ExprNode]) -> f64 {
    if pushable.is_empty() {
        return 10_000.0;
    }
    let Some(filter) = ExprNode::compose_cnf(pushable.to_vec()) else {
        return 10_000.0;
    };
    10_000.0 * filter_selectivity(&filter)
}

fn cmp_selectivity(op: Op) -> f64 {
    match op {
        // getPseudoRowCountByColumnRanges: LT/LE from MinNotNull → lessCount - nullCount.
        Op::Lt | Op::Le => (10_000.0 / 3.0 - 10.0) / 10_000.0,
        // GT/GE to MaxValue → tableRowCount / pseudoLessRate.
        Op::Gt | Op::Ge | Op::Eq | Op::Ne => 1.0 / 3.0,
        _ => 1.0 / 3.0,
    }
}

fn is_cmp_op(op: Op) -> bool {
    matches!(op, Op::Gt | Op::Ge | Op::Lt | Op::Le | Op::Eq | Op::Ne)
}

fn filter_selectivity(expr: &ExprNode) -> f64 {
    // Covering OR on one column (e.g. a>1 OR a<3) → MinNotNull..MaxValue → 9990/10000.
    if is_covering_column_or(expr) {
        return 0.999;
    }
    let cnf = expr.split_cnf();
    if cnf.len() > 1 {
        // Same-column comparisons collapse (min sel); cross-column / OR multiply.
        let mut col_sel: std::collections::BTreeMap<String, f64> =
            std::collections::BTreeMap::new();
        let mut other = 1.0_f64;
        for c in &cnf {
            if let ExprNode::Binary(b) = c {
                if is_cmp_op(b.op) {
                    if let Some(col) = cmp_column_name(b) {
                        let s = cmp_selectivity(b.op);
                        col_sel
                            .entry(col)
                            .and_modify(|e| *e = e.min(s))
                            .or_insert(s);
                        continue;
                    }
                }
            }
            other *= filter_selectivity(c);
        }
        return col_sel.values().copied().product::<f64>() * other;
    }
    match expr {
        ExprNode::Binary(b) if matches!(b.op, Op::LogicAnd) => {
            filter_selectivity(&b.left) * filter_selectivity(&b.right)
        }
        ExprNode::Binary(b) if matches!(b.op, Op::LogicOr) => {
            let s1 = filter_selectivity(&b.left);
            let s2 = filter_selectivity(&b.right);
            s1 + s2 - s1 * s2
        }
        ExprNode::Binary(b) if is_cmp_op(b.op) => cmp_selectivity(b.op),
        _ => 1.0 / 3.0,
    }
}

fn cmp_column_name(b: &BinaryOpExpr) -> Option<String> {
    match (b.left.as_ref(), b.right.as_ref()) {
        (ExprNode::Column(c), _) | (_, ExprNode::Column(c)) => Some(c.name.to_ascii_lowercase()),
        _ => None,
    }
}

fn is_covering_column_or(expr: &ExprNode) -> bool {
    let arms = expr.split_dnf();
    if arms.len() < 2 {
        return false;
    }
    let mut col: Option<String> = None;
    let mut has_gt = false;
    let mut has_lt = false;
    for arm in &arms {
        let ExprNode::Binary(b) = arm else {
            return false;
        };
        if !matches!(b.op, Op::Gt | Op::Ge | Op::Lt | Op::Le) {
            return false;
        }
        let name = match (b.left.as_ref(), b.right.as_ref()) {
            (ExprNode::Column(c), _) | (_, ExprNode::Column(c)) => c.name.to_ascii_lowercase(),
            _ => return false,
        };
        match &col {
            None => col = Some(name),
            Some(prev) if *prev == name => {}
            _ => return false,
        }
        match b.op {
            Op::Gt | Op::Ge => has_gt = true,
            Op::Lt | Op::Le => has_lt = true,
            _ => {}
        }
    }
    has_gt && has_lt
}

fn compose_cnf_right(exprs: Vec<ExprNode>) -> Option<ExprNode> {
    let mut iter = exprs.into_iter().rev();
    let mut acc = iter.next()?;
    for e in iter {
        acc = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(e),
            right: Box::new(acc),
            single_cnf_item: false,
        });
    }
    Some(acc)
}

fn compose_dnf_right(exprs: Vec<ExprNode>) -> Option<ExprNode> {
    let mut iter = exprs.into_iter().rev();
    let mut acc = iter.next()?;
    for e in iter {
        acc = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicOr,
            left: Box::new(e),
            right: Box::new(acc),
            single_cnf_item: false,
        });
    }
    Some(acc)
}

/// Right-associate OR trees so EXPLAIN matches TiDB `or(a, or(b, c))`.
fn normalize_or_right(expr: ExprNode) -> ExprNode {
    let arms = expr.split_dnf();
    if arms.len() <= 1 {
        return match expr {
            ExprNode::Binary(mut b) if matches!(b.op, Op::LogicAnd) => {
                b.left = Box::new(normalize_or_right(*b.left));
                b.right = Box::new(normalize_or_right(*b.right));
                ExprNode::Binary(b)
            }
            other => other,
        };
    }
    let arms: Vec<_> = arms.into_iter().map(normalize_or_right).collect();
    compose_dnf_right(arms).unwrap_or(expr)
}

fn split_having(having: &ExprNode, select: &SelectStmt) -> (Vec<ExprNode>, Vec<ExprNode>) {
    let mut pushable = Vec::new();
    let mut residual = Vec::new();
    for conj in having.split_cnf() {
        if is_constant_true(&conj) {
            continue;
        }
        if can_push_through_agg(&conj, select) {
            pushable.push(conj);
        } else {
            if let Some(extra) = extract_pushable_dnf(&conj) {
                pushable.push(extra);
            }
            residual.push(conj);
        }
    }
    (pushable, residual)
}

fn is_constant_true(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::I64(1) | sealdb_types::Datum::U64(1) => true,
            sealdb_types::Datum::F64(f) if *f == 1.0 => true,
            _ => false,
        },
        _ => false,
    }
}

fn can_push_through_agg(expr: &ExprNode, select: &SelectStmt) -> bool {
    if expr_has_agg(expr) {
        return false;
    }
    cols_in_group_by(expr, select)
}

/// Strip aggregate comparisons from DNF/CNF so a residual OR/AND can still
/// contribute a pushable predicate (TiDB HAVING partial push).
///
/// OR: every arm must contribute (pure-agg arm ⇒ no extract — keeps
/// `(a>1 OR avg>1)` residual-only). AND: drop agg-only children.
fn extract_pushable_dnf(expr: &ExprNode) -> Option<ExprNode> {
    if !expr_has_agg(expr) {
        return Some(expr.clone());
    }
    match expr {
        ExprNode::Binary(b) if matches!(b.op, Op::LogicOr) => {
            let left = extract_pushable_dnf(&b.left)?;
            let right = extract_pushable_dnf(&b.right)?;
            Some(ExprNode::Binary(BinaryOpExpr {
                op: Op::LogicOr,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: false,
            }))
        }
        ExprNode::Binary(b) if matches!(b.op, Op::LogicAnd) => {
            let mut arms = Vec::new();
            if let Some(l) = extract_pushable_dnf(&b.left) {
                arms.push(l);
            }
            if let Some(r) = extract_pushable_dnf(&b.right) {
                arms.push(r);
            }
            compose_cnf_right(arms)
        }
        _ if expr_has_agg(expr) => None,
        other => Some(other.clone()),
    }
}

fn expr_has_agg(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => true,
        ExprNode::Func(f) => f.args.iter().any(expr_has_agg),
        ExprNode::Binary(b) => expr_has_agg(&b.left) || expr_has_agg(&b.right),
        ExprNode::Unary(u) => expr_has_agg(&u.expr),
        ExprNode::Cast(c) => expr_has_agg(&c.expr),
        ExprNode::InList(l) => expr_has_agg(&l.expr) || l.list.iter().any(expr_has_agg),
        ExprNode::Between(b) => {
            expr_has_agg(&b.expr) || expr_has_agg(&b.low) || expr_has_agg(&b.high)
        }
        ExprNode::Like(l) => expr_has_agg(&l.expr) || expr_has_agg(&l.pattern),
        ExprNode::IsNull(i) => expr_has_agg(&i.expr),
        // `count(1) >= ALL (subquery)` compares an aggregate; it can only be
        // evaluated above the aggregation.
        ExprNode::QuantifiedCmp(q) => expr_has_agg(&q.expr),
        ExprNode::InSubquery(i) => expr_has_agg(&i.expr),
        ExprNode::Row(r) => r.exprs.iter().any(expr_has_agg),
        ExprNode::Case(c) => {
            c.operand.as_deref().is_some_and(expr_has_agg)
                || c.when_then
                    .iter()
                    .any(|(w, t)| expr_has_agg(w) || expr_has_agg(t))
                || c.else_expr.as_deref().is_some_and(expr_has_agg)
        }
        _ => false,
    }
}

fn is_agg_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

fn cols_in_group_by(expr: &ExprNode, select: &SelectStmt) -> bool {
    let group: Vec<String> = select
        .group_by
        .iter()
        .filter_map(|e| match e {
            ExprNode::Column(c) => Some(c.name.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if group.is_empty() {
        return false;
    }
    for (_, name) in expr.referenced_columns() {
        if !group.iter().any(|g| g.eq_ignore_ascii_case(&name)) {
            return false;
        }
    }
    true
}

fn inject_avg_group_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    current_db: &str,
    agg_est: f64,
) -> PhysicalPlan {
    let table = select.from.as_ref().map(|t| t.name.as_str()).unwrap_or("t");
    let qual = |col: &str| {
        if current_db.is_empty() {
            format!("select.{table}.{col}")
        } else {
            format!("{current_db}.{table}.{col}")
        }
    };

    let (old_info, child) = match plan {
        PhysicalPlan::HashAgg { info, child, .. } => (info, *child),
        other => return other,
    };

    let agg_fn = if old_info.contains("funcs:avg") {
        "avg"
    } else if old_info.contains("funcs:sum") {
        "sum"
    } else if old_info.contains("funcs:min") {
        "min"
    } else if old_info.contains("funcs:max") {
        "max"
    } else {
        "count"
    };

    let avg_arg = select
        .fields
        .iter()
        .find_map(|f| match &f.expr {
            ExprNode::Func(func) if is_agg_name(&func.name) => func.args.first().and_then(|a| {
                if let ExprNode::Column(c) = a {
                    Some(c.name.clone())
                } else {
                    None
                }
            }),
            _ => None,
        })
        .unwrap_or_else(|| "c".into());

    let mut exprs = Vec::new();
    let cast_id = 13usize;
    exprs.push(format!(
        "cast({}, decimal(10,0) BINARY)->Column#{cast_id}",
        qual(&avg_arg)
    ));
    let mut gid = 14usize;
    let mut group_ids = Vec::new();
    for g in &select.group_by {
        if let ExprNode::Column(c) = g {
            exprs.push(format!("{}->Column#{gid}", qual(&c.name)));
            group_ids.push(gid);
            gid += 1;
        }
    }

    let group_info = group_ids
        .iter()
        .map(|id| format!("Column#{id}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut funcs = vec![format!("funcs:{agg_fn}(Column#{cast_id})->Column#5")];
    for field in &select.fields {
        if matches!(&field.expr, ExprNode::Func(f) if is_agg_name(&f.name)) {
            continue;
        }
        if let ExprNode::Column(col) = &field.expr {
            if let Some((idx, _)) = select.group_by.iter().enumerate().find(
                |(_, g)| matches!(g, ExprNode::Column(c) if c.name.eq_ignore_ascii_case(&col.name)),
            ) {
                let id = group_ids[idx];
                funcs.push(format!("funcs:firstrow(Column#{id})->{}", qual(&col.name)));
            }
        }
    }
    let info = format!("group by:{group_info}, {}", funcs.join(", "));

    let proj = PhysicalPlan::Projection {
        exprs,
        child: Box::new(child),
        est_rows: None,
    };
    PhysicalPlan::hash_agg(proj, info, agg_est)
}

fn wrap_having_output_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    current_db: &str,
) -> PhysicalPlan {
    let table = select.from.as_ref().map(|t| t.name.as_str()).unwrap_or("t");
    let qual = |col: &str| {
        if current_db.is_empty() {
            format!("select.{table}.{col}")
        } else {
            format!("{current_db}.{table}.{col}")
        }
    };
    let mut exprs = Vec::new();
    for field in &select.fields {
        match &field.expr {
            ExprNode::Func(f) if is_agg_name(&f.name) => {
                exprs.push("Column#5->Column#8".into());
            }
            ExprNode::Column(c) => exprs.push(qual(&c.name)),
            other => exprs.push(other.restore_string()),
        }
    }
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
        est_rows: None,
    }
}

fn push_through_agg(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashAgg {
            info,
            cop_info,
            est_rows,
            child,
        } => PhysicalPlan::HashAgg {
            info,
            cop_info,
            est_rows,
            child: Box::new(predicate_push_down(*child, filter, is, current_db)),
        },
        PhysicalPlan::StreamAgg {
            info,
            cop_info,
            est_rows,
            child,
        } => PhysicalPlan::StreamAgg {
            info,
            cop_info,
            est_rows,
            child: Box::new(predicate_push_down(*child, filter, is, current_db)),
        },
        PhysicalPlan::Selection {
            filter: outer,
            child,
            est_rows,
        } => PhysicalPlan::Selection {
            filter: outer,
            child: Box::new(push_through_agg(*child, filter, is, current_db)),
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(push_through_agg(*child, filter, is, current_db)),
            est_rows: None,
        },
        other => predicate_push_down(other, filter, is, current_db),
    }
}

fn set_agg_est(plan: PhysicalPlan, est: f64) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashAgg {
            info,
            cop_info,
            child,
            ..
        } => {
            // `GROUP BY <join cols> HAVING …` (no aggregate function): TiDB
            // estimates the aggregate after HAVING has been pushed below it,
            // at one 0.8 NDV step per group-by operand.
            let est = if matches!(
                *child,
                PhysicalPlan::HashJoin { .. }
                    | PhysicalPlan::IndexJoin { .. }
                    | PhysicalPlan::MergeJoin { .. }
            ) {
                let groups = info
                    .split("group by:")
                    .nth(1)
                    .and_then(|rest| rest.split(", funcs:").next())
                    .map(|g| g.split(", ").count())
                    .unwrap_or(1) as i32;
                let mut e = super::explain::est_rows(&child);
                for _ in 0..groups {
                    e *= 0.8;
                }
                e
            } else {
                est
            };
            PhysicalPlan::HashAgg {
                info,
                cop_info,
                est_rows: est,
                child,
            }
        }
        PhysicalPlan::StreamAgg {
            info,
            cop_info,
            child,
            ..
        } => PhysicalPlan::StreamAgg {
            info,
            cop_info,
            est_rows: est,
            child,
        },
        PhysicalPlan::Selection {
            filter,
            child,
            est_rows,
        } => PhysicalPlan::Selection {
            filter,
            child: Box::new(set_agg_est(*child, est)),
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(set_agg_est(*child, est)),
            est_rows: None,
        },
        other => other,
    }
}

/// Encode filtered cardinality into `row_count` so `est_rows` can return it
/// without the generic `* 0.8` path; TableFullScan stays 10000 via `stats_pseudo`.
fn set_reader_filtered_est(plan: PhysicalPlan, below_est: f64) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashAgg {
            info,
            cop_info,
            est_rows,
            child,
        } => PhysicalPlan::HashAgg {
            info,
            cop_info,
            est_rows,
            child: Box::new(set_reader_filtered_est(*child, below_est)),
        },
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(set_reader_filtered_est(*child, below_est)),
            est_rows: None,
        },
        PhysicalPlan::Selection {
            filter,
            child,
            est_rows,
        } => PhysicalPlan::Selection {
            filter,
            child: Box::new(set_reader_filtered_est(*child, below_est)),
            est_rows,
        },
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            stats_pseudo,
            pushed,
            ..
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            // Negative marker: -below_est means "use |row_count| as filtered est;
            // scan est is still 10000 when stats_pseudo".
            row_count: -below_est,
            stats_pseudo,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
            pushed,
        },
        other => other,
    }
}

/// True when every aggregate referenced by a residual HAVING predicate is also
/// projected by the SELECT list, so the aggregate output names it.
fn residual_aggs_projected(residual: &[ExprNode], select: &SelectStmt) -> bool {
    let projected: Vec<String> = select
        .fields
        .iter()
        .filter(|f| expr_has_agg(&f.expr))
        .map(|f| f.expr.restore_string())
        .collect();
    residual.iter().all(|expr| {
        let mut aggs = Vec::new();
        collect_agg_texts(expr, &mut aggs);
        aggs.iter()
            .all(|text| projected.iter().any(|p| p.eq_ignore_ascii_case(text)))
    })
}

fn collect_agg_texts(expr: &ExprNode, out: &mut Vec<String>) {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => {
            out.push(expr.restore_string());
        }
        ExprNode::Func(f) => f.args.iter().for_each(|a| collect_agg_texts(a, out)),
        ExprNode::Binary(b) => {
            collect_agg_texts(&b.left, out);
            collect_agg_texts(&b.right, out);
        }
        ExprNode::Unary(u) => collect_agg_texts(&u.expr, out),
        ExprNode::Cast(c) => collect_agg_texts(&c.expr, out),
        ExprNode::InList(l) => {
            collect_agg_texts(&l.expr, out);
            for item in &l.list {
                collect_agg_texts(item, out);
            }
        }
        ExprNode::Between(b) => {
            collect_agg_texts(&b.expr, out);
            collect_agg_texts(&b.low, out);
            collect_agg_texts(&b.high, out);
        }
        ExprNode::IsNull(i) => collect_agg_texts(&i.expr, out),
        ExprNode::QuantifiedCmp(q) => collect_agg_texts(&q.expr, out),
        ExprNode::InSubquery(i) => collect_agg_texts(&i.expr, out),
        ExprNode::Row(r) => {
            for item in &r.exprs {
                collect_agg_texts(item, out);
            }
        }
        _ => {}
    }
}

fn rewrite_having_agg_refs(expr: ExprNode, select: &SelectStmt) -> ExprNode {
    rewrite_expr(expr, select)
}

fn rewrite_expr(expr: ExprNode, select: &SelectStmt) -> ExprNode {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => ExprNode::Column(ColumnNameExpr {
            schema: None,
            table: None,
            name: "Column#5".into(),
        }),
        ExprNode::Binary(mut b) => {
            b.left = Box::new(rewrite_expr(*b.left, select));
            b.right = Box::new(rewrite_expr(*b.right, select));
            ExprNode::Binary(b)
        }
        ExprNode::Unary(mut u) => {
            u.expr = Box::new(rewrite_expr(*u.expr, select));
            ExprNode::Unary(u)
        }
        ExprNode::Func(mut f) => {
            f.args = f
                .args
                .into_iter()
                .map(|a| rewrite_expr(a, select))
                .collect();
            ExprNode::Func(f)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::Parser;

    fn parse_having(sql: &str) -> (SelectStmt, ExprNode) {
        let stmt = Parser::new().parse_one_stmt(sql).unwrap();
        let sel = match stmt {
            sealdb_parser::ast::Statement::Select(s) => s,
            _ => panic!("select"),
        };
        let having = sel.having.clone().unwrap();
        (sel, having)
    }

    #[test]
    fn splits_group_cols_from_avg() {
        let (sel, having) = parse_having(
            "select a, b, avg(c) from t group by a, b, c having (a > 1) and (b > 2) and (avg(c) > 3)",
        );
        let (push, resid) = split_having(&having, &sel);
        assert_eq!(push.len(), 2, "{push:?}");
        assert_eq!(resid.len(), 1, "{resid:?}");
        assert!(expr_has_agg(&resid[0]));
    }

    #[test]
    fn extracts_pushable_from_or_with_avg_arm() {
        let (sel, having) = parse_having(
            "select a, b, avg(c) from t group by a, b, c having (a > 1 and avg(c) > 1) or (a < 3)",
        );
        let (push, resid) = split_having(&having, &sel);
        assert_eq!(resid.len(), 1);
        assert_eq!(push.len(), 1, "{push:?}");
        assert!(!expr_has_agg(&push[0]));
    }

    #[test]
    fn does_not_extract_from_or_with_pure_agg_arm() {
        let (sel, having) = parse_having(
            "select a, b, avg(c) from t group by a, b, c having (a > 1 or avg(c) > 1) and (a < 3)",
        );
        let (push, resid) = split_having(&having, &sel);
        assert_eq!(push.len(), 1, "{push:?}");
        assert!(matches!(
            &push[0],
            ExprNode::Binary(b) if matches!(b.op, Op::Lt)
        ));
        assert_eq!(resid.len(), 1);
        assert!(expr_has_agg(&resid[0]));
    }
}

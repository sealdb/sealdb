/// F09 leftover: rewrite simple correlated EXISTS / IN / NOT EXISTS Apply
/// into HashJoin Semi/Anti (TiDB `rule_decorrelate.go`).
///
/// NOT IN stays Apply (null-aware). Scalar subqueries, ALL/ANY, derived
/// tables, and composite join keys stay Apply. F14 `semi-anti-rewrite` is
/// the later SemiJoinRewriter (semi → inner + agg), not this operator.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode};
use sealdb_parser::ast::{SelectStmt, TableName};
use sealdb_parser::opcode::Op;

/// Recursively replace Apply nodes that carry a decorrelatable EXISTS/IN
/// predicate with a left-deep HashJoin Semi or Anti.
pub fn rewrite_semi_join(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Apply { child, filter } => rewrite_apply(rewrite_semi_join(*child), filter),
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join(*child)),
            filter,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Sort { child } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::TopN { child } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Limit { child } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::HashAgg { child } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::StreamAgg { child } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Window { child } => PhysicalPlan::Window {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => PhysicalPlan::HashJoin {
            tp,
            left: Box::new(rewrite_semi_join(*left)),
            right: Box::new(rewrite_semi_join(*right)),
            left_table,
            right_table,
            on,
        },
        PhysicalPlan::Union {
            distinct,
            left,
            right,
        } => PhysicalPlan::Union {
            distinct,
            left: Box::new(rewrite_semi_join(*left)),
            right: Box::new(rewrite_semi_join(*right)),
        },
        PhysicalPlan::CTE {
            name,
            columns,
            seed,
        } => PhysicalPlan::CTE {
            name,
            columns,
            seed: Box::new(rewrite_semi_join(*seed)),
        },
        PhysicalPlan::Explain { format, child } => PhysicalPlan::Explain {
            format,
            child: Box::new(rewrite_semi_join(*child)),
        },
        other => other,
    }
}

struct SemiSpec {
    tp: JoinType,
    right_table: String,
    right: PhysicalPlan,
    on: ExprNode,
}

fn rewrite_apply(mut plan: PhysicalPlan, filter: ExprNode) -> PhysicalPlan {
    let mut leftover = Vec::new();
    for item in filter.split_cnf() {
        match try_semi_spec(&plan, &item) {
            Some(spec) => {
                let left_table = plan_table(&plan).unwrap_or_else(|| spec.right_table.clone());
                plan = PhysicalPlan::HashJoin {
                    tp: spec.tp,
                    left: Box::new(plan),
                    right: Box::new(spec.right),
                    left_table,
                    right_table: spec.right_table,
                    on: spec.on,
                };
            }
            None => leftover.push(item),
        }
    }
    wrap_leftover(plan, leftover)
}

fn wrap_leftover(plan: PhysicalPlan, leftover: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(leftover) else {
        return plan;
    };
    if filter.has_subquery() {
        PhysicalPlan::Apply {
            child: Box::new(plan),
            filter,
        }
    } else {
        PhysicalPlan::Selection {
            child: Box::new(plan),
            filter,
        }
    }
}

fn try_semi_spec(outer: &PhysicalPlan, expr: &ExprNode) -> Option<SemiSpec> {
    match expr {
        ExprNode::Exists(exists) => from_exists(exists.not, &exists.query),
        ExprNode::InSubquery(sub) if !sub.not => {
            from_in(&sub.expr, &sub.query, plan_table(outer).as_deref())
        }
        _ => None,
    }
}

/// Correlated `EXISTS` / `NOT EXISTS` with a single equality that mentions
/// both the inner table and some outer table.
fn from_exists(not: bool, query: &SelectStmt) -> Option<SemiSpec> {
    let inner = simple_inner(query)?;
    let selection = query.selection.as_ref()?;
    let (on, inner_filter) = split_semi_pred(selection, &inner.name)?;
    Some(build_spec(
        if not { JoinType::Anti } else { JoinType::Semi },
        inner.name.clone(),
        inner_filter,
        on,
    ))
}

/// Uncorrelated `outer_col IN (SELECT inner_col FROM t [WHERE inner-only])`.
fn from_in(expr: &ExprNode, query: &SelectStmt, outer_table: Option<&str>) -> Option<SemiSpec> {
    let inner = simple_inner(query)?;
    let inner_filter = match &query.selection {
        Some(sel) if only_inner(sel, &inner.name) => Some(sel.clone()),
        Some(_) => return None,
        None => None,
    };
    let inner_expr = qualify_table(query.fields.first()?.expr.clone(), &inner.name);
    let outer_expr = match outer_table {
        Some(table) => qualify_table(expr.clone(), table),
        None => expr.clone(),
    };
    if !matches!(inner_expr, ExprNode::Column(_)) || !matches!(outer_expr, ExprNode::Column(_)) {
        return None;
    }
    if !is_eq_across_tables(&outer_expr, &inner_expr, &inner.name) {
        return None;
    }
    Some(build_spec(
        JoinType::Semi,
        inner.name.clone(),
        inner_filter,
        eq_expr(outer_expr, inner_expr),
    ))
}

fn build_spec(
    tp: JoinType,
    right_table: String,
    inner_filter: Option<ExprNode>,
    on: ExprNode,
) -> SemiSpec {
    let mut right = PhysicalPlan::table_reader(right_table.clone(), Vec::new());
    if let Some(filter) = inner_filter {
        right = PhysicalPlan::Selection {
            child: Box::new(right),
            filter,
        };
    }
    SemiSpec {
        tp,
        right_table,
        right,
        on,
    }
}

/// Single-table subquery with no grouping, limit, CTE, or extra joins.
fn simple_inner(query: &SelectStmt) -> Option<&TableName> {
    if !query.joins.is_empty()
        || !query.group_by.is_empty()
        || query.having.is_some()
        || query.with.is_some()
        || query.limit.is_some()
        || query.offset.is_some()
    {
        return None;
    }
    let from = query.from.as_ref()?;
    if from.name.eq_ignore_ascii_case("dual") {
        return None;
    }
    Some(from)
}

/// Split a CNF WHERE into one cross-table equality (the join key) plus
/// optional inner-only filters pushed onto the right TableReader.
fn split_semi_pred(selection: &ExprNode, inner: &str) -> Option<(ExprNode, Option<ExprNode>)> {
    let mut on = None;
    let mut inner_preds = Vec::new();
    for item in selection.split_cnf() {
        if is_join_eq(&item, inner) {
            if on.is_some() {
                return None;
            }
            on = Some(item);
        } else if only_inner(&item, inner) {
            inner_preds.push(item);
        } else {
            return None;
        }
    }
    Some((on?, ExprNode::compose_cnf(inner_preds)))
}

fn is_join_eq(expr: &ExprNode, inner: &str) -> bool {
    let ExprNode::Binary(bin) = expr else {
        return false;
    };
    if bin.op != Op::Eq {
        return false;
    }
    is_eq_across_tables(&bin.left, &bin.right, inner)
}

fn is_eq_across_tables(left: &ExprNode, right: &ExprNode, inner: &str) -> bool {
    match (col_table(left), col_table(right)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(inner) != b.eq_ignore_ascii_case(inner),
        _ => false,
    }
}

fn col_table(expr: &ExprNode) -> Option<&str> {
    match expr {
        ExprNode::Column(col) => col.table.as_deref(),
        _ => None,
    }
}

fn only_inner(expr: &ExprNode, inner: &str) -> bool {
    !expr.has_subquery()
        && expr.referenced_columns().iter().all(|(table, _)| {
            table
                .as_deref()
                .is_none_or(|name| name.eq_ignore_ascii_case(inner))
        })
}

fn qualify_table(expr: ExprNode, table: &str) -> ExprNode {
    match expr {
        ExprNode::Column(mut col) if col.table.is_none() => {
            col.table = Some(table.to_string());
            ExprNode::Column(col)
        }
        other => other,
    }
}

fn eq_expr(left: ExprNode, right: ExprNode) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn plan_table(plan: &PhysicalPlan) -> Option<String> {
    match plan {
        PhysicalPlan::TableReader { table, .. }
        | PhysicalPlan::PointGet { table, .. }
        | PhysicalPlan::BatchPointGet { table, .. }
        | PhysicalPlan::IndexLookUpReader { table, .. }
        | PhysicalPlan::IndexReader { table, .. }
        | PhysicalPlan::HashJoin {
            left_table: table, ..
        } => Some(table.clone()),
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Apply { child, .. } => plan_table(child),
        _ => None,
    }
}

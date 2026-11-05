/// rewrite simple correlated EXISTS / IN / NOT EXISTS Apply
/// into HashJoin Semi/Anti (TiDB `rule_decorrelate.go`).
///
/// `semi-anti-rewrite` then wraps the Semi inner side with Distinct
/// on the join key (TiDB SemiJoinRewriter: semi → inner + agg). Anti stays
/// as HashJoin Anti. NOT IN stays Apply.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_parser::ast::expressions::{
    BinaryOpExpr, ColumnNameExpr, ExprNode, IsNullExpr, UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{SelectStmt, TableName};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

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
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join(*child)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join(*child)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join(*child)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join(*child)),
            info,
            est_rows,
        },
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join(*child)),
            info,
            cop_info,
            est_rows,
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
        PhysicalPlan::Union { op, left, right } => PhysicalPlan::Union {
            op,
            left: Box::new(rewrite_semi_join(*left)),
            right: Box::new(rewrite_semi_join(*right)),
        },
        PhysicalPlan::CTE {
            name,
            columns,
            seed,
            inline,
        } => PhysicalPlan::CTE {
            name,
            columns,
            seed: Box::new(rewrite_semi_join(*seed)),
            inline,
        },
        PhysicalPlan::Distinct { keys, child } => PhysicalPlan::Distinct {
            keys,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
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
    /// Null-reject filters pushed onto the outer (Probe) side.
    outer_filter: Option<ExprNode>,
}

fn rewrite_apply(mut plan: PhysicalPlan, filter: ExprNode) -> PhysicalPlan {
    let mut leftover = Vec::new();
    for item in filter.split_cnf() {
        match try_semi_spec(&plan, &item) {
            Some(spec) => {
                let left_table = plan_table(&plan).unwrap_or_else(|| spec.right_table.clone());
                if let Some(outer_filter) = spec.outer_filter {
                    plan = push_isnull_filter(plan, outer_filter);
                }
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
        ExprNode::Exists(exists) => exists
            .query
            .as_select()
            .and_then(|query| from_exists(exists.not, query)),
        ExprNode::InSubquery(sub) if !sub.not => sub
            .query
            .as_select()
            .and_then(|query| from_in(&sub.expr, query, plan_table(outer).as_deref())),
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
        None,
    ))
}

/// `outer_col IN (SELECT inner_col FROM t [WHERE …])`.
/// Correlated WHERE preds become `other cond`; ORDER BY is ignored (semi).
fn from_in(expr: &ExprNode, query: &SelectStmt, outer_table: Option<&str>) -> Option<SemiSpec> {
    let inner = simple_inner(query)?;
    // ORDER BY in an IN subquery is meaningless for semi-join and is dropped.
    let (inner_filter, other_conds) = split_in_selection(query.selection.as_ref(), &inner.name)?;
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
    let equal = eq_expr(outer_expr.clone(), inner_expr.clone());
    if other_conds.is_empty() {
        return Some(build_spec(
            JoinType::Semi,
            inner.name.clone(),
            inner_filter,
            equal,
            None,
        ));
    }
    // Correlated IN → semi join with equal + other cond (TiDB decorrelate).
    let outer = outer_table?;
    let mut cond_exprs = vec![equal.clone()];
    cond_exprs.extend(other_conds.iter().cloned());
    let right_nulls = not_isnull_cols(&cond_exprs, &inner.name);
    let left_nulls = not_isnull_cols(&cond_exprs, outer);
    let mut right = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
    if let Some(filter) = merge_filters(inner_filter, right_nulls) {
        right = push_isnull_filter(right, filter);
    }
    let info = format!(
        "semi join, equal:[{}], other cond:{}",
        explain_semi_eq(&outer_expr, &inner_expr),
        other_conds
            .iter()
            .map(explain_semi_pred)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some(SemiSpec {
        tp: JoinType::Semi,
        right_table: inner.name.clone(),
        right,
        on: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        }),
        outer_filter: left_nulls,
    })
}

fn build_spec(
    tp: JoinType,
    right_table: String,
    inner_filter: Option<ExprNode>,
    on: ExprNode,
    outer_filter: Option<ExprNode>,
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
        outer_filter,
    }
}

fn split_in_selection(
    selection: Option<&ExprNode>,
    inner: &str,
) -> Option<(Option<ExprNode>, Vec<ExprNode>)> {
    let Some(sel) = selection else {
        return Some((None, Vec::new()));
    };
    let mut inner_preds = Vec::new();
    let mut other = Vec::new();
    for item in sel.split_cnf() {
        if item.has_subquery() {
            return None;
        }
        if only_inner(&item, inner) {
            inner_preds.push(item);
        } else {
            other.push(item);
        }
    }
    Some((ExprNode::compose_cnf(inner_preds), other))
}

fn merge_filters(a: Option<ExprNode>, b: Option<ExprNode>) -> Option<ExprNode> {
    match (a, b) {
        (Some(x), Some(y)) => ExprNode::compose_cnf(vec![x, y]),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

fn push_isnull_filter(plan: PhysicalPlan, filter: ExprNode) -> PhysicalPlan {
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            pushed,
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            pushed: match pushed {
                Some(prev) => ExprNode::compose_cnf(vec![prev, filter]),
                None => Some(filter),
            },
        },
        other => PhysicalPlan::Selection {
            child: Box::new(other),
            filter,
        },
    }
}

fn not_isnull_cols(exprs: &[ExprNode], table: &str) -> Option<ExprNode> {
    let mut seen = Vec::new();
    let mut filters = Vec::new();
    for expr in exprs {
        for (t, name) in expr.referenced_columns() {
            if !t
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(table))
            {
                continue;
            }
            if seen
                .iter()
                .any(|(tn, cn): &(String, String)| tn.eq_ignore_ascii_case(table) && cn.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            seen.push((table.to_string(), name.clone()));
            filters.push(ExprNode::Unary(UnaryOpExpr {
                op: Op::Not,
                expr: Box::new(ExprNode::IsNull(IsNullExpr {
                    expr: Box::new(ExprNode::Column(ColumnNameExpr {
                        schema: None,
                        table: Some(table.to_string()),
                        name,
                    })),
                    not: false,
                })),
            }));
        }
    }
    ExprNode::compose_cnf(filters)
}

fn explain_semi_eq(left: &ExprNode, right: &ExprNode) -> String {
    format!(
        "eq({}, {})",
        explain_semi_col(left),
        explain_semi_col(right)
    )
}

fn explain_semi_pred(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Eq => "eq",
                Op::Ne => "ne",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_semi_col(&bin.left),
                explain_semi_col(&bin.right)
            )
        }
        other => other.restore_string(),
    }
}

fn explain_semi_col(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(c) => {
            let table = c.table.as_deref().unwrap_or("t");
            format!("select.{table}.{}", c.name)
        }
        other => other.restore_string(),
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
        | PhysicalPlan::IndexMerge { table, .. }
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
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Distinct { child, .. } => plan_table(child),
        _ => None,
    }
}

/// TiDB SemiJoinRewriter: DISTINCT the Semi inner (right) side on the join
/// key so the build side is unique. Keep HashJoin Semi so SELECT * still
/// emits the left schema (TiDB adds a left projection after Inner+Agg).
pub fn rewrite_semi_join_agg(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left = rewrite_semi_join_agg(*left);
            let right = rewrite_semi_join_agg(*right);
            if tp == JoinType::Semi
                && let Some(key) = join_key_on_table(&on, &right_table)
            {
                PhysicalPlan::HashJoin {
                    tp,
                    left: Box::new(left),
                    right: Box::new(PhysicalPlan::Distinct {
                        keys: vec![key],
                        child: Box::new(right),
                    }),
                    left_table,
                    right_table,
                    on,
                }
            } else {
                PhysicalPlan::HashJoin {
                    tp,
                    left: Box::new(left),
                    right: Box::new(right),
                    left_table,
                    right_table,
                    on,
                }
            }
        }
        PhysicalPlan::Apply { child, filter } => PhysicalPlan::Apply {
            child: Box::new(rewrite_semi_join_agg(*child)),
            filter,
        },
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join_agg(*child)),
            filter,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join_agg(*child)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join_agg(*child)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join_agg(*child)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join_agg(*child)),
            info,
            est_rows,
        },
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join_agg(*child)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::Window { child } => PhysicalPlan::Window {
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Union { op, left, right } => PhysicalPlan::Union {
            op,
            left: Box::new(rewrite_semi_join_agg(*left)),
            right: Box::new(rewrite_semi_join_agg(*right)),
        },
        PhysicalPlan::CTE {
            name,
            columns,
            seed,
            inline,
        } => PhysicalPlan::CTE {
            name,
            columns,
            seed: Box::new(rewrite_semi_join_agg(*seed)),
            inline,
        },
        PhysicalPlan::Distinct { keys, child } => PhysicalPlan::Distinct {
            keys,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        other => other,
    }
}

fn join_key_on_table(on: &ExprNode, table: &str) -> Option<String> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    for side in [&bin.left, &bin.right] {
        if let ExprNode::Column(col) = side.as_ref()
            && col
                .table
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(table))
        {
            return Some(col.name.clone());
        }
    }
    None
}

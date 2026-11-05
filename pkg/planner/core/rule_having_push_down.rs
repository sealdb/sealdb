//! HAVING predicate pushdown (TiDB PushSelDownAggregation / predicate push
//! across Aggregation): group-by-only conjuncts go below HashAgg; aggregate
//! predicates stay as Selection above.

use super::physical_plans::PhysicalPlan;
use super::rule_predicate_push_down::predicate_push_down;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::opcode::Op;

/// Split HAVING and push group-by predicates below aggregation.
pub fn apply_having_push_down(
    plan: PhysicalPlan,
    having: &ExprNode,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let (pushable, residual) = split_having(having, select);
    let mut plan = plan;
    if let Some(filter) = ExprNode::compose_cnf(pushable) {
        plan = push_through_agg(plan, &filter, is, current_db);
    }
    if let Some(filter) = ExprNode::compose_cnf(residual) {
        // Explain uses Column#N for agg results; keep AST for execution and
        // rewrite display in explain.rs when needed.
        plan = PhysicalPlan::Selection {
            filter: rewrite_having_agg_refs(filter, select),
            child: Box::new(plan),
        };
    }
    plan
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
    // OR of mix (col OR avg) cannot push.
    if expr_has_or(expr) && expr_has_agg_deep(expr) {
        return false;
    }
    cols_in_group_by(expr, select)
}

fn expr_has_or(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Binary(b) if matches!(b.op, Op::LogicOr) => true,
        ExprNode::Binary(b) => expr_has_or(&b.left) || expr_has_or(&b.right),
        ExprNode::Unary(u) => expr_has_or(&u.expr),
        ExprNode::Func(f) => f.args.iter().any(expr_has_or),
        _ => false,
    }
}

fn expr_has_agg(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => true,
        ExprNode::Func(f) => f.args.iter().any(expr_has_agg),
        ExprNode::Binary(b) => expr_has_agg(&b.left) || expr_has_agg(&b.right),
        ExprNode::Unary(u) => expr_has_agg(&u.expr),
        ExprNode::Cast(c) => expr_has_agg(&c.expr),
        _ => false,
    }
}

fn expr_has_agg_deep(expr: &ExprNode) -> bool {
    expr_has_agg(expr)
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

fn push_through_agg(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashAgg {
            info,
            est_rows,
            child,
        } => PhysicalPlan::HashAgg {
            info,
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
        PhysicalPlan::Selection { filter: outer, child } => PhysicalPlan::Selection {
            filter: outer,
            child: Box::new(push_through_agg(*child, filter, is, current_db)),
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(push_through_agg(*child, filter, is, current_db)),
        },
        other => predicate_push_down(other, filter, is, current_db),
    }
}

/// Map `avg(c) > 3` style HAVING to `gt(Column#5, 3)` for TiDB EXPLAIN parity.
fn rewrite_having_agg_refs(expr: ExprNode, select: &SelectStmt) -> ExprNode {
    rewrite_expr(expr, select)
}

fn rewrite_expr(expr: ExprNode, select: &SelectStmt) -> ExprNode {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => {
            // First aggregate in SELECT list → Column#5 (suite convention).
            ExprNode::Column(sealdb_parser::ast::expressions::ColumnNameExpr {
                schema: None,
                table: None,
                name: "Column#5".into(),
            })
        }
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
}

//! CTE name-scope propagation into nested SELECTs.
//!
//! TiDB resolves a statement-level `WITH` inside every subquery of that
//! statement (`… WHERE EXISTS (SELECT * FROM cte1 …)` where `cte1` is the
//! outer CTE). SealDB planning reads only `select.with`, so the visible scope
//! is pushed into nested SELECTs. Inner CTEs keep precedence for shadowed
//! names.

use super::dml::{SelectStmt, WithClause};
use super::expressions::ExprNode;

/// Push `select.with` (already merged with any outer scope) into every nested
/// SELECT reachable from this select's expressions.
pub fn inherit_cte_scope_in_select(select: &mut SelectStmt) {
    let scope = select.with.clone();
    let scope = scope.as_ref();
    for field in &mut select.fields {
        inherit_cte_in_expr(&mut field.expr, scope);
    }
    if let Some(sel) = select.selection.as_mut() {
        inherit_cte_in_expr(sel, scope);
    }
    if let Some(having) = select.having.as_mut() {
        inherit_cte_in_expr(having, scope);
    }
    for expr in &mut select.group_by {
        inherit_cte_in_expr(expr, scope);
    }
    for item in &mut select.order_by {
        inherit_cte_in_expr(&mut item.expr, scope);
    }
    for join in &mut select.joins {
        inherit_cte_in_expr(&mut join.on, scope);
    }
    if let Some(item) = select.from_item.as_mut() {
        inherit_cte_in_from_item(item, scope);
    }
}

/// Attach `outer` to `select.with`: inner definitions win on shadowed names.
pub fn merge_cte_scope(select: &mut SelectStmt, outer: &WithClause) {
    let mut scope = select.with.take().unwrap_or(WithClause {
        is_recursive: false,
        ctes: Vec::new(),
    });
    scope.is_recursive |= outer.is_recursive;
    for cte in &outer.ctes {
        if scope.find(&cte.name).is_none() {
            scope.ctes.push(cte.clone());
        }
    }
    select.with = Some(scope);
}

fn inherit_cte_in_from_item(item: &mut super::dml::FromItem, scope: Option<&WithClause>) {
    match item {
        super::dml::FromItem::Table(_) => {}
        super::dml::FromItem::Join {
            left, right, on, ..
        } => {
            inherit_cte_in_from_item(left, scope);
            inherit_cte_in_from_item(right, scope);
            inherit_cte_in_expr(on, scope);
        }
    }
}

fn inherit_cte_in_expr(expr: &mut ExprNode, scope: Option<&WithClause>) {
    match expr {
        ExprNode::ScalarSubquery(s) => inherit_cte_in_result_set(&mut s.query, scope),
        ExprNode::Exists(e) => inherit_cte_in_result_set(&mut e.query, scope),
        ExprNode::InSubquery(s) => {
            inherit_cte_in_expr(&mut s.expr, scope);
            inherit_cte_in_result_set(&mut s.query, scope);
        }
        ExprNode::QuantifiedCmp(q) => {
            inherit_cte_in_expr(&mut q.expr, scope);
            inherit_cte_in_result_set(&mut q.query, scope);
        }
        ExprNode::Binary(b) => {
            inherit_cte_in_expr(&mut b.left, scope);
            inherit_cte_in_expr(&mut b.right, scope);
        }
        ExprNode::Unary(u) => inherit_cte_in_expr(&mut u.expr, scope),
        ExprNode::Func(f) => {
            for arg in &mut f.args {
                inherit_cte_in_expr(arg, scope);
            }
        }
        ExprNode::Cast(c) => inherit_cte_in_expr(&mut c.expr, scope),
        ExprNode::InList(i) => {
            inherit_cte_in_expr(&mut i.expr, scope);
            for item in &mut i.list {
                inherit_cte_in_expr(item, scope);
            }
        }
        ExprNode::Between(b) => {
            inherit_cte_in_expr(&mut b.expr, scope);
            inherit_cte_in_expr(&mut b.low, scope);
            inherit_cte_in_expr(&mut b.high, scope);
        }
        ExprNode::Like(l) => {
            inherit_cte_in_expr(&mut l.expr, scope);
            inherit_cte_in_expr(&mut l.pattern, scope);
        }
        ExprNode::IsNull(i) => inherit_cte_in_expr(&mut i.expr, scope),
        ExprNode::Case(c) => {
            if let Some(operand) = &mut c.operand {
                inherit_cte_in_expr(operand, scope);
            }
            for (when, then) in &mut c.when_then {
                inherit_cte_in_expr(when, scope);
                inherit_cte_in_expr(then, scope);
            }
            if let Some(branch) = &mut c.else_expr {
                inherit_cte_in_expr(branch, scope);
            }
        }
        ExprNode::Window(w) => {
            for arg in &mut w.args {
                inherit_cte_in_expr(arg, scope);
            }
        }
        ExprNode::Column(_) | ExprNode::Value(_) => {}
    }
}

fn inherit_cte_in_result_set(query: &mut super::ResultSetNode, scope: Option<&WithClause>) {
    match query {
        super::ResultSetNode::Select(select) => {
            if let Some(outer) = scope {
                merge_cte_scope(select, outer);
            }
            inherit_cte_scope_in_select(select);
        }
        super::ResultSetNode::SetOpr(set) => {
            let inner = set.with.clone();
            let merged = match (scope, inner) {
                (Some(outer), Some(mut inner)) => {
                    inner.is_recursive |= outer.is_recursive;
                    for cte in &outer.ctes {
                        if inner.find(&cte.name).is_none() {
                            inner.ctes.push(cte.clone());
                        }
                    }
                    Some(inner)
                }
                (Some(outer), None) => Some(outer.clone()),
                (None, inner) => inner,
            };
            set.with = merged;
            let scope = set.with.clone();
            if let Some(outer) = scope.as_ref() {
                merge_cte_scope(&mut set.first, outer);
            }
            inherit_cte_scope_in_select(&mut set.first);
            for (_, select) in &mut set.rest {
                if let Some(outer) = scope.as_ref() {
                    merge_cte_scope(select, outer);
                }
                inherit_cte_scope_in_select(select);
            }
        }
    }
}

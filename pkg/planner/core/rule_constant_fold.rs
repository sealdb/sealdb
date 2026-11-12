//! Constant folding (TiDB `expression.FoldConstant` / `foldConstant`).
//!
//! TiDB evaluates deterministic sub-expressions at plan time, so the plan and
//! its `EXPLAIN` output never carry a literal-only function call:
//!
//! ```text
//! SELECT ifnull(NULL, c1) FROM t  ->  Projection c1
//! SELECT if(10, c1, c2) FROM t    ->  Projection c1
//! SELECT * FROM t WHERE 1 = 1     ->  TableReader (no Selection)
//! ```
//!
//! The rewriting runs bottom-up and only replaces a node when evaluation
//! succeeds, so runtime-only errors keep their original timing.

use sealdb_expression::{eval_expr, truthy};
use sealdb_parser::ast::expressions::ValueExpr;
use sealdb_parser::ast::{ExprNode, SelectStmt};
use sealdb_types::Datum;

/// Functions whose result depends on session state, time or randomness;
/// TiDB keeps them unfolded in the plan (`now()` must stay `now()`).
fn is_volatile_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "rand"
            | "random"
            | "now"
            | "current_timestamp"
            | "utc_timestamp"
            | "sysdate"
            | "curdate"
            | "curtime"
            | "current_date"
            | "current_time"
            | "localtime"
            | "localtimestamp"
            | "unix_timestamp"
            | "uuid"
            | "uuid_short"
            | "sleep"
            | "last_insert_id"
            | "connection_id"
            | "database"
            | "schema"
            | "user"
            | "current_user"
            | "session_user"
            | "system_user"
            | "version"
            | "found_rows"
            | "row_count"
            | "get_lock"
            | "release_lock"
            | "default"
            | "values"
    )
}

/// Aggregate / window names never fold even when their arguments are literal
/// (`count(1)` is computed over rows, not over the literal).
fn is_aggregate_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "group_concat"
            | "bit_and"
            | "bit_or"
            | "bit_xor"
            | "std"
            | "stddev"
            | "stddev_pop"
            | "stddev_samp"
            | "var_pop"
            | "var_samp"
            | "variance"
            | "json_arrayagg"
            | "json_objectagg"
            | "firstrow"
    )
}

/// `true` when every leaf of `expr` is a literal and no function is volatile /
/// aggregate. Subqueries are never constant.
fn is_constant(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(_) => true,
        ExprNode::Column(_) => false,
        ExprNode::Binary(b) => is_constant(&b.left) && is_constant(&b.right),
        ExprNode::Unary(u) => is_constant(&u.expr),
        ExprNode::Func(f) => {
            !f.distinct
                && !is_volatile_func(&f.name)
                && !is_aggregate_func(&f.name)
                && f.args.iter().all(is_constant)
        }
        ExprNode::Cast(c) => is_constant(&c.expr),
        ExprNode::InList(i) => is_constant(&i.expr) && i.list.iter().all(is_constant),
        ExprNode::Between(b) => is_constant(&b.expr) && is_constant(&b.low) && is_constant(&b.high),
        ExprNode::Like(l) => is_constant(&l.expr) && is_constant(&l.pattern),
        ExprNode::IsNull(i) => is_constant(&i.expr),
        ExprNode::Case(c) => {
            c.operand.as_ref().map(|e| is_constant(e)).unwrap_or(true)
                && c.when_then
                    .iter()
                    .all(|(w, t)| is_constant(w) && is_constant(t))
                && c.else_expr.as_ref().map(|e| is_constant(e)).unwrap_or(true)
        }
        ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_)
        | ExprNode::Window(_)
        | ExprNode::Row(_) => false,
    }
}

fn value(datum: Datum) -> ExprNode {
    ExprNode::Value(ValueExpr { datum })
}

fn as_const(expr: &ExprNode) -> Option<&Datum> {
    match expr {
        ExprNode::Value(v) => Some(&v.datum),
        _ => None,
    }
}

/// TiDB `ifnull(NULL, x)` / `if(<const>, x, y)` / `coalesce` short-circuit.
fn fold_control(func: &sealdb_parser::ast::expressions::FuncCallExpr) -> Option<ExprNode> {
    let name = func.name.to_ascii_lowercase();
    match name.as_str() {
        "ifnull" | "nvl" if func.args.len() == 2 => as_const(&func.args[0])?
            .is_null()
            .then(|| func.args[1].clone()),
        "if" if func.args.len() == 3 => {
            let cond = as_const(&func.args[0])?;
            let taken = if truthy(cond)? {
                &func.args[1]
            } else {
                &func.args[2]
            };
            Some(taken.clone())
        }
        "coalesce" => {
            for arg in &func.args {
                let datum = as_const(arg)?;
                if !datum.is_null() {
                    return Some(arg.clone());
                }
            }
            None
        }
        _ => None,
    }
}

/// Bottom-up constant folding. Returns `None` when nothing changed so callers
/// can skip cloning the containing plan node.
pub fn fold_constants(expr: &ExprNode) -> Option<ExprNode> {
    let mut changed = false;
    let rebuilt = fold_children(expr, &mut changed);
    // `ifnull(NULL, x)` / `if(<const>, x, y)` / `coalesce`: TiDB replaces the
    // function with the decided branch even when that branch is not constant.
    if let ExprNode::Func(f) = &rebuilt
        && let Some(branch) = fold_control(f)
    {
        return Some(fold_constants(&branch).unwrap_or(branch));
    }
    if !changed && !is_constant(&rebuilt) {
        return None;
    }
    if let Some(folded) = try_eval(&rebuilt) {
        return Some(folded);
    }
    changed.then_some(rebuilt)
}

fn fold_children(expr: &ExprNode, changed: &mut bool) -> ExprNode {
    fn fold(e: &ExprNode, changed: &mut bool) -> ExprNode {
        match fold_constants(e) {
            Some(folded) => {
                *changed = true;
                folded
            }
            None => e.clone(),
        }
    }
    use sealdb_parser::ast::expressions as ast;
    match expr {
        ExprNode::Binary(b) => ExprNode::Binary(ast::BinaryOpExpr {
            left: Box::new(fold(&b.left, changed)),
            op: b.op,
            right: Box::new(fold(&b.right, changed)),
            single_cnf_item: b.single_cnf_item,
        }),
        ExprNode::Unary(u) => ExprNode::Unary(ast::UnaryOpExpr {
            op: u.op,
            expr: Box::new(fold(&u.expr, changed)),
        }),
        ExprNode::Func(f) => ExprNode::Func(ast::FuncCallExpr {
            name: f.name.clone(),
            args: f.args.iter().map(|a| fold(a, changed)).collect(),
            distinct: f.distinct,
        }),
        ExprNode::Cast(c) => ExprNode::Cast(ast::FuncCastExpr {
            expr: Box::new(fold(&c.expr, changed)),
            target: c.target.clone(),
        }),
        ExprNode::InList(i) => ExprNode::InList(ast::InListExpr {
            expr: Box::new(fold(&i.expr, changed)),
            not: i.not,
            list: i.list.iter().map(|a| fold(a, changed)).collect(),
        }),
        ExprNode::Between(b) => ExprNode::Between(ast::BetweenExpr {
            expr: Box::new(fold(&b.expr, changed)),
            not: b.not,
            low: Box::new(fold(&b.low, changed)),
            high: Box::new(fold(&b.high, changed)),
        }),
        ExprNode::Like(l) => ExprNode::Like(ast::PatternLikeExpr {
            expr: Box::new(fold(&l.expr, changed)),
            not: l.not,
            pattern: Box::new(fold(&l.pattern, changed)),
            ci: l.ci,
        }),
        ExprNode::IsNull(i) => ExprNode::IsNull(ast::IsNullExpr {
            expr: Box::new(fold(&i.expr, changed)),
            not: i.not,
        }),
        ExprNode::Case(c) => ExprNode::Case(ast::CaseExpr {
            operand: c.operand.as_ref().map(|e| Box::new(fold(e, changed))),
            when_then: c
                .when_then
                .iter()
                .map(|(w, t)| (fold(w, changed), fold(t, changed)))
                .collect(),
            else_expr: c.else_expr.as_ref().map(|e| Box::new(fold(e, changed))),
        }),
        other => other.clone(),
    }
}

/// A literal `NULL` anywhere in the tree (used to keep TiDB's "inlined view
/// column" shape: TiDB substitutes the view's `NULL AS col` *after* its
/// constant-fold pass, so the enclosing expression stays unfolded).
fn has_null_literal(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(v) => v.datum.is_null(),
        ExprNode::Binary(b) => has_null_literal(&b.left) || has_null_literal(&b.right),
        ExprNode::Unary(u) => has_null_literal(&u.expr),
        ExprNode::Func(f) => f.args.iter().any(has_null_literal),
        ExprNode::Cast(c) => has_null_literal(&c.expr),
        ExprNode::InList(i) => has_null_literal(&i.expr) || i.list.iter().any(has_null_literal),
        ExprNode::Between(b) => {
            has_null_literal(&b.expr) || has_null_literal(&b.low) || has_null_literal(&b.high)
        }
        ExprNode::Like(l) => has_null_literal(&l.expr) || has_null_literal(&l.pattern),
        ExprNode::IsNull(i) => has_null_literal(&i.expr),
        ExprNode::Case(c) => {
            c.operand.as_ref().is_some_and(|e| has_null_literal(e))
                || c.when_then
                    .iter()
                    .any(|(w, t)| has_null_literal(w) || has_null_literal(t))
                || c.else_expr.as_ref().is_some_and(|e| has_null_literal(e))
        }
        _ => false,
    }
}

/// Evaluate a rebuilt expression that is fully constant. `None` means "leave
/// it to the executor".
fn try_eval(expr: &ExprNode) -> Option<ExprNode> {
    if !is_constant(expr) {
        return None;
    }
    if has_null_literal(expr) {
        return None;
    }
    // TiDB's parser produces a negative literal directly; SealDB keeps the
    // `unary minus` node so the planner sees the same AST shape (folding it
    // would change how range/NDV code reads `-9223372036854775808`).
    if matches!(expr, ExprNode::Unary(_)) {
        return None;
    }
    // Never fold a bare literal into itself.
    if matches!(expr, ExprNode::Value(_)) {
        return None;
    }
    eval_expr(expr).ok().map(value)
}

/// `true` when two constant equalities on the same column disagree
/// (`a = 1 AND a = 2`), which no row can satisfy.
///
/// The column's table qualifier is part of the identity: the same CTE read
/// twice (`FROM q, q q1 WHERE q.a = 1 AND q1.a = 2`) constrains two different
/// scans, not one column twice.
fn has_conflicting_equalities(expr: &ExprNode) -> bool {
    let equalities: Vec<(Option<String>, String, Datum)> = expr
        .split_cnf()
        .into_iter()
        .filter_map(|conjunct| qualified_column_const(&conjunct))
        .collect();
    equalities
        .iter()
        .enumerate()
        .any(|(i, (table, column, value))| {
            equalities[i + 1..]
                .iter()
                .any(|(other_table, other_column, other_value)| {
                    let same_table = match (table, other_table) {
                        // An unqualified name resolves to the single table in
                        // scope; a qualified one to its alias.
                        (None, None) => true,
                        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                        _ => false,
                    };
                    same_table
                        && column.eq_ignore_ascii_case(other_column)
                        && value.compare(other_value) != std::cmp::Ordering::Equal
                })
        })
}

/// `col = const` keeping the written table qualifier (`None` when unqualified).
fn qualified_column_const(expr: &ExprNode) -> Option<(Option<String>, String, Datum)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if !matches!(
        bin.op,
        sealdb_parser::opcode::Op::Eq | sealdb_parser::opcode::Op::NullSafeEq
    ) {
        return None;
    }
    let (column, other) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(column), other) | (other, ExprNode::Column(column)) => (column, other),
        _ => return None,
    };
    other.const_datum().map(|value| {
        (
            column.table.clone(),
            column.name.to_ascii_lowercase(),
            value,
        )
    })
}

/// Fold a select statement's select list and filters. Returns `None` when the
/// statement is unchanged.
pub fn fold_constant_select(select: &SelectStmt) -> Option<SelectStmt> {
    let mut changed = false;

    let fields = select
        .fields
        .iter()
        .map(|field| match fold_constants(&field.expr) {
            Some(expr) => {
                changed = true;
                let mut out = field.clone();
                out.expr = expr;
                out
            }
            None => field.clone(),
        })
        .collect::<Vec<_>>();

    let fold_filter = |filter: &Option<ExprNode>, changed: &mut bool| match filter {
        Some(expr) => match fold_constants(expr) {
            Some(folded) => {
                *changed = true;
                // `WHERE <true>` is not a filter at all; a false/`NULL`
                // predicate stays so the planner can prune to `TableDual`.
                match &folded {
                    ExprNode::Value(v) if !v.datum.is_null() && truthy(&v.datum) == Some(true) => {
                        None
                    }
                    _ => Some(folded),
                }
            }
            None => Some(expr.clone()),
        },
        None => None,
    };
    let selection = fold_filter(&select.selection, &mut changed);
    // TiDB `PredicateSimplification`: conflicting constant equalities on one
    // column (`a = 1 AND a = 2`) are unsatisfiable, so the WHERE folds to
    // `0` and the plan is a zero-row `TableDual`.
    let selection = match selection {
        Some(expr) if has_conflicting_equalities(&expr) => {
            changed = true;
            Some(ExprNode::Value(ValueExpr {
                datum: Datum::I64(0),
            }))
        }
        other => other,
    };
    let having = fold_filter(&select.having, &mut changed);

    if !changed {
        return None;
    }
    let mut out = select.clone();
    out.fields = fields;
    out.selection = selection;
    out.having = having;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold_sql(sql: &str) -> SelectStmt {
        let parser = sealdb_parser::Parser::new();
        let stmt = parser.parse_one_stmt(sql).expect("parse");
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!("expected select");
        };
        fold_constant_select(&select).expect("changed")
    }

    fn first_expr(select: &SelectStmt) -> &ExprNode {
        &select.fields[0].expr
    }

    #[test]
    fn folds_ifnull_with_null_first_arg() {
        let select = fold_sql("select ifnull(null, a) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "a"));
    }

    #[test]
    fn folds_decided_if_branch() {
        let select = fold_sql("select if(10, a, b) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "a"));
        let select = fold_sql("select if(0, a, b) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "b"));
    }

    #[test]
    fn folds_constant_arithmetic() {
        let select = fold_sql("select 1 + 2 * 3");
        assert!(matches!(first_expr(&select), ExprNode::Value(v) if v.datum == Datum::I64(7)));
    }

    #[test]
    fn keeps_volatile_functions() {
        let parser = sealdb_parser::Parser::new();
        let stmt = parser.parse_one_stmt("select now(), rand()").unwrap();
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!();
        };
        assert!(fold_constant_select(&select).is_none());
    }

    #[test]
    fn drops_true_filter_and_keeps_false_one() {
        assert!(fold_sql("select a from t where 1 = 1").selection.is_none());
        let select = fold_sql("select a from t where 1 = 0");
        assert!(matches!(
            select.selection,
            Some(ExprNode::Value(v)) if v.datum == Datum::I64(0)
        ));
    }
}

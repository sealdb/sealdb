//! TiDB `rule_inject_extra_projection.go` — inject Projection under Sort/TopN
//! when ORDER BY contains scalar functions (`rand()`, `abs(rand())+1`, …),
//! and drop constant ORDER BY items (`abs(2)`).

use super::physical_plans::PhysicalPlan;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{SelectStmt, TableName};
use sealdb_parser::opcode::Op;

/// True when every ORDER BY item folds to a constant (no columns / non-deterministic funcs).
pub fn order_by_all_constant(select: &SelectStmt) -> bool {
    !select.order_by.is_empty() && select.order_by.iter().all(|item| is_const_order_expr(&item.expr))
}

/// True when any ORDER BY item is a scalar function (not a bare column or
/// select-list ordinal — ordinals sort by output position in SortExec).
pub fn order_by_has_scalar_func(select: &SelectStmt) -> bool {
    select.order_by.iter().any(|item| {
        !matches!(item.expr, ExprNode::Column(_)) && !is_order_by_ordinal(&item.expr)
    })
}

/// InjectProjBelowSort: Projection(select cols) → Sort(Column#N) →
/// Projection(select cols, expr→Column#N) → child.
pub fn inject_proj_below_sort(
    child: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let table = select
        .from
        .as_ref()
        .map(|t| t.name.as_str())
        .unwrap_or("dual");
    let col_id = next_order_column_id(select, is, current_db);
    let mut bottom_exprs: Vec<String> = select
        .fields
        .iter()
        .map(|f| explain_select_field(table, &f.expr))
        .collect();
    let mut by_parts = Vec::new();
    let mut next_id = col_id;
    for item in &select.order_by {
        if matches!(item.expr, ExprNode::Column(_)) {
            by_parts.push(explain_select_field(table, &item.expr));
            continue;
        }
        let expr_info = explain_scalar_expr(table, &item.expr);
        bottom_exprs.push(format!("{expr_info}->Column#{next_id}"));
        by_parts.push(format!("Column#{next_id}"));
        next_id += 1;
    }
    let bottom = PhysicalPlan::Projection {
        exprs: bottom_exprs,
        child: Box::new(child),
    };
    let sorted = PhysicalPlan::Sort {
        child: Box::new(bottom),
        by_info: by_parts.join(", "),
    };
    let top_exprs: Vec<String> = select
        .fields
        .iter()
        .map(|f| explain_select_field(table, &f.expr))
        .collect();
    PhysicalPlan::Projection {
        exprs: top_exprs,
        child: Box::new(sorted),
    }
}

fn next_order_column_id(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> usize {
    // TiDB AllocPlanColumnID after table cols + handle: t(a,b) → Column#4.
    if let Some(from) = &select.from {
        if let Some(n) = table_column_count(from, is, current_db) {
            return n + 2;
        }
    }
    4
}

fn table_column_count(from: &TableName, is: Option<&InfoSchema>, current_db: &str) -> Option<usize> {
    let schema = from
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    is.and_then(|is| is.table_by_name(schema, &from.name))
        .map(|info| info.columns.len())
}

fn explain_select_field(table: &str, expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(col) => format!("select.{table}.{}", col.name),
        other => explain_scalar_expr(table, other),
    }
}

/// TiDB ScalarFunction ExplainInfo: `plus(abs(rand()), 1)`, `rand()`, …
fn explain_scalar_expr(table: &str, expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(col) => format!("select.{table}.{}", col.name),
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::Null => "<nil>".into(),
            sealdb_types::Datum::I64(n) => n.to_string(),
            sealdb_types::Datum::U64(n) => n.to_string(),
            sealdb_types::Datum::F64(n) => n.to_string(),
            _ => expr.restore_string(),
        },
        ExprNode::Func(f) => {
            let args: Vec<String> = f
                .args
                .iter()
                .map(|a| explain_scalar_expr(table, a))
                .collect();
            format!("{}({})", f.name.to_ascii_lowercase(), args.join(", "))
        }
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Plus => "plus",
                Op::Minus => "minus",
                Op::Mul => "mul",
                Op::Div => "div",
                Op::Mod => "mod",
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::LogicAnd => "and",
                Op::LogicOr => "or",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_scalar_expr(table, &bin.left),
                explain_scalar_expr(table, &bin.right)
            )
        }
        ExprNode::Unary(u) => match u.op {
            Op::UnaryMinus => format!("unaryminus({})", explain_scalar_expr(table, &u.expr)),
            Op::Not | Op::Not2 => format!("not({})", explain_scalar_expr(table, &u.expr)),
            _ => expr.restore_string(),
        },
        other => other.restore_string(),
    }
}

/// MySQL/TiDB `ORDER BY n` — 1-based select-list ordinal, not a foldable const.
fn is_order_by_ordinal(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::I64(n) => *n > 0,
            sealdb_types::Datum::U64(n) => *n > 0,
            _ => false,
        },
        _ => false,
    }
}

fn is_const_order_expr(expr: &ExprNode) -> bool {
    match expr {
        // Ordinals sort by output position; do not elide Sort (`ORDER BY 1, 2`).
        ExprNode::Value(_) if is_order_by_ordinal(expr) => false,
        ExprNode::Value(_) => true,
        ExprNode::Column(_) => false,
        ExprNode::Func(f) => {
            if is_non_deterministic_func(&f.name) {
                return false;
            }
            f.args.iter().all(is_const_order_expr)
        }
        ExprNode::Binary(bin) => {
            is_const_order_expr(&bin.left) && is_const_order_expr(&bin.right)
        }
        ExprNode::Unary(u) => is_const_order_expr(&u.expr),
        ExprNode::Cast(c) => is_const_order_expr(&c.expr),
        _ => false,
    }
}

fn is_non_deterministic_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "rand"
            | "random"
            | "uuid"
            | "sleep"
            | "sysdate"
            | "now"
            | "current_timestamp"
            | "current_time"
            | "current_date"
            | "unix_timestamp"
            | "connection_id"
    )
}

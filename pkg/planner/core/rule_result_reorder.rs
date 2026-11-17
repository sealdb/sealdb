//! ResultReorder (TiDB `pkg/planner/core/rule_result_reorder.go`, MVB).
//!
//! When `tidb_enable_ordered_result_mode` is on, TiDB injects (or completes) a
//! `Sort` on the first non-input-order operator of the plan so query results
//! are stable:
//!
//! * `Selection` / `Projection` / `Limit` / `TableDual` are input-order
//!   keepers — walk through them;
//! * an existing `Sort` gets every output column appended;
//! * otherwise a new `Sort` is injected above that operator.
//!
//! A single-column integer primary key (TiDB `pk_is_handle`) is a row handle,
//! and sorting by it alone is enough (`extractHandleCol`); common handles
//! deliberately keep every output column.
//!
//! SealDB's planner is AST-driven, so the rule rewrites the outer `SELECT`
//! statement instead of a logical plan; the sort keys produced here are the
//! same ones TiDB derives from the plan schema.
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::dml::{ByItem, SelectField, SelectStmt};
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, ValueExpr};
use sealdb_types::Datum;

/// Append the output columns (or the row handle) to `select`'s result order.
pub fn reorder_result(select: &SelectStmt, is: &InfoSchema, current_db: &str) -> SelectStmt {
    let mut select = select.clone();
    let Some(keys) = sort_keys(&select, is, current_db) else {
        return select;
    };
    for key in keys {
        if select
            .order_by
            .iter()
            .any(|item| same_key(&item.expr, &key))
        {
            continue;
        }
        select.order_by.push(ByItem {
            expr: key,
            desc: false,
        });
    }
    select
}

/// Sort keys TiDB would inject: the row handle when it is part of the output,
/// otherwise every output column.
fn sort_keys(select: &SelectStmt, is: &InfoSchema, current_db: &str) -> Option<Vec<ExprNode>> {
    // `SELECT 1` has no table and therefore no schema columns to order by.
    if select.from.is_none() && select.from_item.is_none() && select.joins.is_empty() {
        return None;
    }
    let output = output_columns(select, is, current_db);
    if output.is_empty() {
        return None;
    }
    if let Some(handle) = handle_column(select, is, current_db)
        && output.iter().any(|expr| same_column(expr, &handle))
    {
        return Some(vec![ExprNode::Column(handle)]);
    }
    Some(output)
}

/// Columns the outer projection exposes: `*` expands per table, expressions
/// are used as written (TiDB sorts by the plan schema's columns).
fn output_columns(select: &SelectStmt, is: &InfoSchema, current_db: &str) -> Vec<ExprNode> {
    if is_star_select(select) {
        let mut out = Vec::new();
        for table in select.join_tables() {
            let (db, name) = table.qualified(current_db);
            let Some(info) = is.table_by_name(&db, &name) else {
                continue;
            };
            for column in &info.columns {
                out.push(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(table.name.clone()),
                    name: column.name.o().to_string(),
                }));
            }
        }
        return out;
    }
    select
        .fields
        .iter()
        .map(|field| field.expr.clone())
        .collect()
}

/// TiDB `extractHandleCol`: only a plain `DataSource` with a single-column
/// integer primary key has a sortable row handle.
fn handle_column(select: &SelectStmt, is: &InfoSchema, current_db: &str) -> Option<ColumnNameExpr> {
    if !select.joins.is_empty() || select.from_item.is_some() {
        return None;
    }
    let table = select.from.as_ref()?;
    let (db, name) = table.qualified(current_db);
    let info = is.table_by_name(&db, &name)?;
    if !info.pk_is_handle {
        return None;
    }
    let primary = info.indices.iter().find(|index| index.primary)?;
    let column = primary.columns.first()?;
    Some(ColumnNameExpr {
        schema: None,
        table: Some(table.name.clone()),
        name: column.name.o().to_string(),
    })
}

fn same_column(expr: &ExprNode, handle: &ColumnNameExpr) -> bool {
    let ExprNode::Column(column) = expr else {
        return false;
    };
    if !column.name.eq_ignore_ascii_case(&handle.name) {
        return false;
    }
    match (&column.table, &handle.table) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => true,
    }
}

/// Two sort keys name the same output column when their names match and their
/// qualifiers are compatible (`a` ≡ `t.a`); other expressions compare by text.
fn same_key(left: &ExprNode, right: &ExprNode) -> bool {
    match (left, right) {
        (ExprNode::Column(l), ExprNode::Column(r)) => {
            l.name.eq_ignore_ascii_case(&r.name)
                && match (&l.table, &r.table) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                }
        }
        _ => left.restore_string() == right.restore_string(),
    }
}

fn is_star_select(select: &SelectStmt) -> bool {
    matches!(
        select.fields.as_slice(),
        [SelectField { expr: ExprNode::Value(ValueExpr { datum: Datum::Bytes(bytes) }), .. }]
            if bytes == b"*"
    )
}

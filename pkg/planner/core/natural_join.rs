//! `NATURAL JOIN` expansion.
//!
//! The parser cannot know table schemas, so `NATURAL` joins are lowered with
//! `natural = true` and `ON TRUE`. Before planning, this pass resolves both
//! sides' output columns against the catalog + CTE definitions and rewrites the
//! join into the equivalent `USING (...)` join (``using_cols`` plus an `ON`
//! equality chain), matching TiDB's `buildNaturalJoin`.

use sealdb_errno::{ERR_AMBIGUOUS_COLUMN, Error, Result};
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::dml::{CommonTableExpression, Join, SelectStmt};
use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ExprNode};
use sealdb_parser::ast::{FromItem, Statement};
use sealdb_parser::opcode::Op;
use sealdb_parser::ast::expressions::ValueExpr;
use sealdb_types::Datum;

use sealdb_infoschema::InfoSchema;

/// A side's visible `(column, owning table)` pairs in output order.
type SideColumns = Vec<(String, String)>;

pub fn stmt_has_natural_join(stmt: &Statement) -> bool {
    match stmt {
        Statement::Select(select) => select_has_natural(select),
        // EXPLAIN [ANALYZE] plans an inner statement; expand it too so the
        // reported plan matches the executed one.
        Statement::Explain(explain) => stmt_has_natural_join(&explain.stmt),
        _ => false,
    }
}

fn select_has_natural(select: &SelectStmt) -> bool {
    if select.joins.iter().any(|join| join.natural) {
        return true;
    }
    if let Some(FromItem::Join { .. }) = &select.from_item
        && item_has_natural(select.from_item.as_ref().expect("checked"))
    {
        return true;
    }
    select
        .with
        .as_ref()
        .is_some_and(|with| with.ctes.iter().any(|cte| select_has_natural(&cte.query)))
}

fn item_has_natural(item: &FromItem) -> bool {
    match item {
        FromItem::Table(_) => false,
        FromItem::Join {
            left,
            right,
            natural,
            ..
        } => *natural || item_has_natural(left) || item_has_natural(right),
    }
}

/// Rewrite every `NATURAL JOIN` in `stmt` into `USING` + `ON` form.
pub fn expand_natural_joins(
    stmt: &Statement,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<Statement> {
    match stmt {
        Statement::Select(select) => {
            let mut select = select.clone();
            expand_select(&mut select, is, current_db)?;
            Ok(Statement::Select(select))
        }
        Statement::Explain(explain) => {
            let mut explain = explain.clone();
            explain.stmt = Box::new(expand_natural_joins(&explain.stmt, is, current_db)?);
            Ok(Statement::Explain(explain))
        }
        _ => Ok(stmt.clone()),
    }
}

fn expand_select(
    select: &mut SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    if let Some(with) = &mut select.with {
        for cte in &mut with.ctes {
            expand_select(&mut cte.query, is, current_db)?;
            for (_, query) in &mut cte.set_rest {
                expand_select(query, is, current_db)?;
            }
            for (_, query) in &mut cte.recursive_rest {
                expand_select(query, is, current_db)?;
            }
        }
    }
    if select.from_item.is_some() {
        let mut item = select.from_item.take().expect("checked");
        expand_from_item(&mut item, &cte_scope(select), is, current_db)?;
        select.joins = flatten_from_item(&item);
        select.from_item = Some(item);
        return Ok(());
    }
    expand_join_chain(select, is, current_db)
}

fn cte_scope(select: &SelectStmt) -> Vec<CommonTableExpression> {
    select
        .with
        .as_ref()
        .map(|with| with.ctes.clone())
        .unwrap_or_default()
}

fn expand_join_chain(
    select: &mut SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    let ctes = cte_scope(select);
    let mut left = match &select.from {
        Some(table) => table_side_columns(table, &ctes, is, current_db),
        None => None,
    };
    for join in &mut select.joins {
        let right = table_side_columns(&join.right, &ctes, is, current_db);
        if join.natural
            && let Some(right_cols) = right.clone()
        {
            // A NATURAL JOIN binds to its own left operand, not to the whole
            // accumulated FROM list: `t1, t2 NATURAL JOIN t3` compares t2 and
            // t3 only (comma is lower precedence than JOIN).
            let left_cols = table_side_columns(&join.left, &ctes, is, current_db)
                .or_else(|| left.clone())
                .expect("left operand columns resolved");
            let (using_cols, on) = resolve_natural(left_cols, right_cols)?;
            join.using_cols = using_cols;
            join.on = on;
            join.natural = false;
        }
        left = merge_side_columns(left, right, &join.using_cols);
    }
    Ok(())
}

fn expand_from_item(
    item: &mut FromItem,
    ctes: &[CommonTableExpression],
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    let FromItem::Join {
        left,
        right,
        tp,
        on,
        using_cols,
        natural,
    } = item
    else {
        return Ok(());
    };
    expand_from_item(left, ctes, is, current_db)?;
    expand_from_item(right, ctes, is, current_db)?;
    if !*natural {
        return Ok(());
    }
    let (Some(left_cols), Some(right_cols)) = (
        item_side_columns(left, ctes, is, current_db),
        item_side_columns(right, ctes, is, current_db),
    ) else {
        return Ok(());
    };
    let (cols, on_expr) = resolve_natural(left_cols, right_cols)?;
    *using_cols = cols;
    *on = on_expr;
    *natural = false;
    let _ = tp;
    Ok(())
}

/// TiDB `buildNaturalJoin`: common columns in left output order; a column that
/// appears more than once on either side is ambiguous (error 1052).
fn resolve_natural(left: SideColumns, right: SideColumns) -> Result<(Vec<String>, ExprNode)> {
    let count = |cols: &SideColumns, name: &str| {
        cols.iter()
            .filter(|(col, _)| col.eq_ignore_ascii_case(name))
            .count()
    };
    let mut cols = Vec::new();
    let mut eqs: Vec<ExprNode> = Vec::new();
    for (col, left_owner) in &left {
        if !right.iter().any(|(r, _)| r.eq_ignore_ascii_case(col)) {
            continue;
        }
        let (lc, rc) = (count(&left, col), count(&right, col));
        if lc > 1 || rc > 1 {
            return Err(Error::new(
                ERR_AMBIGUOUS_COLUMN,
                format!("Column '{col}' in from clause is ambiguous"),
            ));
        }
        let right_owner = right
            .iter()
            .find(|(r, _)| r.eq_ignore_ascii_case(col))
            .map(|(_, owner)| owner.clone())
            .unwrap_or_default();
        cols.push(col.clone());
        eqs.push(equi_column(col, left_owner, &right_owner));
    }
    if eqs.is_empty() {
        return Ok((cols, true_expr()));
    }
    let mut iter = eqs.into_iter();
    let first = iter.next().expect("non-empty");
    let on = iter.fold(first, |acc, next| {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(acc),
            right: Box::new(next),
            single_cnf_item: false,
        })
    });
    Ok((cols, on))
}

fn equi_column(col: &str, left_table: &str, right_table: &str) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(column_ref(left_table, col)),
        right: Box::new(column_ref(right_table, col)),
        single_cnf_item: false,
    })
}

fn column_ref(table: &str, col: &str) -> ExprNode {
    ExprNode::Column(ColumnNameExpr {
        schema: None,
        table: Some(table.to_string()),
        name: col.to_string(),
    })
}

fn true_expr() -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::I64(1),
    })
}

fn table_side_columns(
    table: &TableName,
    ctes: &[CommonTableExpression],
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SideColumns> {
    let name = table.resolve_name();
    if let Some(cte) = ctes
        .iter()
        .find(|cte| cte.name.eq_ignore_ascii_case(name) || cte.name.eq_ignore_ascii_case(&table.name))
    {
        let cols = cte_columns(cte);
        return Some(cols.into_iter().map(|col| (col, table.name.clone())).collect());
    }
    let is = is?;
    let schema = table
        .schema
        .as_deref()
        .filter(|schema| !schema.is_empty())
        .unwrap_or(current_db);
    let info = is.table_by_name(schema, name)?;
    Some(
        info.columns
            .iter()
            .map(|col| (col.name.o().to_string(), table.name.clone()))
            .collect(),
    )
}

fn cte_columns(cte: &CommonTableExpression) -> Vec<String> {
    if !cte.col_name_list.is_empty() {
        return cte.col_name_list.clone();
    }
    select_field_names(&cte.query)
}

fn select_field_names(select: &SelectStmt) -> Vec<String> {
    select
        .fields
        .iter()
        .map(|field| match (&field.as_name, &field.expr) {
            (Some(alias), _) => alias.clone(),
            (None, ExprNode::Column(col)) => col.name.clone(),
            // No alias: TiDB labels the item with its text (`select null` → NULL).
            (None, expr) => field
                .orig_text
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| expr.restore_string()),
        })
        .collect()
}

fn item_side_columns(
    item: &FromItem,
    ctes: &[CommonTableExpression],
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SideColumns> {
    match item {
        FromItem::Table(table) => table_side_columns(table, ctes, is, current_db),
        FromItem::Join {
            left,
            right,
            using_cols,
            ..
        } => {
            let left_cols = item_side_columns(left, ctes, is, current_db)?;
            let right_cols = item_side_columns(right, ctes, is, current_db)?;
            merge_side_columns(Some(left_cols), Some(right_cols), using_cols)
        }
    }
}

/// Output columns after a join: left order, then right columns not in `using`.
fn merge_side_columns(
    left: Option<SideColumns>,
    right: Option<SideColumns>,
    using_cols: &[String],
) -> Option<SideColumns> {
    let mut cols = left?;
    let right = right?;
    let dropped = |col: &str| using_cols.iter().any(|u| u.eq_ignore_ascii_case(col));
    cols.extend(right.into_iter().filter(|(col, _)| !dropped(col)));
    Some(cols)
}

/// Re-derive the flat join chain from an expanded `FromItem` tree so
/// `select_output_columns` / executor build see the same `using_cols`.
fn flatten_from_item(item: &FromItem) -> Vec<Join> {
    match item {
        FromItem::Table(_) => Vec::new(),
        FromItem::Join {
            left,
            right,
            tp,
            on,
            using_cols,
            natural,
        } => {
            let mut joins = flatten_from_item(left);
            joins.extend(flatten_from_item(right));
            let left_tables = left.tables();
            let right_tables = right.tables();
            if let (Some(l), Some(r)) = (left_tables.last(), right_tables.first()) {
                joins.push(Join {
                    left: (*l).clone(),
                    right: (*r).clone(),
                    tp: *tp,
                    on: on.clone(),
                    using_cols: using_cols.clone(),
                    natural: *natural,
                    straight: false,
                });
            }
            joins
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(pairs: &[(&str, &str)]) -> SideColumns {
        pairs
            .iter()
            .map(|(col, table)| ((*col).to_string(), (*table).to_string()))
            .collect()
    }

    #[test]
    fn natural_common_columns_in_left_order() {
        let (cols, _) = resolve_natural(side(&[("c", "t1"), ("b", "t1")]), side(&[("b", "t2"), ("a", "t2")]))
            .unwrap();
        assert_eq!(cols, vec!["b".to_string()]);
    }

    #[test]
    fn natural_duplicate_column_is_ambiguous() {
        let err = resolve_natural(
            side(&[("c", "t1"), ("b", "t1")]),
            side(&[("b", "t3"), ("c", "t3"), ("y", "t4"), ("c", "t4")]),
        )
        .unwrap_err();
        assert_eq!(err.code, ERR_AMBIGUOUS_COLUMN);
        assert_eq!(err.message, "Column 'c' in from clause is ambiguous");
    }

    #[test]
    fn natural_from_item_owner_resolution() {
        let left = side(&[("c", "t1"), ("b", "t1"), ("a", "t2"), ("b", "t2")]);
        let err = resolve_natural(left, side(&[("b", "t3"), ("c", "t3"), ("y", "t4")])).unwrap_err();
        assert_eq!(err.message, "Column 'b' in from clause is ambiguous");
    }
}

//! TiDB expression rewriter for `NOT NULL` columns.
//!
//! `expression_rewriter.go` turns `ifnull(col, const)` into a cast whenever
//! `col` is a column declared `NOT NULL` (and not ENUM/SET):
//!
//! ```text
//! ifnull(nb, 0)  ->  cast(nb, bigint BINARY)
//! ```
//!
//! The cast target is `InferType4ControlFuncs`'s merged type of both branches;
//! for an integer column and an integer constant that is `bigint`.
//!
//! `FoldConstant` then folds `isnull(col)` to `0` on such a column, which is why
//! `ifnull(nb, 1) or nb is null` becomes `or(cast(nb, bigint BINARY), 0)`.
//! A column on the null-supplying side of an outer join can be NULL at runtime,
//! so both rewrites are skipped there.

use sealdb_meta::model::ColumnInfo;
use sealdb_parser::ast::dml::{JoinType, SelectStmt};
use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode, FuncCallExpr, FuncCastExpr, ValueExpr};
use sealdb_types::field_type::{
    NOT_NULL_FLAG, TYPE_DECIMAL, TYPE_DOUBLE, TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONG_LONG,
    TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_TINY,
};
use sealdb_types::Datum;

/// Column lookup used by the rewrites below.
struct Columns<'a> {
    is: &'a sealdb_infoschema::InfoSchema,
    current_db: &'a str,
    select: &'a SelectStmt,
    /// Tables whose columns can be NULL after an outer join (null-supplying
    /// side): `left join` right side, `right join` everything to the left.
    nullable: Vec<String>,
}

impl Columns<'_> {
    fn build(select: &SelectStmt) -> Vec<String> {
        let mut nullable = Vec::new();
        for join in &select.joins {
            match join.tp {
                JoinType::LeftOuter => nullable.push(join.right.resolve_name().to_string()),
                JoinType::RightOuter => {
                    nullable.push(join.left.resolve_name().to_string());
                    for earlier in &select.joins {
                        if earlier.right.name.eq_ignore_ascii_case(&join.right.name) {
                            break;
                        }
                        nullable.push(earlier.right.resolve_name().to_string());
                    }
                }
                _ => {}
            }
        }
        nullable
    }

    fn column(&self, col: &sealdb_parser::ast::expressions::ColumnNameExpr) -> Option<&ColumnInfo> {
        let is_table_nullable = |name: &str| {
            self.nullable
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
        };
        for table in self.select.join_tables() {
            if let Some(tbl) = col.table.as_deref()
                && !table.name.eq_ignore_ascii_case(tbl)
                && !table.resolve_name().eq_ignore_ascii_case(tbl)
            {
                continue;
            }
            let schema = table
                .schema
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(self.current_db);
            let Some(info) = self.is.table_by_name(schema, table.resolve_name()) else {
                continue;
            };
            if is_table_nullable(table.name.as_str())
                || is_table_nullable(table.resolve_name())
            {
                return None;
            }
            return info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(&col.name));
        }
        None
    }
}

fn is_integer_type(tp: u8) -> bool {
    matches!(
        tp,
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_LONG_LONG | TYPE_INT24
    )
}

fn is_number_type(tp: u8) -> bool {
    is_integer_type(tp) || matches!(tp, TYPE_DOUBLE | TYPE_FLOAT | TYPE_DECIMAL | TYPE_NEW_DECIMAL)
}

/// TiDB cast target text for the merged `ifnull` type. Only the numeric mixes
/// the rewriter has to reproduce are handled; anything else keeps `ifnull`.
fn cast_target(column_tp: u8, rhs: &Datum) -> Option<&'static str> {
    if !is_number_type(column_tp) {
        return None;
    }
    match rhs {
        Datum::I64(_) | Datum::U64(_) => Some("bigint"),
        Datum::F64(_) => Some("double"),
        Datum::Decimal(_) => Some("decimal"),
        _ => None,
    }
}

fn is_enum_or_set(tp: u8) -> bool {
    // mysql.TypeEnum = 0xf7, mysql.TypeSet = 0xf8
    matches!(tp, 0xf7 | 0xf8)
}

/// `ifnull(<not null col>, <numeric const>)` → `cast(col, <merged type>)`.
fn rewrite_ifnull(func: &FuncCallExpr, cols: &Columns<'_>) -> Option<ExprNode> {
    if !(func.name.eq_ignore_ascii_case("ifnull") || func.name.eq_ignore_ascii_case("nvl"))
        || func.args.len() != 2
        || func.distinct
    {
        return None;
    }
    let ExprNode::Column(col) = &func.args[0] else {
        return None;
    };
    let info = cols.column(col)?;
    if info.field_type.flag & NOT_NULL_FLAG == 0 || is_enum_or_set(info.field_type.tp) {
        return None;
    }
    let ExprNode::Value(ValueExpr { datum }) = &func.args[1] else {
        return None;
    };
    let target = cast_target(info.field_type.tp, datum)?;
    Some(ExprNode::Cast(FuncCastExpr {
        expr: Box::new(func.args[0].clone()),
        target: target.to_string(),
    }))
}

/// `isnull(<not null col>)` → `0`.
fn rewrite_isnull(expr: &sealdb_parser::ast::expressions::IsNullExpr, cols: &Columns<'_>) -> Option<ExprNode> {
    if expr.not {
        return None;
    }
    let ExprNode::Column(col) = expr.expr.as_ref() else {
        return None;
    };
    let info = cols.column(col)?;
    if info.field_type.flag & NOT_NULL_FLAG == 0 {
        return None;
    }
    Some(ExprNode::Value(ValueExpr {
        datum: Datum::I64(0),
    }))
}

fn rewrite(expr: &ExprNode, cols: &Columns<'_>, changed: &mut bool) -> ExprNode {
    let rebuilt = match expr {
        ExprNode::Func(f) => {
            if let Some(rewritten) = rewrite_ifnull(f, cols) {
                *changed = true;
                return rewritten;
            }
            ExprNode::Func(FuncCallExpr {
                name: f.name.clone(),
                args: f.args.iter().map(|a| rewrite(a, cols, changed)).collect(),
                distinct: f.distinct,
            })
        }
        ExprNode::IsNull(i) => {
            if let Some(rewritten) = rewrite_isnull(i, cols) {
                *changed = true;
                return rewritten;
            }
            ExprNode::IsNull(sealdb_parser::ast::expressions::IsNullExpr {
                not: i.not,
                expr: Box::new(rewrite(&i.expr, cols, changed)),
            })
        }
        ExprNode::Binary(b) => ExprNode::Binary(BinaryOpExpr {
            left: Box::new(rewrite(&b.left, cols, changed)),
            op: b.op,
            right: Box::new(rewrite(&b.right, cols, changed)),
            single_cnf_item: b.single_cnf_item,
        }),
        ExprNode::Unary(u) => ExprNode::Unary(sealdb_parser::ast::expressions::UnaryOpExpr {
            op: u.op,
            expr: Box::new(rewrite(&u.expr, cols, changed)),
        }),
        ExprNode::Cast(c) => ExprNode::Cast(FuncCastExpr {
            expr: Box::new(rewrite(&c.expr, cols, changed)),
            target: c.target.clone(),
        }),
        ExprNode::Between(b) => ExprNode::Between(sealdb_parser::ast::expressions::BetweenExpr {
            expr: Box::new(rewrite(&b.expr, cols, changed)),
            low: Box::new(rewrite(&b.low, cols, changed)),
            high: Box::new(rewrite(&b.high, cols, changed)),
            not: b.not,
        }),
        ExprNode::InList(i) => ExprNode::InList(sealdb_parser::ast::expressions::InListExpr {
            expr: Box::new(rewrite(&i.expr, cols, changed)),
            list: i.list.iter().map(|e| rewrite(e, cols, changed)).collect(),
            not: i.not,
        }),
        ExprNode::Like(l) => ExprNode::Like(sealdb_parser::ast::expressions::PatternLikeExpr {
            expr: Box::new(rewrite(&l.expr, cols, changed)),
            pattern: Box::new(rewrite(&l.pattern, cols, changed)),
            not: l.not,
            ci: l.ci,
        }),
        ExprNode::Case(c) => ExprNode::Case(sealdb_parser::ast::expressions::CaseExpr {
            operand: c.operand.as_ref().map(|e| Box::new(rewrite(e, cols, changed))),
            when_then: c
                .when_then
                .iter()
                .map(|(w, t)| {
                    (
                        rewrite(w, cols, changed),
                        rewrite(t, cols, changed),
                    )
                })
                .collect(),
            else_expr: c.else_expr.as_ref().map(|e| Box::new(rewrite(e, cols, changed))),
        }),
        other => other.clone(),
    };
    rebuilt
}

/// Rewrite every expression of the statement; `None` when nothing changed.
pub fn fold_not_null(
    select: &SelectStmt,
    is: &sealdb_infoschema::InfoSchema,
    current_db: &str,
) -> Option<SelectStmt> {
    let nullable = Columns::build(select);
    let cols = Columns {
        is,
        current_db,
        select,
        nullable,
    };
    let mut changed = false;

    let mut out = select.clone();
    for field in &mut out.fields {
        field.expr = rewrite(&field.expr, &cols, &mut changed);
    }
    if let Some(selection) = out.selection.clone() {
        out.selection = Some(rewrite(&selection, &cols, &mut changed));
    }
    if let Some(having) = out.having.clone() {
        out.having = Some(rewrite(&having, &cols, &mut changed));
    }
    for expr in &mut out.group_by {
        *expr = rewrite(expr, &cols, &mut changed);
    }
    for item in &mut out.order_by {
        item.expr = rewrite(&item.expr, &cols, &mut changed);
    }
    for join in &mut out.joins {
        join.on = rewrite(&join.on, &cols, &mut changed);
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_target_mixes() {
        assert_eq!(cast_target(TYPE_LONG, &Datum::I64(0)), Some("bigint"));
        assert_eq!(cast_target(TYPE_LONG, &Datum::F64(0.0)), Some("double"));
        assert_eq!(cast_target(TYPE_LONG, &Datum::Null), None);
    }
}

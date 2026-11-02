use sealdb_errno::{ERR_BAD_FIELD, ERR_DIVISION_BY_ZERO, ERR_UNKNOWN_FUNCTION, Error, Result};
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ColumnNameExpr, ExprNode, FuncCallExpr, FuncCastExpr, InListExpr,
    IsNullExpr, PatternLikeExpr, UnaryOpExpr,
};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;
use sealdb_types::field_type::FieldType;
use std::cell::RefCell;

thread_local! {
    static COLLATION_CI: RefCell<bool> = const { RefCell::new(false) };
}

pub fn with_collation_ci<T>(ci: bool, f: impl FnOnce() -> T) -> T {
    COLLATION_CI.with(|cell| {
        let prev = *cell.borrow();
        *cell.borrow_mut() = ci;
        let out = f();
        *cell.borrow_mut() = prev;
        out
    })
}

pub fn collation_ci() -> bool {
    COLLATION_CI.with(|cell| *cell.borrow())
}

fn compare_datums(left: &Datum, right: &Datum) -> std::cmp::Ordering {
    if matches!((left, right), (Datum::Bytes(_), Datum::Bytes(_)))
        && COLLATION_CI.with(|c| *c.borrow())
    {
        let left_b = left.as_bytes();
        let right_b = right.as_bytes();
        return if left_b.eq_ignore_ascii_case(&right_b) {
            std::cmp::Ordering::Equal
        } else {
            left_b
                .to_ascii_lowercase()
                .cmp(&right_b.to_ascii_lowercase())
        };
    }
    left.compare(right)
}

pub fn eval_expr(expr: &ExprNode) -> Result<Datum> {
    eval_expr_with_row(expr, &[], &[])
}

pub fn eval_expr_with_row(expr: &ExprNode, schema: &[String], row: &[Datum]) -> Result<Datum> {
    match expr {
        ExprNode::Value(v) => Ok(v.datum.clone()),
        ExprNode::Column(c) => resolve_column(c, schema, row),
        ExprNode::Binary(b) => eval_binary(b, schema, row),
        ExprNode::Unary(u) => eval_unary(u, schema, row),
        ExprNode::Func(f) => eval_func(f, schema, row),
        ExprNode::Cast(c) => eval_cast(c, schema, row),
        ExprNode::InList(i) => eval_in_list(i, schema, row),
        ExprNode::Between(b) => eval_between(b, schema, row),
        ExprNode::Like(l) => eval_like(l, schema, row),
        ExprNode::IsNull(i) => eval_is_null(i, schema, row),
        ExprNode::Exists(_) | ExprNode::InSubquery(_) | ExprNode::ScalarSubquery(_) => {
            Err(Error::unsupported("subquery evaluation outside Apply"))
        }
        ExprNode::Window(_) => Err(Error::unsupported("window evaluation outside WindowExec")),
    }
}

pub fn eval_filter(expr: &ExprNode, schema: &[String], row: &[Datum]) -> Result<bool> {
    Ok(truthy(&eval_expr_with_row(expr, schema, row)?).unwrap_or(false))
}

fn resolve_column(col: &ColumnNameExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let mut found = None;
    for (i, name) in schema.iter().enumerate() {
        if column_matches(col, name) {
            if found.is_some() && col.table.is_none() {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Column '{}' in field list is ambiguous", col.name),
                ));
            }
            found = Some(i);
        }
    }
    let idx = found.ok_or_else(|| {
        Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{}' in 'where clause'", col.name),
        )
    })?;
    Ok(row.get(idx).cloned().unwrap_or(Datum::Null))
}

fn column_matches(col: &ColumnNameExpr, schema_name: &str) -> bool {
    let (table, name) = split_qualified(schema_name);
    let name_ok =
        name.eq_ignore_ascii_case(&col.name) || schema_name.eq_ignore_ascii_case(&col.name);
    match col.table.as_deref() {
        Some(wanted) => {
            name_ok
                && (table.is_none()
                    || table.is_some_and(|t| t.eq_ignore_ascii_case(wanted))
                    || schema_name.eq_ignore_ascii_case(&format!("{wanted}.{}", col.name)))
        }
        None => name_ok,
    }
}

fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((table, col)) => (Some(table), col),
        None => (None, name),
    }
}

fn eval_binary(expr: &BinaryOpExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let left = eval_expr_with_row(&expr.left, schema, row)?;
    let right = eval_expr_with_row(&expr.right, schema, row)?;
    if matches!(expr.op, Op::LogicAnd | Op::LogicOr) {
        return eval_logic(expr.op, &left, &right);
    }
    if left.is_null() || right.is_null() {
        return Ok(Datum::Null);
    }
    match expr.op {
        Op::Plus => num_bin(&left, &right, |a, b| Ok(a + b), |a, b| Ok(a + b)),
        Op::Minus => num_bin(&left, &right, |a, b| Ok(a - b), |a, b| Ok(a - b)),
        Op::Mul => num_bin(&left, &right, |a, b| Ok(a * b), |a, b| Ok(a * b)),
        Op::Div => num_bin(
            &left,
            &right,
            |a, b| {
                if b == 0 {
                    Err(Error::new(ERR_DIVISION_BY_ZERO, "Division by 0"))
                } else {
                    Ok(a / b)
                }
            },
            |a, b| {
                if b == 0.0 {
                    Err(Error::new(ERR_DIVISION_BY_ZERO, "Division by 0"))
                } else {
                    Ok(a / b)
                }
            },
        ),
        Op::Mod => num_bin(
            &left,
            &right,
            |a, b| {
                if b == 0 {
                    Err(Error::new(ERR_DIVISION_BY_ZERO, "Division by 0"))
                } else {
                    Ok(a % b)
                }
            },
            |a, b| {
                if b == 0.0 {
                    Err(Error::new(ERR_DIVISION_BY_ZERO, "Division by 0"))
                } else {
                    Ok(a % b)
                }
            },
        ),
        Op::Eq => Ok(bool_datum(compare_datums(&left, &right).is_eq())),
        Op::Ne => Ok(bool_datum(!compare_datums(&left, &right).is_eq())),
        Op::Lt => Ok(bool_datum(compare_datums(&left, &right).is_lt())),
        Op::Le => Ok(bool_datum(compare_datums(&left, &right).is_le())),
        Op::Gt => Ok(bool_datum(compare_datums(&left, &right).is_gt())),
        Op::Ge => Ok(bool_datum(compare_datums(&left, &right).is_ge())),
        _ => Err(Error::unsupported(format!("binary {}", expr.op.restore()))),
    }
}

fn eval_logic(op: Op, left: &Datum, right: &Datum) -> Result<Datum> {
    let l = truthy(left);
    let r = truthy(right);
    match op {
        Op::LogicAnd => match (l, r) {
            (Some(false), _) | (_, Some(false)) => Ok(bool_datum(false)),
            (Some(true), Some(true)) => Ok(bool_datum(true)),
            _ => Ok(Datum::Null),
        },
        Op::LogicOr => match (l, r) {
            (Some(true), _) | (_, Some(true)) => Ok(bool_datum(true)),
            (Some(false), Some(false)) => Ok(bool_datum(false)),
            _ => Ok(Datum::Null),
        },
        _ => unreachable!(),
    }
}

fn eval_unary(expr: &UnaryOpExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let inner = eval_expr_with_row(&expr.expr, schema, row)?;
    if inner.is_null() {
        return Ok(Datum::Null);
    }
    match expr.op {
        Op::UnaryMinus | Op::Minus => match inner {
            Datum::I64(v) => Ok(Datum::I64(-v)),
            Datum::F64(v) => Ok(Datum::F64(-v)),
            other => Ok(Datum::I64(-other.as_i64()?)),
        },
        Op::Not => Ok(bool_datum(!truthy(&inner).unwrap_or(false))),
        Op::Plus => Ok(inner),
        _ => Err(Error::unsupported(format!("unary {}", expr.op.restore()))),
    }
}

fn eval_func(func: &FuncCallExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    match func.name.to_ascii_uppercase().as_str() {
        "IF" => {
            if func.args.len() != 3 {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "IF requires 3 arguments",
                ));
            }
            let cond = eval_expr_with_row(&func.args[0], schema, row)?;
            if truthy(&cond).unwrap_or(false) {
                eval_expr_with_row(&func.args[1], schema, row)
            } else {
                eval_expr_with_row(&func.args[2], schema, row)
            }
        }
        "IFNULL" => {
            if func.args.len() != 2 {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "IFNULL requires 2 arguments",
                ));
            }
            let first = eval_expr_with_row(&func.args[0], schema, row)?;
            if first.is_null() {
                eval_expr_with_row(&func.args[1], schema, row)
            } else {
                Ok(first)
            }
        }
        other => Err(Error::new(
            ERR_UNKNOWN_FUNCTION,
            format!("FUNCTION {other} does not exist"),
        )),
    }
}

fn eval_in_list(expr: &InListExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let left = eval_expr_with_row(&expr.expr, schema, row)?;
    if left.is_null() {
        return Ok(Datum::Null);
    }
    let mut saw_null = false;
    for item in &expr.list {
        let value = eval_expr_with_row(item, schema, row)?;
        if value.is_null() {
            saw_null = true;
            continue;
        }
        if compare_datums(&left, &value).is_eq() {
            return Ok(bool_datum(!expr.not));
        }
    }
    if saw_null {
        Ok(Datum::Null)
    } else {
        Ok(bool_datum(expr.not))
    }
}

fn eval_between(expr: &BetweenExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let value = eval_expr_with_row(&expr.expr, schema, row)?;
    let low = eval_expr_with_row(&expr.low, schema, row)?;
    let high = eval_expr_with_row(&expr.high, schema, row)?;
    if value.is_null() || low.is_null() || high.is_null() {
        return Ok(Datum::Null);
    }
    let inside = compare_datums(&value, &low).is_ge() && compare_datums(&value, &high).is_le();
    Ok(bool_datum(if expr.not { !inside } else { inside }))
}

fn eval_like(expr: &PatternLikeExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let value = eval_expr_with_row(&expr.expr, schema, row)?;
    let pattern = eval_expr_with_row(&expr.pattern, schema, row)?;
    if value.is_null() || pattern.is_null() {
        return Ok(Datum::Null);
    }
    let ci = expr.ci || COLLATION_CI.with(|c| *c.borrow());
    let matched = like_match(&value.as_bytes(), &pattern.as_bytes(), ci);
    Ok(bool_datum(if expr.not { !matched } else { matched }))
}

fn eval_is_null(expr: &IsNullExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let value = eval_expr_with_row(&expr.expr, schema, row)?;
    Ok(bool_datum(if expr.not {
        !value.is_null()
    } else {
        value.is_null()
    }))
}

fn like_match(text: &[u8], pattern: &[u8], ci: bool) -> bool {
    like_at(text, 0, pattern, 0, ci)
}

fn like_at(text: &[u8], ti: usize, pattern: &[u8], pi: usize, ci: bool) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }
    match pattern[pi] {
        b'%' => {
            like_at(text, ti, pattern, pi + 1, ci)
                || (ti < text.len() && like_at(text, ti + 1, pattern, pi, ci))
        }
        b'_' => ti < text.len() && like_at(text, ti + 1, pattern, pi + 1, ci),
        ch => {
            ti < text.len()
                && bytes_eq(text[ti], ch, ci)
                && like_at(text, ti + 1, pattern, pi + 1, ci)
        }
    }
}

fn bytes_eq(a: u8, b: u8, ci: bool) -> bool {
    if ci {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn eval_cast(cast: &FuncCastExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let value = eval_expr_with_row(&cast.expr, schema, row)?;
    let ft = match cast.target.to_ascii_uppercase().as_str() {
        "SIGNED" | "SIGNED INTEGER" => FieldType::new_longlong(),
        "UNSIGNED" | "UNSIGNED INTEGER" => {
            let mut ft = FieldType::new_longlong();
            ft.unsigned = true;
            ft
        }
        "DOUBLE" | "REAL" | "FLOAT" => FieldType::new_double(),
        "CHAR" | "BINARY" | "CHARSET" => FieldType::new_string(),
        "DECIMAL" | "NUMERIC" => FieldType::new_newdecimal(),
        _ => FieldType::new_string(),
    };
    value.cast(&ft)
}

fn num_bin(
    left: &Datum,
    right: &Datum,
    i: impl Fn(i64, i64) -> Result<i64>,
    f: impl Fn(f64, f64) -> Result<f64>,
) -> Result<Datum> {
    if matches!(left, Datum::F64(_)) || matches!(right, Datum::F64(_)) {
        Ok(Datum::F64(f(left.as_f64()?, right.as_f64()?)?))
    } else {
        Ok(Datum::I64(i(left.as_i64()?, right.as_i64()?)?))
    }
}

fn truthy(d: &Datum) -> Option<bool> {
    if d.is_null() {
        return None;
    }
    match d {
        Datum::I64(v) => Some(*v != 0),
        Datum::U64(v) => Some(*v != 0),
        Datum::F64(v) => Some(*v != 0.0),
        Datum::Bytes(b) => Some(!b.is_empty()),
        _ => Some(d.as_i64().ok().is_some_and(|v| v != 0)),
    }
}

fn bool_datum(v: bool) -> Datum {
    Datum::I64(i64::from(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::Parser;

    fn eval_sql_expr(sql: &str) -> Datum {
        let stmt = Parser::new()
            .parse_one_stmt(&format!("select {sql}"))
            .unwrap();
        let select = stmt.as_select().unwrap();
        eval_expr(&select.fields[0].expr).unwrap()
    }

    #[test]
    fn test_f01_arithmetic_compare_logic_cast_if() {
        assert_eq!(eval_sql_expr("1 + 2"), Datum::I64(3));
        assert_eq!(eval_sql_expr("6 / 2"), Datum::I64(3));
        assert_eq!(eval_sql_expr("1 = 1"), Datum::I64(1));
        assert_eq!(eval_sql_expr("1 < 2"), Datum::I64(1));
        assert_eq!(eval_sql_expr("1 AND 0"), Datum::I64(0));
        assert_eq!(eval_sql_expr("0 OR 1"), Datum::I64(1));
        assert_eq!(
            eval_sql_expr("CAST(1 AS CHAR)"),
            Datum::Bytes(b"1".to_vec())
        );
        assert_eq!(eval_sql_expr("IF(1, 10, 20)"), Datum::I64(10));
        assert_eq!(eval_sql_expr("IFNULL(NULL, 7)"), Datum::I64(7));
    }

    #[test]
    fn test_column_filter() {
        let stmt = Parser::new()
            .parse_one_stmt("SELECT * FROM t WHERE id = 1 AND name = 'a'")
            .unwrap();
        let expr = stmt.as_select().unwrap().selection.as_ref().unwrap();
        let schema = ["id".to_string(), "name".to_string()];
        assert!(eval_filter(expr, &schema, &[Datum::I64(1), Datum::Bytes(b"a".to_vec())]).unwrap());
        assert!(
            !eval_filter(expr, &schema, &[Datum::I64(2), Datum::Bytes(b"a".to_vec())]).unwrap()
        );
    }
}

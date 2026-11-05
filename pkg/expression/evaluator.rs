use sealdb_errno::{
    ERR_BAD_FIELD, ERR_DATA_OUT_OF_RANGE, ERR_DIVISION_BY_ZERO, ERR_UNKNOWN_ERROR,
    ERR_UNKNOWN_FUNCTION, Error, Result,
};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ColumnNameExpr, ExprNode, FuncCallExpr, FuncCastExpr, InListExpr,
    InSubqueryExpr, IsNullExpr, PatternLikeExpr, UnaryOpExpr,
};
use sealdb_parser::opcode::Op;
use sealdb_parser::sqlparser_bridge::parse_expr;
use sealdb_types::Datum;
use sealdb_types::field_type::FieldType;
use sealdb_util::collate::{CollateMode, compare_bytes};
use std::cell::RefCell;

thread_local! {
    static COLLATION_MODE: RefCell<CollateMode> = const { RefCell::new(CollateMode::Bin) };
    static SQL_MODE: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn with_collation<T>(mode: CollateMode, f: impl FnOnce() -> T) -> T {
    COLLATION_MODE.with(|cell| {
        let prev = *cell.borrow();
        *cell.borrow_mut() = mode;
        let out = f();
        *cell.borrow_mut() = prev;
        out
    })
}

/// Run `f` under the session `sql_mode` (TiDB unsigned arithmetic flags).
pub fn with_sql_mode<T>(mode: impl Into<String>, f: impl FnOnce() -> T) -> T {
    SQL_MODE.with(|cell| {
        let prev = cell.borrow().clone();
        *cell.borrow_mut() = mode.into();
        let out = f();
        *cell.borrow_mut() = prev;
        out
    })
}

pub fn sql_mode_flag(flag: &str) -> bool {
    SQL_MODE.with(|cell| {
        cell.borrow()
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case(flag))
    })
}

thread_local! {
    static CURRENT_DB: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn with_current_db<T>(db: impl Into<String>, f: impl FnOnce() -> T) -> T {
    CURRENT_DB.with(|cell| {
        let prev = cell.borrow().clone();
        *cell.borrow_mut() = db.into();
        let out = f();
        *cell.borrow_mut() = prev;
        out
    })
}

fn current_db() -> String {
    CURRENT_DB.with(|cell| cell.borrow().clone())
}

/// Backward-compatible helper: `true` → GeneralCi, `false` → Bin.
pub fn with_collation_ci<T>(ci: bool, f: impl FnOnce() -> T) -> T {
    with_collation(
        if ci {
            CollateMode::GeneralCi
        } else {
            CollateMode::Bin
        },
        f,
    )
}

pub fn collation_mode() -> CollateMode {
    COLLATION_MODE.with(|cell| *cell.borrow())
}

pub fn collation_ci() -> bool {
    collation_mode().is_ci()
}

fn compare_datums(left: &Datum, right: &Datum) -> Result<std::cmp::Ordering> {
    if matches!((left, right), (Datum::Bytes(_), Datum::Bytes(_))) {
        let lb = left.as_bytes();
        let rb = right.as_bytes();
        let l_ok = std::str::from_utf8(&lb).is_ok();
        let r_ok = std::str::from_utf8(&rb).is_ok();
        // TiDB: comparing a utf8 string column with a binary literal that is
        // not valid utf8mb4 fails (select.test `a > 0x80`). Pure binary vs
        // binary stays byte-ordered.
        if l_ok != r_ok {
            if !l_ok {
                ensure_utf8mb4_convertible(&lb)?;
            }
            if !r_ok {
                ensure_utf8mb4_convertible(&rb)?;
            }
        }
        let mode = collation_mode();
        if mode != CollateMode::Bin {
            return Ok(compare_bytes(mode, &lb, &rb));
        }
    }
    Ok(left.compare(right))
}

/// Format invalid bytes like TiDB `formatInvalidChars` (`\x80`).
fn format_invalid_chars(src: &[u8]) -> String {
    let mut out = String::new();
    for (i, b) in src.iter().enumerate() {
        if i > 5 {
            out.push_str("...");
            break;
        }
        if *b > 0x7f {
            out.push_str(&format!("\\x{b:X}"));
        } else {
            out.push(*b as char);
        }
    }
    out
}

fn ensure_utf8mb4_convertible(bytes: &[u8]) -> Result<()> {
    if std::str::from_utf8(bytes).is_err() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!(
                "Cannot convert string '{}' from binary to utf8mb4",
                format_invalid_chars(bytes)
            ),
        ));
    }
    Ok(())
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
        ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_) => {
            Err(Error::unsupported("subquery evaluation outside Apply"))
        }
        ExprNode::Window(_) => Err(Error::unsupported("window evaluation outside WindowExec")),
    }
}

pub fn eval_filter(expr: &ExprNode, schema: &[String], row: &[Datum]) -> Result<bool> {
    Ok(truthy(&eval_expr_with_row(expr, schema, row)?).unwrap_or(false))
}

/// Reject binary literals that cannot convert to utf8mb4 when compared to a
/// column (TiDB charset convert), even if the scan yields no rows.
pub fn validate_string_cmp_literals(expr: &ExprNode) -> Result<()> {
    match expr {
        ExprNode::Binary(b) if matches!(b.op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) => {
            check_col_vs_bytes_literal(&b.left, &b.right)?;
            check_col_vs_bytes_literal(&b.right, &b.left)?;
            validate_string_cmp_literals(&b.left)?;
            validate_string_cmp_literals(&b.right)
        }
        ExprNode::Binary(b) => {
            validate_string_cmp_literals(&b.left)?;
            validate_string_cmp_literals(&b.right)
        }
        ExprNode::Unary(u) => validate_string_cmp_literals(&u.expr),
        ExprNode::Func(f) => {
            for arg in &f.args {
                validate_string_cmp_literals(arg)?;
            }
            Ok(())
        }
        ExprNode::InList(i) => {
            validate_string_cmp_literals(&i.expr)?;
            for item in &i.list {
                validate_string_cmp_literals(item)?;
            }
            Ok(())
        }
        ExprNode::Between(b) => {
            validate_string_cmp_literals(&b.expr)?;
            validate_string_cmp_literals(&b.low)?;
            validate_string_cmp_literals(&b.high)
        }
        ExprNode::Like(l) => {
            validate_string_cmp_literals(&l.expr)?;
            validate_string_cmp_literals(&l.pattern)
        }
        ExprNode::IsNull(i) => validate_string_cmp_literals(&i.expr),
        ExprNode::Cast(c) => validate_string_cmp_literals(&c.expr),
        _ => Ok(()),
    }
}

fn check_col_vs_bytes_literal(col_side: &ExprNode, lit_side: &ExprNode) -> Result<()> {
    if !matches!(col_side, ExprNode::Column(_)) {
        return Ok(());
    }
    let ExprNode::Value(v) = lit_side else {
        return Ok(());
    };
    if let Datum::Bytes(b) = &v.datum {
        ensure_utf8mb4_convertible(b)?;
    }
    Ok(())
}

/// Locate the physical partition id for `values`, evaluating expression keys.
pub fn locate_partition_for_row(info: &TableInfo, values: &[Datum]) -> Result<i64> {
    match &info.partition {
        Some(part) if part.enable && part.is_expr_key() => {
            let expr = parse_expr(&part.expr)?;
            let schema: Vec<String> = info
                .columns
                .iter()
                .map(|c| c.name.o().to_string())
                .collect();
            let datum = eval_expr_with_row(&expr, &schema, values)?;
            part.locate(&datum)
        }
        _ => info.locate_partition_row(values),
    }
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
    if matches!(expr.op, Op::LogicOr) && sql_mode_flag("PIPES_AS_CONCAT") {
        return Ok(Datum::Bytes(
            [left.as_bytes().as_slice(), right.as_bytes().as_slice()].concat(),
        ));
    }
    if matches!(expr.op, Op::LogicAnd | Op::LogicOr) {
        return eval_logic(expr.op, &left, &right);
    }
    if left.is_null() || right.is_null() {
        return Ok(Datum::Null);
    }
    match expr.op {
        Op::Plus => num_bin(&left, &right, |a, b| Ok(a + b), |a, b| Ok(a + b)),
        Op::Minus => {
            // TiDB: either operand UNSIGNED → unsigned result unless
            // sql_mode NO_UNSIGNED_SUBTRACTION (then signed).
            if !sql_mode_flag("NO_UNSIGNED_SUBTRACTION")
                && (matches!(left, Datum::U64(_)) || matches!(right, Datum::U64(_)))
            {
                unsigned_minus(&left, &right)
            } else {
                // Signed BIGINT result (also under NO_UNSIGNED_SUBTRACTION).
                // Use i128 so U64 magnitudes stay positive — wrapping U64→i64
                // wrongly makes CAST(-1 UNSIGNED)-MAX == i64::MIN (in range).
                if matches!(left, Datum::F64(_)) || matches!(right, Datum::F64(_)) {
                    num_bin(&left, &right, |a, b| Ok(a - b), |a, b| Ok(a - b))
                } else {
                    let li = datum_as_i128(&left)?;
                    let ri = datum_as_i128(&right)?;
                    let diff = li - ri;
                    if diff < i64::MIN as i128 || diff > i64::MAX as i128 {
                        let label = format!(
                            "({} - {})",
                            minus_operand_label(&expr.left, &left),
                            minus_operand_label(&expr.right, &right)
                        );
                        Err(Error::new(
                            ERR_DATA_OUT_OF_RANGE,
                            format!("BIGINT value is out of range in '{label}'"),
                        ))
                    } else {
                        Ok(Datum::I64(diff as i64))
                    }
                }
            }
        }
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
        Op::Eq => Ok(bool_datum(compare_datums(&left, &right)?.is_eq())),
        Op::Ne => Ok(bool_datum(!compare_datums(&left, &right)?.is_eq())),
        Op::Lt => Ok(bool_datum(compare_datums(&left, &right)?.is_lt())),
        Op::Le => Ok(bool_datum(compare_datums(&left, &right)?.is_le())),
        Op::Gt => Ok(bool_datum(compare_datums(&left, &right)?.is_gt())),
        Op::Ge => Ok(bool_datum(compare_datums(&left, &right)?.is_ge())),
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
    // sqlparser always binds NOT below BETWEEN/IN/LIKE. Under HIGH_NOT_PRECEDENCE
    // TiDB binds NOT tighter: `NOT x BETWEEN a AND b` → `(NOT x) BETWEEN a AND b`.
    if matches!(expr.op, Op::Not | Op::Not2) && sql_mode_flag("HIGH_NOT_PRECEDENCE") {
        if let Some(rewritten) = rewrite_high_not_precedence(expr.op, &expr.expr) {
            return eval_expr_with_row(&rewritten, schema, row);
        }
    }
    let inner = eval_expr_with_row(&expr.expr, schema, row)?;
    if inner.is_null() {
        return Ok(Datum::Null);
    }
    match expr.op {
        Op::UnaryMinus | Op::Minus => match inner {
            Datum::I64(v) => v.checked_neg().map(Datum::I64).ok_or_else(|| {
                Error::new(
                    ERR_DATA_OUT_OF_RANGE,
                    format!("BIGINT value is out of range in '(-{v})'"),
                )
            }),
            // `-9223372036854775808` parses as UnaryMinus(U64(2^63)).
            Datum::U64(v) if v == 1u64 << 63 => Ok(Datum::I64(i64::MIN)),
            Datum::U64(v) if v <= i64::MAX as u64 => Ok(Datum::I64(-(v as i64))),
            Datum::U64(v) => Err(Error::new(
                ERR_DATA_OUT_OF_RANGE,
                format!("BIGINT UNSIGNED value is out of range in '(-{v})'"),
            )),
            Datum::F64(v) => Ok(Datum::F64(-v)),
            other => Ok(Datum::I64(-other.as_i64()?)),
        },
        Op::Not | Op::Not2 => Ok(bool_datum(!truthy(&inner).unwrap_or(false))),
        Op::Plus => Ok(inner),
        _ => Err(Error::unsupported(format!("unary {}", expr.op.restore()))),
    }
}

fn rewrite_high_not_precedence(op: Op, inner: &ExprNode) -> Option<ExprNode> {
    let not_inner = |e: ExprNode| {
        ExprNode::Unary(UnaryOpExpr {
            op,
            expr: Box::new(e),
        })
    };
    match inner {
        ExprNode::Between(b) if !b.not => Some(ExprNode::Between(BetweenExpr {
            expr: Box::new(not_inner((*b.expr).clone())),
            not: false,
            low: b.low.clone(),
            high: b.high.clone(),
        })),
        ExprNode::Like(l) if !l.not => Some(ExprNode::Like(PatternLikeExpr {
            expr: Box::new(not_inner((*l.expr).clone())),
            not: false,
            pattern: l.pattern.clone(),
            ci: l.ci,
        })),
        ExprNode::InList(i) if !i.not => Some(ExprNode::InList(InListExpr {
            expr: Box::new(not_inner((*i.expr).clone())),
            not: false,
            list: i.list.clone(),
        })),
        ExprNode::InSubquery(i) if !i.not => Some(ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(not_inner((*i.expr).clone())),
            not: false,
            query: i.query.clone(),
        })),
        ExprNode::IsNull(i) if !i.not => Some(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(not_inner((*i.expr).clone())),
            not: false,
        })),
        _ => None,
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
        "COALESCE" => {
            if func.args.is_empty() {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "COALESCE requires at least 1 argument",
                ));
            }
            for arg in &func.args {
                let v = eval_expr_with_row(arg, schema, row)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Datum::Null)
        }
        "ISNULL" => {
            if func.args.len() != 1 {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "ISNULL requires 1 argument",
                ));
            }
            let v = eval_expr_with_row(&func.args[0], schema, row)?;
            Ok(bool_datum(v.is_null()))
        }
        "GREATEST" | "LEAST" => {
            if func.args.len() < 2 {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    format!("{} requires at least 2 arguments", func.name),
                ));
            }
            let want_greatest = func.name.eq_ignore_ascii_case("GREATEST");
            let mut best: Option<Datum> = None;
            for arg in &func.args {
                let v = eval_expr_with_row(arg, schema, row)?;
                if v.is_null() {
                    return Ok(Datum::Null);
                }
                best = Some(match best {
                    None => v,
                    Some(cur) => {
                        let ord = compare_datums(&cur, &v)?;
                        if want_greatest {
                            if ord.is_lt() { v } else { cur }
                        } else if ord.is_gt() {
                            v
                        } else {
                            cur
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Datum::Null))
        }
        "ABS" => {
            if func.args.len() != 1 {
                return Err(Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "ABS requires 1 argument",
                ));
            }
            let value = eval_expr_with_row(&func.args[0], schema, row)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            match value {
                Datum::I64(v) => Ok(Datum::I64(v.saturating_abs())),
                Datum::U64(v) => Ok(Datum::U64(v)),
                Datum::F64(v) => Ok(Datum::F64(v.abs())),
                other => Ok(Datum::I64(other.as_i64()?.saturating_abs())),
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
        if compare_datums(&left, &value)?.is_eq() {
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
    let inside =
        compare_datums(&value, &low)?.is_ge() && compare_datums(&value, &high)?.is_le();
    Ok(bool_datum(if expr.not { !inside } else { inside }))
}

fn eval_like(expr: &PatternLikeExpr, schema: &[String], row: &[Datum]) -> Result<Datum> {
    let value = eval_expr_with_row(&expr.expr, schema, row)?;
    let pattern = eval_expr_with_row(&expr.pattern, schema, row)?;
    if value.is_null() || pattern.is_null() {
        return Ok(Datum::Null);
    }
    let ci = expr.ci || collation_ci();
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

fn datum_as_i128(d: &Datum) -> Result<i128> {
    match d {
        Datum::I64(v) => Ok(*v as i128),
        Datum::U64(v) => Ok(*v as i128),
        other => Ok(other.as_i64()? as i128),
    }
}

fn datum_decimal_label(d: &Datum) -> String {
    match d {
        Datum::I64(v) => v.to_string(),
        Datum::U64(v) => v.to_string(),
        other => other
            .as_i64()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| format!("{other:?}")),
    }
}

fn minus_operand_label(expr: &ExprNode, value: &Datum) -> String {
    match expr {
        ExprNode::Column(_) => expr_overflow_label(expr),
        _ => datum_decimal_label(value),
    }
}

fn datum_as_u64(d: &Datum) -> Result<u64> {
    match d {
        Datum::U64(v) => Ok(*v),
        Datum::I64(v) if *v >= 0 => Ok(*v as u64),
        Datum::I64(_) => Err(Error::new(
            ERR_DATA_OUT_OF_RANGE,
            "BIGINT UNSIGNED value is out of range",
        )),
        other => {
            let v = other.as_i64()?;
            if v < 0 {
                return Err(Error::new(
                    ERR_DATA_OUT_OF_RANGE,
                    "BIGINT UNSIGNED value is out of range",
                ));
            }
            Ok(v as u64)
        }
    }
}

fn unsigned_minus(left: &Datum, right: &Datum) -> Result<Datum> {
    let l = datum_as_u64(left)?;
    let r = datum_as_u64(right)?;
    if l < r {
        return Err(Error::new(
            ERR_DATA_OUT_OF_RANGE,
            format!("BIGINT UNSIGNED value is out of range in '({l} - {r})'"),
        ));
    }
    Ok(Datum::U64(l - r))
}

fn expr_overflow_label(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(c) => match (&c.schema, &c.table) {
            (Some(schema), Some(table)) => format!("{schema}.{table}.{}", c.name),
            (None, Some(table)) => {
                let db = current_db();
                if db.is_empty() {
                    format!("{table}.{}", c.name)
                } else {
                    format!("{db}.{table}.{}", c.name)
                }
            }
            _ => c.name.clone(),
        },
        other => other.restore_string(),
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

#[cfg(test)]
mod hex_cmp_tests {
    use super::*;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};
    use sealdb_parser::opcode::Op;

    #[test]
    fn char_gt_hex_errors() {
        let schema = vec!["a".into()];
        let row = vec![Datum::Bytes(b"a".to_vec())];
        let expr = ExprNode::Binary(BinaryOpExpr {
            op: Op::Gt,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "a".into(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(vec![0x80]),
            })),
        });
        let err = eval_expr_with_row(&expr, &schema, &row).unwrap_err();
        assert_eq!(err.code, ERR_UNKNOWN_ERROR);
        assert!(err.message.contains("Cannot convert string"), "{}", err.message);
    }
}
#[cfg(test)]
mod unsigned_sub_tests {
    use super::*;
    use sealdb_parser::ast::*;
    use sealdb_parser::Parser;

    fn select_expr(sql: &str) -> ExprNode {
        let stmt = Parser::new().parse_one_stmt(sql).unwrap();
        match stmt {
            Statement::Select(s) => s.fields[0].expr.clone(),
            _ => panic!("expected select"),
        }
    }

    #[test]
    fn minus_unsigned_default_underflow() {
        let expr = select_expr("SELECT 12 - cast(15 as unsigned)");
        with_sql_mode("", || {
            let err = eval_expr_with_row(&expr, &[], &[]).unwrap_err();
            assert_eq!(err.code, ERR_DATA_OUT_OF_RANGE, "{err}");
            assert!(
                err.message.contains("BIGINT UNSIGNED"),
                "{}",
                err.message
            );
        });
        with_sql_mode("NO_UNSIGNED_SUBTRACTION", || {
            assert_eq!(
                eval_expr_with_row(&expr, &[], &[]).unwrap(),
                Datum::I64(-3)
            );
        });
    }

    #[test]
    fn minus_no_unsigned_subtraction_overflow() {
        let expr = select_expr(
            "SELECT CAST(-1 as UNSIGNED) - cast(9223372036854775807 as unsigned)",
        );
        with_sql_mode("NO_UNSIGNED_SUBTRACTION", || {
            let err = eval_expr_with_row(&expr, &[], &[]).unwrap_err();
            assert_eq!(err.code, ERR_DATA_OUT_OF_RANGE, "{err}");
            assert!(
                err.message.contains("18446744073709551615"),
                "{}",
                err.message
            );
            assert!(
                err.message.contains("9223372036854775807"),
                "{}",
                err.message
            );
        });
    }

    #[test]
    fn minus_no_unsigned_subtraction_ok() {
        let expr = select_expr(
            "SELECT CAST(-1 as UNSIGNED) - cast(-9223372036854775808 as unsigned)",
        );
        with_sql_mode("NO_UNSIGNED_SUBTRACTION", || {
            assert_eq!(
                eval_expr_with_row(&expr, &[], &[]).unwrap(),
                Datum::I64(i64::MAX)
            );
        });
    }
}

#[cfg(test)]
mod high_not_tests {
    use super::*;
    use sealdb_parser::Parser;
    use sealdb_parser::ast::*;

    fn eval_select(sql: &str) -> Datum {
        let stmt = Parser::new().parse_one_stmt(sql).unwrap();
        let Statement::Select(s) = stmt else { panic!() };
        eval_expr_with_row(&s.fields[0].expr, &[], &[]).unwrap()
    }

    #[test]
    fn not_between_default_and_high() {
        with_sql_mode("", || {
            assert_eq!(eval_select("SELECT NOT 1 BETWEEN -5 AND 5"), Datum::I64(0));
        });
        with_sql_mode("HIGH_NOT_PRECEDENCE", || {
            assert_eq!(eval_select("SELECT NOT 1 BETWEEN -5 AND 5"), Datum::I64(1));
        });
    }
}

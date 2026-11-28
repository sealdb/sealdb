use sealdb_errno::{
    ERR_AFFINITY, ERR_BAD_FIELD, ERR_BLOB_CANT_HAVE_DEFAULT, ERR_CANNOT_ADD_FOREIGN,
    ERR_DUP_FIELDNAME, ERR_DUP_KEYNAME, ERR_DUPLICATED_VALUE_IN_TYPE, ERR_FK_DUP_NAME,
    ERR_GENERATED_COLUMN_FUNCTION_NOT_ALLOWED, ERR_GENERATED_COLUMN_REF_AUTO_INC,
    ERR_INVALID_AUTO_RANDOM, ERR_INVALID_DEFAULT, ERR_INVALID_ON_UPDATE,
    ERR_KEY_COLUMN_DOES_NOT_EXIST, ERR_M_BIGGER_THAN_D, ERR_MULTIPLE_PRI_KEY, ERR_NO_SUCH_TABLE,
    ERR_OPT_ON_TEMPORARY_TABLE, ERR_PRIMARY_CANT_HAVE_NULL, ERR_TABLE_MUST_HAVE_COLUMNS,
    ERR_TOO_BIG_DISPLAYWIDTH, ERR_TOO_BIG_PRECISION, ERR_TOO_MANY_FIELDS, ERR_TOO_MANY_KEYS,
    ERR_TTL_INVALID_COLUMN, ERR_TTL_UNSUPPORTED_PK_TYPE, ERR_UNKNOWN_ERROR,
    ERR_UNSUPPORTED_DDL_OPERATION, ERR_VIEW_WRONG_LIST, ERR_WARN_TABLE_TYPE_NOT_SUPPORT_FULLTEXT,
    ERR_WRONG_AUTO_KEY, ERR_WRONG_COLUMN_NAME, ERR_WRONG_DB_NAME, ERR_WRONG_FK_DEF,
    ERR_WRONG_NAME_FOR_INDEX, ERR_WRONG_PARAMCOUNT_TO_NATIVE_FCT, ERR_WRONG_TABLE_NAME, Error,
    Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, Mutator, TableInfo};
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, ValueExpr};
use sealdb_parser::ast::{ColumnDef, CreateTableStmt, ForeignKeyDef, IndexPart, SelectStmt};
use sealdb_parser::model::CIStr;
use sealdb_types::Datum;
use sealdb_types::EvalType;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE,
    TYPE_DURATION, TYPE_ENUM, TYPE_FLOAT, TYPE_INT24, TYPE_JSON, TYPE_LONG, TYPE_LONG_BLOB,
    TYPE_LONG_LONG, TYPE_MEDIUM_BLOB, TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING,
    TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB, TYPE_VAR_STRING, TYPE_YEAR,
};
use std::collections::HashMap;

/// Functions TiDB refuses inside a generated-column expression (3102).
const DISALLOWED_GENERATED_FUNCS: &[&str] = &[
    "rand",
    "load_file",
    "curdate",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "now",
    "sysdate",
    "uuid",
    "uuid_short",
    "is_free_lock",
    "is_used_lock",
    "release_lock",
    "get_lock",
    "database",
    "schema",
    "connection_id",
    "last_insert_id",
    "found_rows",
    "row_count",
    "current_user",
    "session_user",
    "system_user",
    "user",
    "version",
];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// TiDB stores a function-call default as an expression default and prints it
/// parenthesized in lowercase (`DEFAULT UUID()` → `DEFAULT (uuid())`).
pub(crate) fn normalize_expression_default(default: &str) -> String {
    let trimmed = default.trim();
    // Literals (quoted, numeric, hex/bit) keep their spelling even when their
    // text contains parentheses.
    let starts_like_function = trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic());
    if !starts_like_function || !trimmed.contains('(') {
        return default.to_string();
    }
    // Temporal function defaults stay bare in SHOW CREATE (`DEFAULT
    // CURRENT_TIMESTAMP`), and their fractional-precision validation (1067)
    // reads the original spelling.
    let head: String = trimmed
        .chars()
        .take_while(|c| *c != '(')
        .collect::<String>()
        .trim()
        .to_ascii_lowercase();
    if matches!(
        head.as_str(),
        "now"
            | "current_timestamp"
            | "localtime"
            | "localtimestamp"
            | "curdate"
            | "current_date"
            | "curtime"
            | "current_time"
            | "utc_date"
            | "utc_time"
            | "utc_timestamp"
            | "sysdate"
    ) {
        return default.to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with('(') {
        lower
    } else {
        format!("({lower})")
    }
}

/// Functions TiDB refuses inside a `DEFAULT (…)` expression (1674): the value
/// would differ on a replica, so it cannot be logged as a statement.
/// Temporal functions are deliberately absent: `DEFAULT now(…)` is a legal
/// timestamp default validated separately (fractional precision → 1067).
const UNSAFE_DEFAULT_FUNCS: &[&str] = &[
    "uuid",
    "uuid_short",
    "rand",
    "connection_id",
    "last_insert_id",
    "current_user",
    "session_user",
    "system_user",
    "user",
    "version",
    "database",
    "schema",
    "found_rows",
    "row_count",
];

/// `ALTER TABLE … ADD COLUMN … DEFAULT …` rejects unsafe functions in either
/// spelling: TiDB validates every added-column default, parenthesized or not.
/// `CREATE TABLE` deliberately does not: `DEFAULT (uuid())` is legal there and
/// generates a fresh value per inserted row.
pub(crate) fn validate_default_expr_any(default: &str) -> Result<()> {
    let lower = default.to_ascii_lowercase();
    for func in UNSAFE_DEFAULT_FUNCS {
        if calls_function(&lower, func) {
            return Err(Error::new(
                1674,
                "Statement is unsafe because it uses a system function that may \
                 return a different value on the slave",
            ));
        }
    }
    Ok(())
}

/// MySQL stores `TEXT` as a `BLOB` code with a non-binary charset; SealDB keeps
/// it as a length-less `VAR_STRING`, so both spellings count as LOB here.
pub(crate) fn is_lob_type(field_type: &FieldType) -> bool {
    matches!(
        field_type.tp,
        TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB | TYPE_JSON
    ) || (field_type.tp == TYPE_VAR_STRING && field_type.flen < 0 && field_type.charset != "binary")
}

/// MySQL/TiDB: a literal default on a BLOB/TEXT/JSON column is error 1101 in
/// strict mode; outside strict mode the default is dropped (`JSON` keeps a
/// textual `'null'` default instead). `DEFAULT (…)` expression defaults and
/// `DEFAULT NULL` are always allowed.
pub(crate) fn normalize_lob_default(
    name: &str,
    field_type: &FieldType,
    default: Option<String>,
) -> Result<Option<String>> {
    let Some(default) = default else {
        return Ok(None);
    };
    if !is_lob_type(field_type) {
        return Ok(Some(default));
    }
    let trimmed = default.trim();
    if trimmed.eq_ignore_ascii_case("null") || trimmed.starts_with('(') {
        return Ok(Some(default));
    }
    let strict = sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
        || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES");
    if strict {
        return Err(Error::new(
            ERR_BLOB_CANT_HAVE_DEFAULT,
            format!("BLOB/TEXT/JSON column '{name}' can't have a default value"),
        ));
    }
    if field_type.tp == TYPE_JSON {
        return Ok(Some("'null'".to_string()));
    }
    Ok(None)
}

/// Does `expr` (already lowercased) call `name(` as a standalone identifier?
fn calls_function(expr: &str, name: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut start = 0;
    while let Some(offset) = expr[start..].find(name) {
        let i = start + offset;
        let prev_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
        let mut j = i + name.len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if prev_ok && j < bytes.len() && bytes[j] == b'(' {
            return true;
        }
        start = i + name.len();
    }
    false
}

/// TiDB rejects non-deterministic / environment functions in generated columns
/// (`GENERATED ALWAYS AS (rand())` → 3102).
pub(crate) fn validate_generated_expr(col_name: &str, expr: &str) -> Result<()> {
    let lower = expr.to_ascii_lowercase();
    for func in DISALLOWED_GENERATED_FUNCS {
        if calls_function(&lower, func) {
            return Err(Error::new(
                ERR_GENERATED_COLUMN_FUNCTION_NOT_ALLOWED,
                format!(
                    "Expression of generated column '{col_name}' contains \
                     a disallowed function."
                ),
            ));
        }
    }
    // A builtin called with the wrong parameter count (e.g. `lower(a, 2)`)
    // fails while the expression is built, exactly as in TiDB (1582).
    if let Ok(parsed) = sealdb_parser::sqlparser_bridge::lower::parse_expr(expr) {
        // TiDB `checkIllegalFn4Generated`: grouping() and every aggregate
        // function are rejected with 1111 before other ALTER checks run.
        if expr_has_aggregate(&parsed) {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_GROUP_FUNC_USE,
                "Invalid use of group function",
            ));
        }
        check_builtin_arg_counts(&parsed)?;
    }
    Ok(())
}

/// Walk `expr` and validate every builtin call's parameter count (1582).
fn check_builtin_arg_counts(expr: &ExprNode) -> Result<()> {
    match expr {
        ExprNode::Value(_) | ExprNode::Column(_) => {}
        ExprNode::Func(func) => {
            if let Err((_min, _max)) =
                sealdb_expression::check_arg_count(&func.name, func.args.len())
            {
                return Err(Error::new(
                    ERR_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
                    format!(
                        "Incorrect parameter count in the call to native function '{}'",
                        func.name
                    ),
                ));
            }
            for arg in &func.args {
                check_builtin_arg_counts(arg)?;
            }
        }
        ExprNode::Binary(binary) => {
            check_builtin_arg_counts(&binary.left)?;
            check_builtin_arg_counts(&binary.right)?;
        }
        ExprNode::Unary(unary) => check_builtin_arg_counts(&unary.expr)?,
        ExprNode::Cast(cast) => check_builtin_arg_counts(&cast.expr)?,
        ExprNode::IsNull(is_null) => check_builtin_arg_counts(&is_null.expr)?,
        ExprNode::InList(in_list) => {
            check_builtin_arg_counts(&in_list.expr)?;
            for item in &in_list.list {
                check_builtin_arg_counts(item)?;
            }
        }
        ExprNode::Between(between) => {
            check_builtin_arg_counts(&between.expr)?;
            check_builtin_arg_counts(&between.low)?;
            check_builtin_arg_counts(&between.high)?;
        }
        ExprNode::Like(like) => {
            check_builtin_arg_counts(&like.expr)?;
            check_builtin_arg_counts(&like.pattern)?;
        }
        ExprNode::Case(case) => {
            if let Some(operand) = &case.operand {
                check_builtin_arg_counts(operand)?;
            }
            for (when, then) in &case.when_then {
                check_builtin_arg_counts(when)?;
                check_builtin_arg_counts(then)?;
            }
            if let Some(else_expr) = &case.else_expr {
                check_builtin_arg_counts(else_expr)?;
            }
        }
        ExprNode::Row(row) => {
            for item in &row.exprs {
                check_builtin_arg_counts(item)?;
            }
        }
        ExprNode::Window(_)
        | ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_) => {}
    }
    Ok(())
}

/// Does `expr` reference `ident` as a column (i.e. not followed by `(`)?
pub(crate) fn references_identifier(expr: &str, ident: &str) -> bool {
    let lower = expr.to_ascii_lowercase();
    let ident = ident.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(offset) = lower[start..].find(&ident) {
        let i = start + offset;
        let prev_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
        let end = i + ident.len();
        let next_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if prev_ok && next_ok {
            let mut j = end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= bytes.len() || bytes[j] != b'(' {
                return true;
            }
        }
        start = i + ident.len();
    }
    false
}

/// Every column referenced by a generated-column expression must exist (1054).
pub(crate) fn validate_generated_column_refs(
    columns: &[ColumnInfo],
    changed: Option<&str>,
) -> Result<()> {
    let known: Vec<String> = columns
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| c.name.o().to_string())
        .collect();
    for col in columns {
        if let Some(changed) = changed
            && !col.name.o().eq_ignore_ascii_case(changed)
        {
            continue;
        }
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        // TiDB `checkIllegalFn4Generated`: grouping() and every aggregate
        // function are rejected with 1111.
        if expr_has_aggregate(&expr) {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_GROUP_FUNC_USE,
                "Invalid use of group function",
            ));
        }
        for (_table, name) in expr.referenced_columns() {
            if !known.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
                // TiDB reports the lower-cased identifier (`ColumnName.L`).
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!(
                        "Unknown column '{}' in 'generated column function'",
                        name.to_ascii_lowercase()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// `true` when `expr` calls `grouping()` or an aggregate function anywhere in
/// the tree (TiDB `illegalFunctionChecker.hasAggFunc`).
fn expr_has_aggregate(expr: &ExprNode) -> bool {
    fn is_agg_name(name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "grouping"
                | "count"
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
                | "variance"
                | "var_pop"
                | "var_samp"
                | "json_arrayagg"
                | "json_objectagg"
                | "any_value"
                | "approx_count_distinct"
        )
    }
    match expr {
        ExprNode::Func(f) => is_agg_name(&f.name) || f.args.iter().any(expr_has_aggregate),
        ExprNode::Binary(b) => expr_has_aggregate(&b.left) || expr_has_aggregate(&b.right),
        ExprNode::Unary(u) => expr_has_aggregate(&u.expr),
        ExprNode::Cast(c) => expr_has_aggregate(&c.expr),
        ExprNode::InList(i) => expr_has_aggregate(&i.expr) || i.list.iter().any(expr_has_aggregate),
        ExprNode::Between(b) => {
            expr_has_aggregate(&b.expr) || expr_has_aggregate(&b.low) || expr_has_aggregate(&b.high)
        }
        ExprNode::Like(l) => expr_has_aggregate(&l.expr) || expr_has_aggregate(&l.pattern),
        ExprNode::IsNull(i) => expr_has_aggregate(&i.expr),
        ExprNode::Case(c) => {
            c.operand.as_deref().is_some_and(expr_has_aggregate)
                || c.when_then
                    .iter()
                    .any(|(when, then)| expr_has_aggregate(when) || expr_has_aggregate(then))
                || c.else_expr.as_deref().is_some_and(expr_has_aggregate)
        }
        ExprNode::Row(r) => r.exprs.iter().any(expr_has_aggregate),
        ExprNode::Window(w) => w.args.iter().any(expr_has_aggregate),
        ExprNode::InSubquery(i) => expr_has_aggregate(&i.expr),
        ExprNode::QuantifiedCmp(q) => expr_has_aggregate(&q.expr),
        ExprNode::Value(_)
        | ExprNode::Column(_)
        | ExprNode::Exists(_)
        | ExprNode::ScalarSubquery(_) => false,
    }
}

/// A generated column may not reference an AUTO_INCREMENT column unless
/// `@@tidb_enable_auto_increment_in_generated` is enabled (3109).
pub(crate) fn validate_generated_auto_increment(
    columns: &[ColumnInfo],
    changed: Option<&str>,
) -> Result<()> {
    if sealdb_expression::allow_auto_increment_in_generated() {
        return Ok(());
    }
    let auto_inc: Vec<String> = columns
        .iter()
        .filter(|c| c.auto_increment)
        .map(|c| c.name.o().to_string())
        .collect();
    if auto_inc.is_empty() {
        return Ok(());
    }
    for col in columns {
        if let Some(changed) = changed
            && !col.name.o().eq_ignore_ascii_case(changed)
        {
            continue;
        }
        let Some(expr) = col.generated_expr.as_deref() else {
            continue;
        };
        for ai in &auto_inc {
            if references_identifier(expr, ai) {
                return Err(Error::new(
                    ERR_GENERATED_COLUMN_REF_AUTO_INC,
                    format!(
                        "Generated column '{}' cannot refer to auto-increment column.",
                        col.name.o()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// TiDB `ColumnOptionOnUpdate`: `ON UPDATE` is only valid on TIMESTAMP /
/// DATETIME columns and only with `CURRENT_TIMESTAMP`, whose fsp must equal the
/// column's (1294).
pub(crate) fn validate_on_update(
    name: &str,
    field_type: &FieldType,
    on_update: Option<&str>,
) -> Result<()> {
    let Some(expr) = on_update else {
        return Ok(());
    };
    let invalid = || {
        Error::new(
            ERR_INVALID_ON_UPDATE,
            format!("Invalid ON UPDATE clause for '{name}' column"),
        )
    };
    if !matches!(field_type.tp, TYPE_TIMESTAMP | TYPE_DATETIME) {
        return Err(invalid());
    }
    let lower = expr.trim().to_ascii_lowercase();
    let Some(func) = ["current_timestamp", "now"]
        .iter()
        .find(|func| lower.starts_with(**func))
    else {
        return Err(invalid());
    };
    let rest = lower[func.len()..].trim();
    let fsp = if rest.is_empty() {
        0
    } else if let Some(inner) = rest
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let inner = inner.trim();
        if inner.is_empty() {
            0
        } else {
            match inner.parse::<i32>() {
                Ok(n) => n,
                Err(_) => return Err(invalid()),
            }
        }
    } else {
        return Err(invalid());
    };
    if fsp != field_type.decimal {
        return Err(invalid());
    }
    Ok(())
}

/// TiDB `checkDefaultValue` for temporal columns: a `NOW(fsp)` /
/// `CURRENT_TIMESTAMP(fsp)` default must match the column's fractional-second
/// precision exactly (`create table t (tt timestamp default now(1))` → 1067).
pub(crate) fn validate_default_fsp(
    name: &str,
    field_type: &FieldType,
    default: Option<&str>,
) -> Result<()> {
    if !matches!(field_type.tp, TYPE_TIMESTAMP | TYPE_DATETIME) {
        return Ok(());
    }
    let Some(default) = default else {
        return Ok(());
    };
    let lower = default.trim().to_ascii_lowercase();
    // Longest first: `localtime` is a prefix of `localtimestamp`.
    let Some(func) = ["current_timestamp", "localtimestamp", "localtime", "now"]
        .iter()
        .find(|func| lower.starts_with(**func))
    else {
        return Ok(());
    };
    let rest = lower[func.len()..].trim();
    let fsp = if rest.is_empty() {
        0
    } else if let Some(inner) = rest
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let inner = inner.trim();
        if inner.is_empty() {
            0
        } else {
            match inner.parse::<i32>() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            }
        }
    } else {
        return Ok(());
    };
    if fsp != field_type.decimal {
        return Err(Error::new(
            ERR_INVALID_DEFAULT,
            format!("Invalid default value for '{name}'"),
        ));
    }
    Ok(())
}

/// Bounds of an integer column type (`types.TinyInt` … `types.Longlong`),
/// honouring the UNSIGNED flag.
fn integer_bounds(field_type: &FieldType) -> Option<(i128, i128)> {
    let unsigned = field_type.is_unsigned();
    let (signed_max, unsigned_max) = match field_type.tp {
        TYPE_TINY => (i128::from(i8::MAX), i128::from(u8::MAX)),
        TYPE_SHORT => (i128::from(i16::MAX), i128::from(u16::MAX)),
        TYPE_INT24 => (i128::from(8_388_607i32), i128::from(16_777_215u32)),
        TYPE_LONG => (i128::from(i32::MAX), i128::from(u32::MAX)),
        TYPE_LONG_LONG => (i128::from(i64::MAX), i128::from(u64::MAX)),
        _ => return None,
    };
    if unsigned {
        Some((0, unsigned_max))
    } else {
        Some((-signed_max - 1, signed_max))
    }
}

/// Digits of a MySQL bit literal (`b'0110'` / `0b0110`).
fn bit_literal_bits(text: &str) -> Option<&str> {
    let rest = if text.len() >= 3 && (text.starts_with("b'") || text.starts_with("B'")) {
        text.get(2..text.len().saturating_sub(1))?
    } else {
        text.strip_prefix("0b")
            .or_else(|| text.strip_prefix("0B"))?
    };
    if (text.starts_with("b'") || text.starts_with("B'")) && !text.ends_with('\'') {
        return None;
    }
    if rest.is_empty() || !rest.bytes().all(|b| b == b'0' || b == b'1') {
        return None;
    }
    Some(rest)
}

/// TiDB `types.CheckDisplayWidth`: `BIT(n)` only allows `1 <= n <= 64`.
pub(crate) fn validate_display_width(name: &str, field_type: &FieldType) -> Result<()> {
    if field_type.tp == TYPE_BIT && (field_type.flen < 1 || field_type.flen > 64) {
        return Err(Error::new(
            ERR_TOO_BIG_DISPLAYWIDTH,
            format!("Display width out of range for column '{name}' (max = 64)"),
        ));
    }
    Ok(())
}

/// TiDB `checkColumnAttributes`: `FLOAT/DOUBLE/DECIMAL(M,D)` requires `M >= D`
/// (1427), and a temporal fsp above 6 is 1426.
pub(crate) fn check_column_attributes(name: &str, field_type: &FieldType) -> Result<()> {
    match field_type.tp {
        TYPE_NEW_DECIMAL | TYPE_DECIMAL | TYPE_DOUBLE | TYPE_FLOAT
            if field_type.flen < field_type.decimal =>
        {
            Err(Error::new(
                ERR_M_BIGGER_THAN_D,
                format!(
                    "For float(M,D), double(M,D) or decimal(M,D), M must be >= D (column '{name}')."
                ),
            ))
        }
        TYPE_DATETIME | TYPE_DURATION | TYPE_TIMESTAMP if field_type.decimal > 6 => {
            Err(Error::new(
                ERR_TOO_BIG_PRECISION,
                format!(
                    "Too big precision {} specified for column '{name}'. Maximum is 6.",
                    field_type.decimal
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// TiDB `checkColumnDefaultValue`: `NO_ZERO_DATE` together with strict mode
/// makes a temporal default that evaluates to `0000-00-00 00:00:00` invalid
/// (1067). Outside strict mode the mode is advisory and the default is kept.
fn is_zero_temporal_default(field_type: &FieldType, default: &str) -> bool {
    if !matches!(field_type.tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP)
        || !sealdb_expression::sql_mode_flag("NO_ZERO_DATE")
        || !(sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
            || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES"))
    {
        return false;
    }
    let text = default.trim();
    let text = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(text)
        .trim();
    if text.is_empty() {
        return false;
    }
    // A bare numeric zero (`DEFAULT 0`) evaluates to the zero time.
    if text.bytes().all(|b| matches!(b, b'0' | b'.' | b'+' | b'-')) {
        return true;
    }
    let time = sealdb_types::parse_datetime_literal(text)
        .or_else(|_| sealdb_types::parse_datetime_literal(&format!("{text} 00:00:00")))
        .or_else(|_| sealdb_types::parse_date_literal(text));
    match time {
        Ok(t) => {
            t.year == 0
                && t.month == 0
                && t.day == 0
                && t.hour == 0
                && t.minute == 0
                && t.second == 0
                && t.micro == 0
        }
        Err(_) => false,
    }
}

fn is_decimal_type(field_type: &FieldType) -> bool {
    matches!(field_type.tp, TYPE_NEW_DECIMAL | TYPE_DECIMAL)
}

/// A `DECIMAL(precision, scale)` default fits when the value rounded to the
/// column scale keeps at most `precision - scale` integer digits.
fn decimal_default_fits(text: &str, field_type: &FieldType) -> bool {
    let precision = field_type.flen.clamp(1, 65);
    let scale = field_type.decimal.clamp(0, 30);
    let Ok(value) = sealdb_types::MyDecimal::parse(text) else {
        return false;
    };
    // Rounding can carry into the integer part (`9999999999.5` on
    // `DECIMAL(10,0)` is out of range).
    let rounded = value.with_frac(scale).to_string();
    let integer_digits = rounded
        .trim_start_matches('-')
        .split('.')
        .next()
        .unwrap_or("")
        .trim_start_matches('0')
        .len();
    integer_digits <= (precision - scale).max(0) as usize
}

/// Temporal functions TiDB evaluates to a `types.Time`. A column that cannot
/// hold a time (`DOUBLE DEFAULT now()`) fails the cast and reports 1067.
const TEMPORAL_DEFAULT_FUNCS: &[&str] = &[
    "now",
    "current_timestamp",
    "localtimestamp",
    "localtime",
    "sysdate",
    "curdate",
    "current_date",
    "curtime",
    "current_time",
    "utc_timestamp",
    "utc_date",
    "utc_time",
];

/// A bare (unparenthesized) temporal function call in `DEFAULT` position.
fn is_bare_temporal_function(default: &str) -> bool {
    let lower = default.trim().to_ascii_lowercase();
    if lower.starts_with('(') {
        return false;
    }
    TEMPORAL_DEFAULT_FUNCS
        .iter()
        .any(|func| calls_function(&lower, func))
}

/// Column types whose zero value is representable by the empty string, so
/// `DEFAULT ''` is accepted (string families, blobs, `ENUM`/`SET`, `BIT`).
fn empty_default_allowed(field_type: &FieldType) -> bool {
    !matches!(
        field_type.tp,
        TYPE_TINY
            | TYPE_SHORT
            | TYPE_INT24
            | TYPE_LONG
            | TYPE_LONG_LONG
            | TYPE_FLOAT
            | TYPE_DOUBLE
            | TYPE_DECIMAL
            | TYPE_NEW_DECIMAL
            | TYPE_DATE
            | TYPE_DATETIME
            | TYPE_TIMESTAMP
            | TYPE_DURATION
    )
}

/// TiDB `checkDefaultValue` on a quoted literal: the value must convert to the
/// column type with truncation/overflow errors raised (`GetColDefaultValue`
/// evaluates with `LevelError`, so this is independent of `sql_mode`).
pub(crate) fn validate_default_literal(
    name: &str,
    field_type: &FieldType,
    default: Option<&str>,
) -> Result<()> {
    let Some(default) = default else {
        return Ok(());
    };
    let trimmed = default.trim();
    // An explicit `DEFAULT NULL` carries no value to convert; the caller
    // separately rejects it on a NOT NULL / primary-key column.
    if trimmed.eq_ignore_ascii_case("null") {
        return Ok(());
    }
    // TiDB `checkColumnDefaultValue`: in strict mode with `NO_ZERO_DATE`, a
    // DATE/DATETIME/TIMESTAMP default that evaluates to the zero time is 1067.
    if is_zero_temporal_default(field_type, trimmed) {
        return Err(Error::new(
            ERR_INVALID_DEFAULT,
            format!("Invalid default value for '{name}'"),
        ));
    }
    if field_type.tp == TYPE_BIT {
        // `b'111'` on `BIT(2)`: the bit-string's value must fit the width.
        if let Some(bits) = bit_literal_bits(trimmed) {
            let width = field_type.flen.clamp(1, 128) as u32;
            let mut acc: u128 = 0;
            let mut overflow = false;
            for b in bits.bytes() {
                if b != b'1' {
                    continue;
                }
                match acc.checked_mul(2).and_then(|v| v.checked_add(1)) {
                    Some(next) => acc = next,
                    None => overflow = true,
                }
            }
            let max = if width >= 128 {
                u128::MAX
            } else {
                (1u128 << width) - 1
            };
            if overflow || acc > max {
                return Err(Error::new(
                    ERR_INVALID_DEFAULT,
                    format!("Invalid default value for '{name}'"),
                ));
            }
        }
        return Ok(());
    }
    let quoted = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''));
    // `DOUBLE DEFAULT now()`: the temporal function is evaluated to a time and
    // then cast to the column type with error-level truncation (1067).
    if quoted.is_none()
        && !matches!(
            field_type.tp,
            TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DURATION
        )
        && is_bare_temporal_function(trimmed)
    {
        return Err(Error::new(
            ERR_INVALID_DEFAULT,
            format!("Invalid default value for '{name}'"),
        ));
    }
    // A bare numeric default only needs the `DECIMAL` range check: the other
    // numeric families normalize to a literal of their own family.
    if quoted.is_none() && !is_decimal_type(field_type) {
        return Ok(());
    }
    let value = quoted
        .map(|inner| inner.replace("''", "'"))
        .unwrap_or_else(|| trimmed.to_string());
    let text = value.trim();
    // `GetColDefaultValue` casts the literal to the column type with
    // error-level truncation: an empty string is only representable by the
    // string-like families (an `INT DEFAULT ''` is 1067).
    if text.is_empty() {
        if empty_default_allowed(field_type) {
            return Ok(());
        }
        return Err(Error::new(
            ERR_INVALID_DEFAULT,
            format!("Invalid default value for '{name}'"),
        ));
    }
    let fits = if let Some((low, high)) = integer_bounds(field_type) {
        let parsed = text
            .parse::<i128>()
            .ok()
            .or_else(|| text.parse::<f64>().ok().map(|v| v.trunc() as i128));
        parsed.is_some_and(|v| v >= low && v <= high)
    } else {
        match field_type.tp {
            // TiDB `GetColDefaultValue` converts with error-level truncation:
            // a value whose integer digits exceed `precision - scale` is 1067.
            TYPE_NEW_DECIMAL | TYPE_DECIMAL => decimal_default_fits(text, field_type),
            // `DOUBLE DEFAULT 'abc'`: the cast from a string fails (1067),
            // while `'1.5'` is a plain value.
            TYPE_FLOAT | TYPE_DOUBLE => text.parse::<f64>().is_ok(),
            // A `DATE` default may carry a time part (`'2024-10-24 12:20'`),
            // which is truncated; a bare `'2024-10'` is not a valid date.
            TYPE_DATE => {
                sealdb_types::parse_date_literal(text).is_ok()
                    || sealdb_types::parse_datetime_literal(text).is_ok()
            }
            TYPE_DATETIME | TYPE_TIMESTAMP => {
                sealdb_types::parse_datetime_literal(text).is_ok()
                    || sealdb_types::parse_datetime_literal(&format!("{text} 00:00:00")).is_ok()
            }
            TYPE_DURATION => {
                normalize_duration_default(text, field_type.decimal.clamp(0, 6)).is_some()
            }
            // A default wider than the declared `VARCHAR`/`CHAR` is 1067: the
            // default is evaluated with error-level truncation regardless of
            // `sql_mode`. A normalized `BINARY(N)` default is padded with
            // literal `\0` escapes (`'t\0\0…'`), so compare decoded length.
            TYPE_STRING | TYPE_VAR_STRING if field_type.flen >= 0 => {
                literal_char_len(text) <= field_type.flen as usize
            }
            _ => true,
        }
    };
    if !fits {
        return Err(Error::new(
            ERR_INVALID_DEFAULT,
            format!("Invalid default value for '{name}'"),
        ));
    }
    Ok(())
}

/// Number of characters a MySQL string literal body decodes to: an escape
/// (`\0`, `\'`, `\\`, …) is one character. `BINARY(N)` defaults carry padded
/// `\0` escapes after normalization, so a raw `chars().count()` overstates the
/// value width.
fn literal_char_len(text: &str) -> usize {
    let mut count = 0usize;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        count += 1;
        if ch == '\\' {
            let _ = chars.next();
        }
    }
    count
}

/// Normalize a quoted literal default to the column type, the way TiDB does at
/// DDL time: temporal defaults are parsed and rounded to the column's
/// fractional precision (`DEFAULT '12:12:13.4'` on `TIME` becomes `12:12:13`),
/// and date/datetime defaults are re-rendered in canonical form.
/// Numeric literal shape used to decide whether TiDB keeps a default unquoted.
struct NumericLiteral<'a> {
    text: &'a str,
    is_integer: bool,
}

fn numeric_literal(text: &str) -> Option<NumericLiteral<'_>> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned.is_empty() {
        return None;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    let seen_exp = false;
    for (i, c) in unsigned.char_indices() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot && !seen_exp => seen_dot = true,
            'e' | 'E' if seen_digit && !seen_exp => {
                let rest = &unsigned[i + 1..];
                let rest = rest.strip_prefix(['+', '-']).unwrap_or(rest);
                if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                return Some(NumericLiteral {
                    text,
                    is_integer: false,
                });
            }
            _ => return None,
        }
    }
    seen_digit.then_some(NumericLiteral {
        text,
        is_integer: !seen_dot,
    })
}

/// Round a numeric literal to an integer the way TiDB casts to an integer
/// column type (0.00 → 0, 2.8 → 3).
fn round_integer_literal(text: &str) -> Option<String> {
    if let Ok(v) = text.trim().parse::<i128>() {
        return Some(v.to_string());
    }
    let v = text.trim().parse::<f64>().ok()?;
    if !v.is_finite() {
        return None;
    }
    let rounded = v.round();
    if rounded.abs() > i64::MAX as f64 {
        return None;
    }
    Some((rounded as i64).to_string())
}

/// Unquoted `DEFAULT` literal normalization; `None` keeps the text as written.
fn unquoted_default(field_type: &FieldType, trimmed: &str) -> Option<String> {
    // A BIT column validates its own bit-string literal (`b'111'` must fit the
    // width); leave it to `validate_default_literal`.
    if field_type.tp == TYPE_BIT {
        return None;
    }
    // MySQL bit/hex literals are values, not numbers of the column's family.
    let lower = trimmed.get(..2).unwrap_or(trimmed).to_ascii_lowercase();
    if matches!(lower.as_str(), "b'" | "x'" | "0b" | "0x") {
        let inner = trimmed
            .strip_prefix(['b', 'B', 'x', 'X'])
            .and_then(|rest| rest.strip_prefix('\''))
            .and_then(|rest| rest.strip_suffix('\''))
            .or_else(|| trimmed.get(2..))?;
        let radix = if lower.starts_with('b') || lower == "0b" {
            2
        } else {
            16
        };
        let value = i128::from_str_radix(inner, radix).ok()?;
        return Some(format!("'{value}'"));
    }
    let literal = numeric_literal(trimmed)?;
    match field_type.eval_type() {
        EvalType::Int => {
            if literal.is_integer {
                Some(literal.text.to_string())
            } else {
                round_integer_literal(literal.text).map(|v| format!("'{v}'"))
            }
        }
        EvalType::Real if matches!(field_type.tp, TYPE_FLOAT | TYPE_DOUBLE) => {
            // TiDB stores the float default as a float64 and SHOW CREATE prints
            // its shortest decimal spelling, quoted (`double DEFAULT 2.0` →
            // `DEFAULT '2'`).
            trimmed
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| format!("'{v}'"))
        }
        _ => None,
    }
}

/// TiDB re-evaluates every literal default when `ALTER TABLE` rebuilds the
/// table, so afterwards SHOW CREATE spells numeric defaults as quoted strings
/// (`int DEFAULT '1'`), even for columns the statement did not touch.
pub(crate) fn requote_default(field_type: &FieldType, default: &str) -> String {
    let trimmed = default.trim();
    if trimmed.starts_with('\'') || trimmed.starts_with('(') || trimmed.eq_ignore_ascii_case("null")
    {
        return default.to_string();
    }
    match field_type.eval_type() {
        EvalType::Int => round_integer_literal(trimmed)
            .map(|v| format!("'{v}'"))
            .unwrap_or_else(|| default.to_string()),
        EvalType::Real if matches!(field_type.tp, TYPE_FLOAT | TYPE_DOUBLE) => trimmed
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| format!("'{v}'"))
            .unwrap_or_else(|| default.to_string()),
        _ => default.to_string(),
    }
}

/// Quoted `DEFAULT` literal on a numeric column.
fn quoted_numeric_default(field_type: &FieldType, value: &str) -> Option<String> {
    let trimmed = value.trim();
    match field_type.eval_type() {
        EvalType::Int => round_integer_literal(trimmed).map(|v| format!("'{v}'")),
        EvalType::Real if matches!(field_type.tp, TYPE_FLOAT | TYPE_DOUBLE) => {
            let v = trimmed.parse::<f64>().ok()?;
            v.is_finite().then(|| format!("'{v}'"))
        }
        _ => None,
    }
}

pub(crate) fn normalize_default(field_type: &FieldType, default: &str) -> String {
    let trimmed = default.trim();
    let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    else {
        // Unquoted literal (number, bit/hex literal): TiDB evaluates it into
        // the column type and quotes the value unless the literal already is a
        // number of the column's own family (`int DEFAULT 1`, `double DEFAULT 1.0`).
        return unquoted_default(field_type, trimmed).unwrap_or_else(|| default.to_string());
    };
    let value = inner.replace("''", "'");
    // A quoted literal on a numeric column is converted at DDL time and stored
    // as the canonical value, which SHOW CREATE TABLE prints back quoted
    // (`float DEFAULT '0012.32'` → `DEFAULT '12.32'`).
    if let Some(text) = quoted_numeric_default(field_type, &value) {
        return text;
    }
    let fsp = field_type.decimal.clamp(0, 6);
    match field_type.tp {
        // TiDB pads a `BINARY(N)` default to N bytes with `\0`, and the
        // padding is part of the printed definition.
        TYPE_STRING if field_type.charset == "binary" && field_type.flen > 0 => {
            let mut text = value;
            let width = field_type.flen as usize;
            let pad = width.saturating_sub(text.len());
            for _ in 0..pad {
                text.push_str("\\0");
            }
            format!("'{text}'")
        }
        TYPE_DURATION => normalize_duration_default(&value, fsp)
            .map(|text| format!("'{text}'"))
            .unwrap_or_else(|| default.to_string()),
        TYPE_DATETIME | TYPE_TIMESTAMP => match sealdb_types::parse_datetime_literal(&value)
            .or_else(|_| sealdb_types::parse_datetime_literal(&format!("{value} 00:00:00")))
        {
            Ok(time) => format!("'{}'", round_time(time, fsp)),
            Err(_) => default.to_string(),
        },
        TYPE_DATE => match sealdb_types::parse_date_literal(&value) {
            Ok(time) => format!("'{time}'"),
            // A `DATE` default may carry a time part, which is truncated.
            Err(_) => match sealdb_types::parse_datetime_literal(&value) {
                Ok(time) => format!("'{:04}-{:02}-{:02}'", time.year, time.month, time.day),
                Err(_) => default.to_string(),
            },
        },
        _ => default.to_string(),
    }
}

/// TiDB `OriginDefaultValue` for a literal TIMESTAMP default: the wall clock is
/// read in the session zone at DDL time and pinned as a UTC spelling, so later
/// inserts (in another session zone) use the same instant.
pub(crate) fn origin_default_for(field_type: &FieldType, default: &str) -> Option<String> {
    if field_type.tp != TYPE_TIMESTAMP {
        return None;
    }
    let trimmed = default.trim();
    let inner = trimmed.strip_prefix('\'')?.strip_suffix('\'')?;
    let value = inner.replace("''", "'");
    if value.starts_with('(') {
        return None;
    }
    let mut t = sealdb_types::parse_datetime_literal(&value)
        .or_else(|_| sealdb_types::parse_datetime_literal(&format!("{value} 00:00:00")))
        .ok()?;
    t.tp = sealdb_types::TimeType::Timestamp;
    // A zero timestamp is not shifted (TiDB keeps `0000-00-00 00:00:00`).
    if t.year == 0 {
        return Some(t.to_string());
    }
    let utc = sealdb_expression::session_local_to_utc(t).ok()?;
    Some(utc.to_string())
}

fn round_time(mut time: sealdb_types::Time, fsp: i32) -> sealdb_types::Time {
    let fsp = fsp.clamp(0, 6) as u32;
    let scale = 10u32.pow(6 - fsp);
    let mut micro = ((time.micro + scale / 2) / scale) * scale;
    if micro >= 1_000_000 {
        micro -= 1_000_000;
        time.second = time.second.saturating_add(1);
        if time.second >= 60 {
            time.second -= 60;
            time.minute = time.minute.saturating_add(1);
            if time.minute >= 60 {
                time.minute -= 60;
                time.hour = time.hour.saturating_add(1);
            }
        }
    }
    time.micro = micro;
    time.fsp = fsp as i8;
    time
}

/// `'12:12:13.4'` (or `'1 12:12:13.4'`, `'-838:59:59'`) as stored in a `TIME`
/// default becomes the normalized `12:12:13` literal, rounded to `fsp`.
fn normalize_duration_default(value: &str, fsp: i32) -> Option<String> {
    let trimmed = value.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    // Drop a leading day part (`D HH:MM:SS`), TiDB stores only the time part.
    let body = body
        .split_once(' ')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(body);
    let (hms, fraction) = match body.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (body, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hour: i64 = parts[0].trim().parse().ok()?;
    let minute: i64 = parts[1].trim().parse().ok()?;
    let mut second: i64 = parts[2].trim().parse().ok()?;
    if !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    let digits: Vec<char> = fraction
        .map(|fraction| fraction.chars().take(7).collect())
        .unwrap_or_default();
    let micro: i64 = digits
        .iter()
        .take(6)
        .fold(String::new(), |mut acc, ch| {
            acc.push(*ch);
            acc
        })
        .parse()
        .map(|micro: i64| micro * 10i64.pow(6 - digits.len().min(6) as u32))
        .unwrap_or(0);
    let fsp = fsp.clamp(0, 6) as u32;
    let scale = 10i64.pow(6 - fsp);
    let rounded_micro = ((micro + scale / 2) / scale) * scale;
    if rounded_micro >= 1_000_000 {
        second += 1;
    }
    let total = hour * 3600 + minute * 60 + second;
    let (hour, minute, second) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut text = format!(
        "{}{:02}:{:02}:{:02}",
        if negative { "-" } else { "" },
        hour.abs(),
        minute.abs(),
        second.abs()
    );
    if fsp > 0 {
        let frac = format!("{:06}", rounded_micro % 1_000_000);
        text.push('.');
        text.push_str(&frac[..fsp as usize]);
    }
    Some(text)
}

/// Column referenced by a normalized TTL expression
/// (`` `col` + INTERVAL 1 DAY ``).
pub(crate) fn ttl_column_name(expr: &str) -> String {
    let trimmed = expr.trim();
    if let Some(rest) = trimmed.strip_prefix('`') {
        return rest.split('`').next().unwrap_or_default().to_string();
    }
    trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn is_temporal_type(tp: u8) -> bool {
    matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP)
}

/// Whether any table (in any schema) has a FOREIGN KEY referencing `table`
/// (TiDB `ErrTTLFKReferenced` gate 8152).
pub(crate) fn referenced_by_foreign_key(is: &InfoSchema, table: &str) -> bool {
    is.all_schemas().iter().any(|db| {
        let db_name = db.name.o().to_string();
        is.schema_tables(&db_name).iter().any(|child| {
            child
                .foreign_keys
                .iter()
                .any(|fk| fk.ref_table.eq_ignore_ascii_case(table))
        })
    })
}

/// TiDB `checkTTLInfo`: the `TTL` column must exist and be `DATETIME`, `DATE`
/// or `TIMESTAMP` (8148 / 1054).
/// TiDB rejects TTL on a table whose clustered primary key holds a FLOAT or
/// DOUBLE column (`ErrTTLUnsupportedPKType`, 8153): the hidden handle cannot be
/// ordered reliably across TiKV and MySQL, so TTL would be unsound.
pub(crate) fn validate_ttl_primary_key(info: &TableInfo) -> Result<()> {
    if !(info.pk_is_handle || info.common_handle) {
        return Ok(());
    }
    let Some(pk) = info.indices.iter().find(|idx| idx.primary) else {
        return Ok(());
    };
    for pk_col in &pk.columns {
        let Some(col) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(pk_col.name.o()))
        else {
            continue;
        };
        if matches!(col.field_type.tp, TYPE_FLOAT | TYPE_DOUBLE) {
            return Err(Error::new(
                ERR_TTL_UNSUPPORTED_PK_TYPE,
                "Unsupported clustered primary key type FLOAT/DOUBLE for TTL",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_ttl(info: &TableInfo) -> Result<()> {
    let Some(ttl) = &info.ttl else {
        return Ok(());
    };
    validate_ttl_primary_key(info)?;
    let column = ttl_column_name(&ttl.expr);
    let Some(col) = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(&column))
    else {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{column}' in 'TTL config'"),
        ));
    };
    if !is_temporal_type(col.field_type.tp) {
        return Err(Error::new(
            ERR_TTL_INVALID_COLUMN,
            format!(
                "Field '{column}' is of a not supported type for TTL config, \
                 expect DATETIME, DATE or TIMESTAMP"
            ),
        ));
    }
    Ok(())
}

/// Validate a TiDB `AFFINITY` table option for a table of the given kind and
/// return the value to store (`"table"` / `"partition"`, empty when unset).
pub(crate) fn affinity_setting(raw: &str, temporary: bool, partitioned: bool) -> Result<String> {
    if temporary {
        return Err(Error::new(
            ERR_AFFINITY,
            "Can not set AFFINITY on a temporary table.",
        ));
    }
    let stored = match raw.to_ascii_lowercase().as_str() {
        "table" => "table",
        "partition" => "partition",
        "none" | "" => "",
        _ => {
            return Err(Error::new(
                ERR_AFFINITY,
                format!("Invalid AFFINITY '{raw}'"),
            ));
        }
    };
    if stored == "table" && partitioned {
        return Err(Error::new(
            ERR_AFFINITY,
            "Can not set AFFINITY='table' on a partition table.",
        ));
    }
    if stored == "partition" && !partitioned {
        return Err(Error::new(
            ERR_AFFINITY,
            "Can not set AFFINITY='partition' on a non-partition table.",
        ));
    }
    Ok(stored.to_string())
}

/// Table-level `DEFAULT CHARSET` / `COLLATE`, also inherited by character
/// columns without an explicit charset. An explicit `COLLATE` names the
/// charset it belongs to and wins over a conflicting `CHARSET`.
fn table_charset_and_collation(stmt: &CreateTableStmt) -> Option<(String, String)> {
    if let Some(collate) = &stmt.collate {
        let parsed = sealdb_parser::charset::Collation::parse(collate);
        return Some((parsed.charset, parsed.name));
    }
    stmt.charset.as_ref().map(|charset| {
        (
            charset.to_ascii_lowercase(),
            sealdb_parser::charset::Collation::default_for_charset(charset).name,
        )
    })
}

pub fn build_table_info(stmt: &CreateTableStmt, table_id: i64) -> Result<TableInfo> {
    build_table_info_with_defaults(stmt, table_id, None)
}

/// `build_table_info` with the enclosing database's charset/collation as the
/// fallback for a table (and its columns) that names neither.
/// TiDB `buildColumnAndConstraint` / `checkTooLongColumn`: a column may not be
/// called `_tidb_rowid`, nor end with a space (1166).
pub(crate) fn validate_column_name(name: &str) -> Result<()> {
    let lower = name.to_lowercase();
    if lower == "_tidb_rowid" || name.is_empty() || name.ends_with(' ') {
        return Err(Error::new(
            ERR_WRONG_COLUMN_NAME,
            format!("Incorrect column name '{lower}'"),
        ));
    }
    Ok(())
}

/// TiDB `checkContainDotColumn` / `ChangeColumn` / `ModifyColumn`: a column
/// name written as `[db.]table.column` must name the statement's target table
/// (1102 wrong database, 1103 wrong table). Returns the bare column name.
pub(crate) fn resolve_qualified_column_name(
    name: &str,
    schema: &str,
    table: &str,
) -> Result<String> {
    let sep = sealdb_parser::sqlparser_bridge::lower::QUALIFIED_COLUMN_SEP;
    if !name.contains(sep) {
        return Ok(name.to_string());
    }
    let parts: Vec<&str> = name.split(sep).collect();
    let (schema_part, table_part, column) = match parts.as_slice() {
        [schema_part, table_part, column] => (Some(*schema_part), *table_part, *column),
        [table_part, column] => (None, *table_part, *column),
        _ => return Ok(name.to_string()),
    };
    if let Some(schema_part) = schema_part
        && !schema_part.eq_ignore_ascii_case(schema)
    {
        return Err(Error::new(
            ERR_WRONG_DB_NAME,
            format!("Incorrect database name '{schema_part}'"),
        ));
    }
    if !table_part.eq_ignore_ascii_case(table) {
        return Err(Error::new(
            ERR_WRONG_TABLE_NAME,
            format!("Incorrect table name '{table_part}'"),
        ));
    }
    Ok(column.to_string())
}

/// TiDB `setTableAutoRandomBits`: `AUTO_RANDOM(shard_bits, range_bits)`
/// range bits must fall in `[32, 64]`, and the bits left for the incremental
/// part (`range_bits - shard_bits`, minus the sign bit of a signed column)
/// must be at least `AutoRandomIncBitsMin` (27). Without an explicit range the
/// column type's width (64) is used.
pub(crate) fn validate_auto_random_bits(
    shard: Option<u32>,
    range: Option<u32>,
    field_type: &FieldType,
) -> Result<()> {
    let Some(shard) = shard else {
        return Ok(());
    };
    if let Some(range) = range
        && !(32..=64).contains(&range)
    {
        return Err(Error::new(
            ERR_INVALID_AUTO_RANDOM,
            format!(
                "Invalid auto random: auto_random range bits must be between 32 and 64, but got {range}"
            ),
        ));
    }
    let incremental = match range {
        Some(range) => range.saturating_sub(shard),
        None => 64u32.saturating_sub(shard),
    };
    let incremental = if field_type.unsigned {
        incremental
    } else {
        incremental.saturating_sub(1)
    };
    if incremental < 27 {
        return Err(Error::new(
            ERR_INVALID_AUTO_RANDOM,
            "Invalid auto random: auto_random ID space is too small, please decrease the shard bits or increase the range bits",
        ));
    }
    Ok(())
}

pub fn build_table_info_with_defaults(
    stmt: &CreateTableStmt,
    table_id: i64,
    schema_defaults: Option<(String, String)>,
) -> Result<TableInfo> {
    // TiDB `checkCreateTableGrammar`: temporary tables cannot carry placement
    // policy options (8006).
    if (stmt.temporary || stmt.on_commit_delete_rows) && stmt.placement_policy.is_some() {
        return Err(Error::new(
            ERR_OPT_ON_TEMPORARY_TABLE,
            "`PLACEMENT` is unsupported on temporary tables.",
        ));
    }
    // TiDB `checkCreateTableGrammar`: temporary tables cannot carry an
    // `auto_random` column (8006).
    if (stmt.temporary || stmt.on_commit_delete_rows)
        && stmt.cols.iter().any(|col| col.auto_random.is_some())
    {
        return Err(Error::new(
            ERR_OPT_ON_TEMPORARY_TABLE,
            "`auto_random` is unsupported on temporary tables.",
        ));
    }
    if stmt.cols.is_empty() {
        return Err(Error::new(
            ERR_TABLE_MUST_HAVE_COLUMNS,
            "A table must have at least 1 column",
        ));
    }
    // Table-level `DEFAULT CHARSET` / `COLLATE` is the default charset of every
    // character column that does not name one (MySQL/TiDB semantics). A table
    // that names neither inherits the database default.
    let table_defaults = table_charset_and_collation(stmt).or(schema_defaults);
    let mut columns = Vec::with_capacity(stmt.cols.len());
    for (offset, col) in stmt.cols.iter().enumerate() {
        // TiDB `buildColumnAndConstraint`: unusable column names are 1166.
        validate_column_name(&col.name)?;
        // TiDB `checkCreateTableGrammar`: column names are unique
        // case-insensitively (1060).
        if columns
            .iter()
            .any(|c: &ColumnInfo| c.name.o().eq_ignore_ascii_case(&col.name))
        {
            return Err(Error::new(
                ERR_DUP_FIELDNAME,
                format!("Duplicate column name '{}'", col.name),
            ));
        }
        let mut info = ColumnInfo::new(
            (offset as i64) + 1,
            offset as i32,
            col.name.clone(),
            field_type_from_name(&col.type_name)?,
        );
        // Column charset/collation (TiDB `FieldType.Charset/Collate`): a charset
        // option pins its default binary collation unless COLLATE names one.
        if let Some(charset) = &col.charset {
            info.field_type.charset = charset.to_ascii_lowercase();
            info.field_type.collate =
                sealdb_parser::charset::Collation::default_for_charset(charset).name;
        }
        if let Some(collate) = &col.collate {
            let parsed = sealdb_parser::charset::Collation::parse(collate);
            info.field_type.charset = parsed.charset;
            info.field_type.collate = parsed.name;
        }
        // A character column without an explicit charset inherits the table's.
        // `binary` marks the BINARY/VARBINARY/BLOB family, which has no charset.
        if col.charset.is_none()
            && col.collate.is_none()
            && info.field_type.charset != "binary"
            && info.field_type.is_character_type()
            && let Some((charset, collate)) = &table_defaults
        {
            info.field_type.charset = charset.clone();
            info.field_type.collate = collate.clone();
        }
        // TiDB `OverwriteCollationWithBinaryFlag`: the MySQL `BINARY` column
        // attribute pins the charset's `_bin` collation, unless an explicit
        // charset and collation already chose one.
        let has_binary_attr = col.comment.as_deref()
            == Some(sealdb_parser::sqlparser_bridge::lower::BINARY_ATTR_COMMENT_MARKER);
        if has_binary_attr
            && !(col.charset.is_some() && col.collate.is_some())
            && info.field_type.is_character_type()
        {
            let charset = info.field_type.charset.clone();
            info.field_type.collate =
                sealdb_parser::charset::Collation::default_for_charset(&charset).name;
        }
        if let Some(generated) = &col.generated {
            validate_generated_expr(&col.name, &generated.expr)?;
            info.generated_expr = Some(generated.expr.clone());
            info.generated_stored = generated.stored;
        } // TiDB `decodeEnumSetBinaryLiteralToUTF8`: hex/bit member literals are
        // raw bytes in the column charset; rewrite them to UTF-8.
        decode_enum_set_binary_literals(&mut info.field_type);
        validate_enum_set_members(&info.name.o().to_string(), &info.field_type)?;
        let default = col.default.as_deref().map(|default| {
            normalize_expression_default(&normalize_default(&info.field_type, default))
        });
        info.default_expr = normalize_lob_default(info.name.o(), &info.field_type, default)?;
        info.origin_default_expr = info
            .default_expr
            .as_deref()
            .and_then(|default| origin_default_for(&info.field_type, default));
        info.auto_increment = col.auto_increment;
        if col.auto_random.is_some() {
            validate_auto_random_bits(col.auto_random, col.auto_random_range, &info.field_type)?;
        }
        info.auto_random = col.auto_random;
        info.auto_random_range = col.auto_random_range;
        // Parser rewrites `ZEROFILL` → UNSIGNED + marker comment.
        if col.comment.as_deref()
            == Some(sealdb_parser::sqlparser_bridge::lower::ZEROFILL_COMMENT_MARKER)
        {
            info.field_type.flag |= 64; // MySQL zerofillFlag
            info.field_type.unsigned = true;
            info.field_type.flag |= 32;
            info.comment = None;
        } else if col.comment.as_deref()
            == Some(sealdb_parser::sqlparser_bridge::lower::BINARY_ATTR_COMMENT_MARKER)
        {
            info.comment = None;
        } else {
            info.comment = col.comment.clone();
        }
        validate_display_width(info.name.o(), &info.field_type)?;
        // TiDB `CheckDisplayWidth`: MySQL 8 deprecates the integer display width
        // (`int(11)`); a bare `int` declares none and stays silent.
        if matches!(
            info.field_type.tp,
            TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG
        ) && parse_len(&col.type_name).is_some()
        {
            sealdb_expression::note_warning(
                1681,
                "Integer display width is deprecated and will be removed in a future release."
                    .to_string(),
            );
        }
        validate_default_literal(
            info.name.o(),
            &info.field_type,
            info.default_expr.as_deref(),
        )?;
        validate_default_fsp(
            info.name.o(),
            &info.field_type,
            info.default_expr.as_deref(),
        )?;
        validate_on_update(info.name.o(), &info.field_type, col.on_update.as_deref())?;
        // TiDB `checkPriKeyConstraint` / `checkDefaultValue`: `DEFAULT NULL` is
        // only legal on a nullable column. On a primary key column the error
        // depends on how the key was declared (1067 inline, 1171 table-level);
        // an explicit `NULL` on a key column is always 1171.
        let in_primary_key = stmt.indices.iter().any(|idx| {
            idx.primary
                && idx.columns.iter().any(|part| {
                    part.column_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&col.name))
                })
        });
        let default_is_null = info
            .default_expr
            .as_deref()
            .is_some_and(|default| default.eq_ignore_ascii_case("null"));
        if default_is_null && in_primary_key {
            return Err(if col.inline_primary_key {
                Error::new(
                    ERR_INVALID_DEFAULT,
                    format!("Invalid default value for '{}'", info.name.o()),
                )
            } else {
                Error::new(
                    ERR_PRIMARY_CANT_HAVE_NULL,
                    "All parts of a PRIMARY KEY must be NOT NULL; if you need NULL in a key, \
                     use UNIQUE instead",
                )
            });
        }
        if default_is_null && col.not_null {
            return Err(Error::new(
                ERR_INVALID_DEFAULT,
                format!("Invalid default value for '{}'", info.name.o()),
            ));
        }
        if col.explicit_null && in_primary_key {
            return Err(Error::new(
                ERR_PRIMARY_CANT_HAVE_NULL,
                "All parts of a PRIMARY KEY must be NOT NULL; if you need NULL in a key, \
                 use UNIQUE instead",
            ));
        }
        if col.not_null {
            info.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
        }
        check_column_attributes(info.name.o(), &info.field_type)?;
        columns.push(info);
    }
    // TiDB `checkTooManyColumns`: at most 1017 columns per table (1117).
    if columns.len() > 1017 {
        return Err(Error::new(ERR_TOO_MANY_FIELDS, "Too many columns"));
    }
    // TiDB `checkAutoIncrement` (a preprocess-time check): only one column may
    // be declared `AUTO_INCREMENT` (1075).
    if columns.iter().filter(|c| c.auto_increment).count() > 1 {
        return Err(Error::new(
            ERR_WRONG_AUTO_KEY,
            "Incorrect table definition; there can be only one auto column and it must be \
             defined as a key",
        ));
    }
    validate_generated_auto_increment(&columns, None)?;
    validate_generated_column_refs(&columns, None)?;
    let names: Vec<String> = columns.iter().map(|c| c.name.o().to_string()).collect();
    crate::alter::check_generated_order(&names, &columns)?;
    let mut info = TableInfo::new(table_id, stmt.table.name.clone(), columns);
    info.checks = stmt
        .checks
        .iter()
        .map(|(expr, enforced)| sealdb_meta::CheckConstraint {
            expr: expr.clone(),
            enforced: *enforced,
        })
        .collect();
    info.placement_policy = stmt.placement_policy.clone();
    // TiDB `SHARD_ROW_ID_BITS` / `PRE_SPLIT_REGIONS` table options.
    if let Some(bits) = stmt.shard_row_id_bits {
        info.shard_row_id_bits = bits.min(63) as u8;
    }
    if let Some(regions) = stmt.pre_split_regions {
        info.pre_split_regions = regions as u8;
    }
    // TiDB `AUTO_ID_CACHE`: the value must fit int64 (1105 otherwise, reported
    // with the whole DDL as a plain unknown error).
    if let Some(raw) = &stmt.auto_id_cache {
        info.auto_id_cache = Some(parse_auto_id_cache(raw)?);
    }
    // Table-level `DEFAULT CHARSET` / `COLLATE` (TiDB `TableInfo.Charset/Collate`).
    if let Some((charset, collate)) = table_defaults {
        info.charset = charset;
        info.collate = collate;
    }
    for index in stmt.indices.iter() {
        // TiDB's InnoDB cannot store a FULLTEXT index: the definition is
        // dropped with warning 1214 instead of being created.
        if index.fulltext {
            sealdb_expression::note_warning(
                ERR_WARN_TABLE_TYPE_NOT_SUPPORT_FULLTEXT,
                "The used table type doesn't support FULLTEXT indexes",
            );
            continue;
        }
        check_index_name(&index.name, index.primary)?;
        // TiDB `checkAndBuildIndexInfo`: a repeated index name is 1061, or 1068
        // when the new key is another PRIMARY KEY.
        if info
            .indices
            .iter()
            .any(|idx| idx.name.o().eq_ignore_ascii_case(&index.name))
        {
            return Err(if index.primary {
                Error::new(ERR_MULTIPLE_PRI_KEY, "Multiple primary key defined")
            } else {
                Error::new(
                    ERR_DUP_KEYNAME,
                    format!("Duplicate key name '{}'", index.name),
                )
            });
        }
        let (idx_cols, hidden) = crate::expr_index::resolve_index_parts(
            &info,
            &index.name,
            &index.columns,
            index.primary,
        )?;
        // TiDB `checkGlobalIndexes` → `validateGlobalIndexWithGeneratedColumns`:
        // auto analyze cannot collect a GLOBAL index that keeps prefix columns
        // or virtual generated columns, so the DDL asks for a manual ANALYZE
        // (8265). The warning is emitted at CREATE TABLE only; `ALTER TABLE …
        // ADD INDEX` accepts the same definition silently.
        let multi_valued = index.columns.iter().any(|part| match part {
            IndexPart::Expression(expr) => crate::expr_index::is_multi_valued_part(expr),
            _ => false,
        });
        if index.global && !multi_valued {
            let special =
                !hidden.is_empty() || index.columns.iter().any(|part| part.prefix_length() > 0);
            if special {
                sealdb_expression::note_warning(
                    8265,
                    format!(
                        "Auto analyze is not effective for index '{}', need analyze manually",
                        index.name
                    ),
                );
            }
        }
        // TiDB `buildTableInfoWithCheck`: a virtual generated column cannot be
        // a primary key (3106).
        if index.primary {
            for ic in &idx_cols {
                if info.columns.iter().any(|c| {
                    c.offset == ic.offset && c.generated_expr.is_some() && !c.generated_stored
                }) {
                    return Err(crate::alter::unsupported_generated(
                        "Defining a virtual generated column as primary key",
                    ));
                }
            }
        }
        for col in hidden {
            info.columns.push(col);
            info.max_col_id = info.max_col_id.max(info.columns.last().unwrap().id);
        }
        // PRIMARY KEY handle choice follows `tidb_enable_clustered_index`
        // (TiDB `BuildTableInfo`): an explicit `CLUSTERED` / `NONCLUSTERED`
        // wins, otherwise the mode decides. A single integer key becomes the
        // integer row handle (`pk_is_handle`) when the mode allows; any other
        // key becomes a common handle only in `ON` mode. BIT is not a handle.
        if index.primary && !index.nonclustered {
            let single_int = idx_cols.len() == 1
                && info.columns.iter().any(|c| {
                    c.offset == idx_cols[0].offset
                        && c.field_type.eval_type() == sealdb_types::field_type::EvalType::Int
                        && c.field_type.tp != TYPE_BIT
                });
            let mode = sealdb_meta::clustered_index_mode();
            if single_int && (index.clustered || mode.clusters_int_primary()) {
                info.pk_is_handle = true;
            } else if index.clustered || mode.clusters_common_primary() {
                info.common_handle = true;
            }
        }
        // TiDB `BuildTableInfo`: a clustered index cannot be a global index
        // (8200), whether or not the table ends up partitioned.
        if index.primary && index.global && (info.pk_is_handle || info.common_handle) {
            return Err(Error::new(
                ERR_UNSUPPORTED_DDL_OPERATION,
                "Unsupported create an index that is both a global index and a clustered index",
            ));
        }
        let pk_col_offsets: Vec<i32> = if index.primary {
            idx_cols.iter().map(|ic| ic.offset).collect()
        } else {
            Vec::new()
        };
        // TiDB `BuildTableInfo`: an integer row handle is not an index, so it
        // takes no index id (`information_schema.tidb_indexes` prints the
        // synthetic PRIMARY row with id 0 while the first real index is 1).
        let idx_id = if index.primary && info.pk_is_handle {
            0
        } else {
            info.max_idx_id + 1
        };
        info.indices.push(
            IndexInfo::new(idx_id, index.name.clone(), info.name.o(), idx_cols)
                .with_flags(index.unique, index.primary)
                .with_global(index.global)
                .with_invisible(index.invisible)
                .with_where_clause(index.where_clause.clone()),
        );
        info.max_idx_id = info.max_idx_id.max(idx_id);
        for c in &mut info.columns {
            if pk_col_offsets.contains(&c.offset) {
                c.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
            }
        }
    }
    // TiDB `checkTooManyIndexes`: at most 64 keys per table (1069).
    if info.indices.len() > 64 {
        return Err(Error::new(
            ERR_TOO_MANY_KEYS,
            "Too many keys specified; max 64 keys allowed",
        ));
    }
    // TiDB `BuildTableInfo`: AUTO_INCREMENT implies NOT NULL.
    for c in &mut info.columns {
        if c.auto_increment {
            c.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
        }
    }
    // TiDB `BuildTableInfo`: a FOREIGN KEY needs a local index whose prefix is
    // exactly the FK columns; one is created implicitly (named after the
    // constraint) when no existing index (or the integer row handle) covers it.
    // A temporary table (TiDB `TempTableLocal` or `TempTableGlobal`) cannot own
    // a foreign key (`ErrCannotAddForeign`, 1215).
    if !stmt.foreign_keys.is_empty() && (stmt.temporary || stmt.on_commit_delete_rows) {
        return Err(Error::new(
            ERR_CANNOT_ADD_FOREIGN,
            "Cannot add foreign key constraint",
        ));
    }
    for (n, fk) in stmt.foreign_keys.iter().enumerate() {
        add_foreign_key(&mut info, fk, n)?;
    }
    Ok(info)
}

/// TiDB `checkIndexInfo`: a secondary index (or FK constraint) may not carry
/// the reserved name `PRIMARY` (1280).
pub(crate) fn check_index_name(name: &str, primary: bool) -> Result<()> {
    if !primary && name.eq_ignore_ascii_case("primary") {
        return Err(Error::new(
            ERR_WRONG_NAME_FOR_INDEX,
            format!("Incorrect index name '{name}'"),
        ));
    }
    Ok(())
}

/// TiDB `BuildTableInfo` / `AlterTableAddForeignKey`: append one FOREIGN KEY,
/// materialising the implicit index over its local columns and rejecting a
/// constraint name already used by the table (`ErrFkDupName`, 1826).
pub(crate) fn add_foreign_key(
    info: &mut TableInfo,
    fk: &ForeignKeyDef,
    position: usize,
) -> Result<()> {
    let name = if fk.name.is_empty() {
        format!("fk_{}", position + 1)
    } else {
        fk.name.clone()
    };
    check_index_name(&name, false)?;
    // TiDB `buildFKInfo`: every referenced column must pair with a local one.
    if fk.columns.len() != fk.ref_columns.len() {
        return Err(Error::new(
            ERR_WRONG_FK_DEF,
            format!(
                "Incorrect foreign key definition for '{name}': Key reference and table reference don't match"
            ),
        ));
    }
    if info
        .foreign_keys
        .iter()
        .any(|existing| existing.name.eq_ignore_ascii_case(&name))
    {
        return Err(Error::new(
            ERR_FK_DUP_NAME,
            format!("Duplicate foreign key constraint name '{name}'"),
        ));
    }
    let cols = fk
        .columns
        .iter()
        .map(|col_name| {
            info.columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(col_name))
                .map(|c| (c.name.o().to_string(), c.offset))
                .ok_or_else(|| {
                    // TiDB `buildTableInfoWithCheck` -> `findCol` reports a
                    // missing local FK column as `ErrKeyColumnDoesNotExits`
                    // (1072), not a generic 1215.
                    Error::new(
                        ERR_KEY_COLUMN_DOES_NOT_EXIST,
                        format!("Key column '{col_name}' doesn't exist in table"),
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if !fk_index_covers(info, &cols) {
        let idx_id = info.max_idx_id + 1;
        info.indices.push(IndexInfo::new(
            idx_id,
            name.clone(),
            info.name.o(),
            cols.iter()
                .map(|(col, offset)| IndexColumn::new(col.clone(), *offset))
                .collect(),
        ));
        info.max_idx_id = idx_id;
    }
    info.foreign_keys.push(sealdb_meta::ForeignKeyInfo {
        name,
        cols: cols.iter().map(|(col, _)| col.clone()).collect(),
        ref_table: fk.ref_table.clone(),
        ref_schema: fk.ref_schema.clone(),
        ref_cols: fk.ref_columns.clone(),
        on_update_cascade: fk.on_update_cascade,
        on_delete_cascade: fk.on_delete_cascade,
    });
    Ok(())
}

/// Schema a foreign key's parent table lives in: an explicit
/// `REFERENCES db.t` names it, otherwise the child table's own schema.
pub(crate) fn fk_ref_schema<'a>(
    child_schema: &'a str,
    fk: &'a sealdb_meta::ForeignKeyInfo,
) -> &'a str {
    fk.ref_schema.as_deref().unwrap_or(child_schema)
}

/// Whether an index (or the integer row handle) already covers the FK columns
/// as a prefix, so no implicit FK index is needed.
fn fk_index_covers(info: &TableInfo, cols: &[(String, i32)]) -> bool {
    let covers = |index: &IndexInfo| {
        index.columns.len() >= cols.len()
            && index
                .columns
                .iter()
                .zip(cols.iter())
                .all(|(ic, (_, offset))| ic.offset == *offset)
    };
    if info.pk_is_handle
        && let Some(pk) = info.indices.iter().find(|idx| idx.primary)
        && covers(pk)
    {
        return true;
    }
    info.indices.iter().any(covers)
}

/// TiDB `BuildTableInfoWithLike`: clone columns/indexes/partitions, new table
/// and partition IDs so row keys do not collide with the source.
pub(crate) fn build_table_info_with_like(
    name: &str,
    refer: &TableInfo,
    table_id: i64,
    meta: &mut Mutator,
) -> Result<TableInfo> {
    let mut info = refer.clone();
    info.id = table_id;
    info.name = CIStr::new(name);
    for idx in &mut info.indices {
        idx.table = CIStr::new(name);
    }
    if let Some(part) = &mut info.partition {
        for def in &mut part.definitions {
            def.id = meta.gen_global_id()?;
        }
    }
    Ok(info)
}

pub(crate) fn columns_from_select(
    is: &InfoSchema,
    select: &SelectStmt,
    current_db: &str,
) -> Result<Vec<ColumnDef>> {
    columns_from_select_scoped(is, select, current_db, &HashMap::new())
}

/// One `FROM` / `JOIN` source: a physical table or a CTE in scope.
struct SelectSource {
    table: String,
    columns: Vec<(String, String)>,
}

fn columns_from_select_scoped(
    is: &InfoSchema,
    select: &SelectStmt,
    current_db: &str,
    outer_scope: &HashMap<String, Vec<(String, String)>>,
) -> Result<Vec<ColumnDef>> {
    let mut scope = outer_scope.clone();
    // CTE bodies see only preceding sibling CTEs; their output names/types are
    // what a later `FROM cte` expands / resolves columns against.
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            let cols = columns_from_select_scoped(is, &cte.query, current_db, &scope)?;
            let mut cols = cols;
            if !cte.col_name_list.is_empty() {
                if cte.col_name_list.len() != cols.len() {
                    return Err(Error::new(
                        ERR_VIEW_WRONG_LIST,
                        "In definition of view, derived table or common table expression, SELECT list and column names list have different column counts",
                    ));
                }
                for (col, name) in cols.iter_mut().zip(&cte.col_name_list) {
                    col.name = name.clone();
                }
            }
            scope.insert(
                cte.name.to_ascii_lowercase(),
                cols.into_iter()
                    .map(|col| (col.name, col.type_name))
                    .collect(),
            );
        }
    }
    let sources = select_sources(is, select, current_db, &scope)?;
    let mut cols = Vec::new();
    for (i, field) in select.fields.iter().enumerate() {
        if is_star_expr(&field.expr) {
            if sources.is_empty() {
                return Err(Error::unsupported("CREATE TABLE AS SELECT * without FROM"));
            }
            for source in &sources {
                for (name, type_name) in &source.columns {
                    push_select_column(&mut cols, name, type_name.clone(), false)?;
                }
            }
            continue;
        }
        // Qualified star `t.*`: expand only that source's columns.
        if let ExprNode::Column(col) = &field.expr
            && col.name == "*"
        {
            let hint = col.table.as_deref().unwrap_or_default();
            let source = sources
                .iter()
                .find(|source| source.table.eq_ignore_ascii_case(hint))
                .ok_or_else(|| {
                    Error::new(ERR_NO_SUCH_TABLE, format!("Table '{hint}' doesn't exist"))
                })?;
            for (name, type_name) in &source.columns {
                push_select_column(&mut cols, name, type_name.clone(), false)?;
            }
            continue;
        }
        let name = select_field_name(field, i);
        let type_name = type_name_from_expr(&field.expr, &sources)?;
        push_select_column(
            &mut cols,
            &name,
            type_name,
            expr_is_not_null_constant(&field.expr),
        )?;
    }
    if cols.is_empty() {
        return Err(Error::unsupported("CREATE TABLE AS SELECT without columns"));
    }
    Ok(cols)
}

/// A CTAS / view column is `NOT NULL` when its expression folds to a non-NULL
/// constant: TiDB builds those with `expression.NewConstant`, which keeps the
/// not-null flag, and a CAST carries it through
/// (`cast(replace(substring_index(…)) as char(32))`).
fn expr_is_not_null_constant(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(v) => !matches!(v.datum, Datum::Null),
        ExprNode::Cast(c) => expr_is_not_null_constant(&c.expr),
        ExprNode::Unary(u) => expr_is_not_null_constant(&u.expr),
        ExprNode::Binary(b) => {
            expr_is_not_null_constant(&b.left) && expr_is_not_null_constant(&b.right)
        }
        ExprNode::Func(f) => !f.args.is_empty() && f.args.iter().all(expr_is_not_null_constant),
        _ => false,
    }
}

fn push_select_column(
    cols: &mut Vec<ColumnDef>,
    name: &str,
    type_name: String,
    not_null: bool,
) -> Result<()> {
    if cols.iter().any(|col| col.name.eq_ignore_ascii_case(name)) {
        return Err(Error::new(
            ERR_DUP_FIELDNAME,
            format!("Duplicate column name '{name}'"),
        ));
    }
    cols.push(ColumnDef {
        name: name.to_string(),
        type_name,
        default: None,
        generated: None,
        auto_increment: false,
        auto_random: None,
        auto_random_range: None,
        not_null,
        explicit_null: false,
        inline_primary_key: false,
        on_update: None,
        charset: None,
        collate: None,
        comment: None,
    });
    Ok(())
}

fn select_sources(
    is: &InfoSchema,
    select: &SelectStmt,
    current_db: &str,
    scope: &HashMap<String, Vec<(String, String)>>,
) -> Result<Vec<SelectSource>> {
    let mut sources = Vec::new();
    for table in select.join_tables() {
        if let Some(columns) = scope.get(&table.resolve_name().to_ascii_lowercase()) {
            sources.push(SelectSource {
                table: table.name.clone(),
                columns: columns.clone(),
            });
            continue;
        }
        let (schema, name) = table.qualified(current_db);
        let info = is.table_by_name(&schema, &name).ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            )
        })?;
        sources.push(SelectSource {
            table: table.name.clone(),
            columns: info
                .columns
                .iter()
                .map(|col| (col.name.o().to_string(), col.field_type.type_name()))
                .collect(),
        });
    }
    Ok(sources)
}

/// TiDB's implicit name for a select field: the explicit alias, else the
/// expression's `Text()` form. Names longer than 64 characters (or with no
/// derivable name) fall back to `name_exp_<position>`.
pub(crate) fn select_field_name(field: &sealdb_parser::ast::SelectField, offset: usize) -> String {
    use sealdb_parser::ast::expressions::ExprNode;
    let name = match &field.as_name {
        Some(name) if !name.is_empty() => name.clone(),
        // Column / literal names come from `expr.name()` (unquoted content).
        // Complex exprs (window / func) prefer the original select-list snippet
        // so TiDB `field.Text()` casing is preserved.
        _ => match &field.expr {
            ExprNode::Column(_) | ExprNode::Value(_) => field.expr.name(),
            _ => field
                .orig_text
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| field.expr.name()),
        },
    };
    // TiDB falls back to `name_exp_<pos>` when the derived name cannot be a
    // column identifier (empty or longer than 64 characters).
    if name.is_empty() || name.len() > 64 {
        return format!("name_exp_{}", offset + 1);
    }
    name
}

fn type_name_from_expr(expr: &ExprNode, sources: &[SelectSource]) -> Result<String> {
    match expr {
        ExprNode::Column(col) => Ok(lookup_select_column(col, sources)?),
        ExprNode::Cast(cast) => Ok(cast_view_type_name(&cast.target)),
        ExprNode::Value(ValueExpr { datum }) => Ok(type_name_from_datum(datum).to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("COUNT") => Ok("bigint".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("SUM") => Ok("decimal".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("AVG") => Ok("double".to_string()),
        ExprNode::Window(_) => Ok("bigint".to_string()),
        _ => Ok("varchar".to_string()),
    }
}

/// TiDB types a `CAST(… AS CHAR(n))` select item as `varchar(n)`; every other
/// cast keeps its target spelling.
fn cast_view_type_name(target: &str) -> String {
    let lower = target.trim().to_ascii_lowercase();
    if lower == "char" {
        return "varchar".to_string();
    }
    if let Some(rest) = lower.strip_prefix("char(") {
        return format!("varchar({rest}");
    }
    target.to_string()
}

fn lookup_select_column(col: &ColumnNameExpr, sources: &[SelectSource]) -> Result<String> {
    let hinted = col.table.as_deref();
    let mut found = None;
    for source in sources {
        if let Some(hint) = hinted
            && !source.table.eq_ignore_ascii_case(hint)
        {
            continue;
        }
        if let Some((_, type_name)) = source
            .columns
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&col.name))
        {
            if found.is_some() && hinted.is_none() {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Column '{0}' in field list is ambiguous", col.name),
                ));
            }
            found = Some(type_name.clone());
        }
    }
    found.ok_or_else(|| {
        Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{}' in 'field list'", col.name),
        )
    })
}

fn type_name_from_datum(datum: &Datum) -> &'static str {
    match datum {
        // TiDB types integer literals as `bigint` (constant fold keeps the
        // longlong type in CTAS / view column metadata).
        Datum::I64(_) | Datum::Year(_) | Datum::U64(_) => "bigint",
        Datum::Bit(_) => "int",
        Datum::F64(_) => "double",
        Datum::Decimal(_) => "decimal",
        Datum::Time(_) => "datetime",
        Datum::Bytes(_) | Datum::Null | Datum::Enum(_) => "varchar",
    }
}

fn is_star_expr(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b)
        }) if b == b"*"
    )
}

pub(crate) fn field_type_from_name(type_name: &str) -> Result<FieldType> {
    let upper = type_name.to_ascii_uppercase();
    let (base, rest) = split_type(&upper);
    let unsigned = rest.contains("UNSIGNED") || base.ends_with("UNSIGNED");
    let mut ft = match base {
        "TINYINT" | "INT1" | "BOOL" | "BOOLEAN" => FieldType {
            tp: TYPE_TINY,
            flen: 4,
            ..numeric_ft()
        },
        "SMALLINT" | "INT2" => FieldType {
            tp: TYPE_SHORT,
            flen: 6,
            ..numeric_ft()
        },
        "MEDIUMINT" | "INT3" => FieldType {
            tp: TYPE_INT24,
            flen: 9,
            ..numeric_ft()
        },
        "INT" | "INTEGER" | "INT4" => FieldType {
            tp: TYPE_LONG,
            flen: 11,
            ..numeric_ft()
        },
        "BIGINT" | "INT8" => FieldType {
            tp: TYPE_LONG_LONG,
            flen: 20,
            ..numeric_ft()
        },
        "FLOAT" => {
            let (flen, decimal) = parse_flen_decimal_float(type_name).unwrap_or((12, -1));
            // MySQL: `FLOAT(p)` with p > 24 widens to a 8-byte DOUBLE.
            FieldType {
                tp: if flen > 24 { TYPE_DOUBLE } else { TYPE_FLOAT },
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "REAL" if sealdb_expression::sql_mode_flag("REAL_AS_FLOAT") => {
            let (flen, decimal) = parse_flen_decimal_float(type_name).unwrap_or((12, -1));
            FieldType {
                tp: TYPE_FLOAT,
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "DOUBLE" | "REAL" => {
            let parsed = parse_flen_decimal_float(type_name);
            let (flen, decimal) = parsed.unwrap_or((22, -1));
            // TiDB parser config `EnableStrictDoubleTypeCheck`: `DOUBLE(M)`
            // without a decimal count is a syntax error (the FLOAT form only
            // widens to DOUBLE with an unspecified length, so it never hits
            // this).
            if parsed.is_some_and(|(_, decimal)| decimal == -1)
                && sealdb_expression::strict_double_type_check()
            {
                return Err(Error::new(
                    sealdb_errno::ERR_SYNTAX,
                    "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use",
                ));
            }
            FieldType {
                tp: TYPE_DOUBLE,
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "DECIMAL" | "NUMERIC" => {
            // MySQL/TiDB: `decimal(0)` / `decimal(0,0)` means precision 10.
            let (flen, decimal) = parse_flen_decimal(type_name).unwrap_or((10, 0));
            let flen = if flen == 0 { 10 } else { flen };
            FieldType {
                tp: TYPE_NEW_DECIMAL,
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "CHAR" => FieldType {
            // MySQL `CHAR` is `TYPE_STRING` (fixed length); SealDB stores it
            // like a VARCHAR but keeps the type for `SHOW CREATE` / `DESC`.
            tp: TYPE_STRING,
            flen: parse_len(type_name).unwrap_or(1),
            ..string_ft()
        },
        "VARCHAR" | "STRING" => FieldType {
            tp: TYPE_VAR_STRING,
            flen: parse_len(type_name).unwrap_or(-1),
            ..string_ft()
        },
        // `TEXT(n)` picks the smallest TEXT family member that holds `n` bytes
        // (TiDB: `TEXT(10)` is `tinytext`); a bare `TEXT` stays unbounded.
        "TEXT" => match parse_len(type_name) {
            Some(len) => FieldType {
                tp: lob_tp_for_len(len),
                flen: -1,
                ..string_ft()
            },
            None => FieldType {
                tp: TYPE_VAR_STRING,
                flen: -1,
                ..string_ft()
            },
        },
        // ENUM/SET store their member labels as text; comparisons keep the
        // label until numeric-vs-enum coercion lands (TiDB numeric path).
        "ENUM" => {
            let (elems, binary) = parse_elems_with_binary(type_name)?;
            FieldType {
                tp: TYPE_ENUM,
                flen: -1,
                elems: Some(elems),
                elem_is_binary_lit: binary,
                ..string_ft()
            }
        }
        "SET" => {
            let (elems, binary) = parse_elems_with_binary(type_name)?;
            FieldType {
                tp: TYPE_SET,
                flen: -1,
                elems: Some(elems),
                elem_is_binary_lit: binary,
                ..string_ft()
            }
        }
        "BIT" => FieldType {
            tp: TYPE_BIT,
            // BIT(n) is an unsigned big-endian bit field (TiDB unsignedFlag).
            flen: parse_len(type_name).unwrap_or(1),
            flag: 32,
            ..numeric_ft()
        },
        // BINARY(n) / VARBINARY(n) carry the `binary` charset: values compare
        // byte-wise and BINARY(n) pads to n bytes on write.
        "BINARY" => FieldType {
            tp: TYPE_STRING,
            flen: parse_len(type_name).unwrap_or(1),
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        "VARBINARY" => FieldType {
            tp: TYPE_VAR_STRING,
            flen: parse_len(type_name).unwrap_or(-1),
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        "BLOB" => FieldType {
            tp: match parse_len(type_name) {
                Some(len) => lob_tp_for_len(len),
                None => TYPE_BLOB,
            },
            flen: -1,
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        "TINYBLOB" => FieldType {
            tp: TYPE_TINY_BLOB,
            flen: -1,
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        "MEDIUMBLOB" => FieldType {
            tp: TYPE_MEDIUM_BLOB,
            flen: -1,
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        // TINYTEXT/MEDIUMTEXT/LONGTEXT share the blob type codes in MySQL and
        // differ only by the utf8mb4 charset (TiDB maps them the same way).
        "TINYTEXT" => FieldType {
            tp: TYPE_TINY_BLOB,
            flen: -1,
            ..string_ft()
        },
        "MEDIUMTEXT" => FieldType {
            tp: TYPE_MEDIUM_BLOB,
            flen: -1,
            ..string_ft()
        },
        "LONGTEXT" => FieldType {
            tp: TYPE_LONG_BLOB,
            flen: -1,
            ..string_ft()
        },
        "LONGBLOB" => FieldType {
            tp: TYPE_LONG_BLOB,
            flen: -1,
            charset: "binary".to_string(),
            collate: "binary".to_string(),
            ..string_ft()
        },
        // JSON is stored as its serialized text for now; comparisons still use
        // the string path until the binary JSON codec lands.
        "JSON" => FieldType {
            tp: TYPE_JSON,
            flen: -1,
            ..string_ft()
        },
        "DATE" => FieldType {
            tp: TYPE_DATE,
            flen: 10,
            ..string_ft()
        },
        "DATETIME" => {
            let fsp = parse_flen_decimal(type_name)
                .map(|(fsp, _)| fsp)
                .unwrap_or(0);
            FieldType {
                tp: TYPE_DATETIME,
                flen: 19 + if fsp > 0 { 1 + fsp } else { 0 },
                decimal: fsp,
                ..string_ft()
            }
        }
        "TIMESTAMP" => {
            let fsp = parse_flen_decimal(type_name)
                .map(|(fsp, _)| fsp)
                .unwrap_or(0);
            FieldType {
                tp: TYPE_TIMESTAMP,
                flen: 19 + if fsp > 0 { 1 + fsp } else { 0 },
                decimal: fsp,
                ..string_ft()
            }
        }
        "TIME" => {
            let fsp = parse_flen_decimal(type_name)
                .map(|(fsp, _)| fsp)
                .unwrap_or(0);
            FieldType {
                tp: TYPE_DURATION,
                flen: 10 + if fsp > 0 { 1 + fsp } else { 0 },
                decimal: fsp,
                ..string_ft()
            }
        }
        "YEAR" => FieldType {
            tp: TYPE_YEAR,
            flen: 4,
            ..numeric_ft()
        },
        _ => {
            return Err(Error::unsupported(format!("column type {type_name}")));
        }
    };
    ft.unsigned = unsigned;
    if unsigned {
        ft.flag |= 32;
    }
    Ok(ft)
}

fn numeric_ft() -> FieldType {
    FieldType {
        charset: "binary".to_string(),
        collate: "binary".to_string(),
        decimal: 0,
        ..FieldType::default()
    }
}

/// The smallest BLOB/TEXT storage class that fits `len` bytes.
fn lob_tp_for_len(len: i32) -> u8 {
    if len <= 255 {
        TYPE_TINY_BLOB
    } else if len <= 65_535 {
        TYPE_BLOB
    } else if len <= 16_777_215 {
        TYPE_MEDIUM_BLOB
    } else {
        TYPE_LONG_BLOB
    }
}

fn string_ft() -> FieldType {
    FieldType {
        charset: "utf8mb4".to_string(),
        collate: "utf8mb4_bin".to_string(),
        decimal: 0,
        ..FieldType::default()
    }
}

fn split_type(upper: &str) -> (&str, &str) {
    if let Some(idx) = upper.find('(') {
        (&upper[..idx], &upper[idx..])
    } else if let Some(idx) = upper.find(' ') {
        (&upper[..idx], &upper[idx..])
    } else {
        (upper, "")
    }
}

fn parse_len(type_name: &str) -> Option<i32> {
    parse_flen_decimal(type_name).map(|(flen, _)| flen)
}

/// `ENUM` / `SET` reject a member list with duplicates (MySQL 1291). A
/// case-insensitive collation makes `'e'` and `'E'` the same member.
fn validate_enum_set_members(col_name: &str, ft: &FieldType) -> Result<()> {
    let kind = match ft.tp {
        sealdb_types::field_type::TYPE_ENUM => "ENUM",
        sealdb_types::field_type::TYPE_SET => "SET",
        _ => return Ok(()),
    };
    let Some(elems) = &ft.elems else {
        return Ok(());
    };
    let ci = ft.collate.ends_with("_ci") || ft.collate == "gbk_chinese_ci";
    let mut seen = std::collections::HashSet::new();
    for elem in elems {
        let key = if ci {
            elem.to_ascii_lowercase()
        } else {
            elem.clone()
        };
        if !seen.insert(key) {
            return Err(Error::new(
                ERR_DUPLICATED_VALUE_IN_TYPE,
                format!("Column '{col_name}' has duplicated value '{elem}' in {kind}"),
            ));
        }
    }
    Ok(())
}

/// Parse the member list of `ENUM('a','b')` / `SET('a','b')`, preserving case.
/// Members are stored unquoted; `''` and `\'` both decode to a quote.
///
/// Members rewritten from binary literals (`0xBAEC`, `b'101'`) keep their raw
/// bytes as Latin-1 characters and are flagged for
/// `decode_enum_set_binary_literals`; TiDB `FieldType.ElemIsBinaryLit`.
fn parse_elems_with_binary(type_name: &str) -> Result<(Vec<String>, Vec<bool>)> {
    let start = type_name
        .find('(')
        .ok_or_else(|| Error::unsupported(format!("column type {type_name}")))?;
    let end = type_name
        .rfind(')')
        .ok_or_else(|| Error::unsupported(format!("column type {type_name}")))?;
    let inner = &type_name[start + 1..end];
    let mut out = Vec::new();
    let mut binary = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        if first != '\'' && first != '"' {
            // Unquoted member (`enum(0x61, 1)`): hex literal or bare word.
            let mut raw = String::new();
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                raw.push(c);
                chars.next();
            }
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (member, is_binary) = match raw_bytes(raw) {
                Some(bytes) => (latin1_string(&bytes), true),
                None => (raw.to_string(), false),
            };
            out.push(member);
            binary.push(is_binary);
            continue;
        }
        chars.next();
        let mut member = String::new();
        loop {
            match chars.next() {
                None => return Err(Error::unsupported(format!("column type {type_name}"))),
                Some('\\') => {
                    if let Some(c) = chars.next() {
                        member.push(c);
                    }
                }
                Some(c) if c == first => {
                    // Doubled quote is an escaped quote; otherwise the member ends.
                    if chars.peek() == Some(&first) {
                        chars.next();
                        member.push(c);
                    } else {
                        break;
                    }
                }
                Some(c) => member.push(c),
            }
        }
        let is_binary =
            member.starts_with(sealdb_parser::sqlparser_bridge::lower::BINARY_ELEM_MARKER);
        if is_binary {
            member.remove(0);
        }
        out.push(member);
        binary.push(is_binary);
    }
    Ok((out, binary))
}

/// Raw bytes → a string whose chars are the byte values (TiDB binary literal).
fn latin1_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// `0x6162` / `X'6162'` member literal → its raw bytes.
fn raw_bytes(raw: &str) -> Option<Vec<u8>> {
    let hex = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .or_else(|| raw.strip_prefix("x'").and_then(|s| s.strip_suffix('\'')))
        .or_else(|| raw.strip_prefix("X'").and_then(|s| s.strip_suffix('\'')))?;
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let digits = hex.as_bytes();
    let mut i = 0;
    while i + 1 < digits.len() {
        let hi = (digits[i] as char).to_digit(16)?;
        let lo = (digits[i + 1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(bytes)
}

/// Inverse of `latin1_string` for the binary-literal member representation.
fn latin1_bytes(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// Decode charset bytes to UTF-8 the way TiDB `OpDecodeReplace` does: each
/// invalid unit becomes `?` (not `U+FFFD`).
fn decode_charset_replace(bytes: &[u8], charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "gbk" => sealdb_parser::charset::encoding_gbk::decode(bytes),
        "latin1" | "binary" | "ascii" if bytes.iter().all(u8::is_ascii) => latin1_string(bytes),
        _ => decode_utf8_replace(bytes),
    }
}

fn decode_utf8_replace(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break;
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0 {
                    out.push_str(std::str::from_utf8(&rest[..valid]).expect("valid prefix"));
                }
                let bad = err.error_len().unwrap_or(rest.len() - valid);
                for _ in 0..bad {
                    out.push('?');
                }
                rest = &rest[valid + bad..];
            }
        }
    }
    out
}

/// TiDB `decodeEnumSetBinaryLiteralToUTF8`: rewrite binary-literal ENUM/SET
/// members from the column charset to UTF-8 (`enum(0xBAEC)` under `gbk_bin` is
/// `'红'`, under `utf8mb4` the bytes are decoded with `?` replacements).
pub(crate) fn decode_enum_set_binary_literals(ft: &mut FieldType) {
    if !matches!(ft.tp, TYPE_ENUM | TYPE_SET) || ft.elem_is_binary_lit.is_empty() {
        return;
    }
    if let Some(elems) = ft.elems.as_mut() {
        let charset = ft.charset.clone();
        for (elem, is_binary) in elems.iter_mut().zip(ft.elem_is_binary_lit.iter()) {
            if !*is_binary {
                continue;
            }
            if let Some(bytes) = latin1_bytes(elem) {
                *elem = decode_charset_replace(&bytes, &charset);
            }
        }
    }
    ft.elem_is_binary_lit.clear();
}

/// `FLOAT(M)` / `DOUBLE(M)` carry a display width only: without a second
/// argument the scale stays unspecified (`-1`), so assignments keep their
/// fractional part instead of rounding to an integer (`FLOAT(M,0)` still
/// rounds).
fn parse_flen_decimal_float(type_name: &str) -> Option<(i32, i32)> {
    let (flen, decimal) = parse_flen_decimal(type_name)?;
    let close = type_name.find(')')?;
    let has_scale = type_name[..close].contains(',');
    Some((flen, if has_scale { decimal } else { -1 }))
}

/// Parse `TYPE(flen)` / `TYPE(flen, decimal)` — TiDB FieldType Flen/Decimal.
fn parse_flen_decimal(type_name: &str) -> Option<(i32, i32)> {
    let start = type_name.find('(')?;
    let end = type_name.find(')')?;
    let mut parts = type_name[start + 1..end].split(',');
    let flen = parts.next()?.trim().parse().ok()?;
    let decimal = parts
        .next()
        .map(|p| p.trim().parse().ok())
        .unwrap_or(Some(0))?;
    Some((flen, decimal))
}

#[cfg(test)]
mod precise_type_tests {
    use super::field_type_from_name;

    #[test]
    fn table_charset_applies_to_columns() {
        use sealdb_parser::Parser;
        use sealdb_parser::ast::Statement;
        let parsed = Parser::new()
            .parse_one_stmt("CREATE TABLE t (a VARCHAR(10)) CHARSET ascii")
            .unwrap();
        let Statement::CreateTable(stmt) = parsed else {
            panic!("not a CREATE TABLE");
        };
        let info = super::build_table_info(&stmt, 1).unwrap();
        assert_eq!(info.charset, "ascii");
        assert_eq!(info.columns[0].field_type.charset, "ascii");
        assert_eq!(info.columns[0].field_type.collate, "ascii_bin");
    }

    #[test]
    fn bigint_unsigned_and_decimal_scale() {
        let u = field_type_from_name("BIGINT UNSIGNED").unwrap();
        assert!(u.is_unsigned(), "unsigned flag for BIGINT UNSIGNED");
        assert_eq!(u.flag & 32, 32);
        let d = field_type_from_name("DECIMAL(21,1)").unwrap();
        assert_eq!(d.flen, 21);
        assert_eq!(d.decimal, 1);
        let u2 = field_type_from_name("BIGINT").unwrap();
        assert!(!u2.is_unsigned());
    }

    #[test]
    fn scratch_binary_json_columns() {
        use sealdb_parser::Parser;
        use sealdb_parser::ast::Statement;
        let parsed = Parser::new()
            .parse_one_stmt("CREATE TABLE t(a binary, b json, c char charset gbk)")
            .unwrap();
        let Statement::CreateTable(stmt) = parsed else {
            panic!()
        };
        let info = super::build_table_info(&stmt, 1).unwrap();
        for col in &info.columns {
            println!(
                "{} tp={} charset={} collate={} eval={:?}",
                col.name.o(),
                col.field_type.tp,
                col.field_type.charset,
                col.field_type.collate,
                col.field_type.eval_type()
            );
        }
    }

    #[test]
    fn enum_set_binary_literals_decode_from_column_charset() {
        use sealdb_parser::Parser;
        use sealdb_parser::ast::Statement;
        // TiDB `decodeEnumSetBinaryLiteralToUTF8`: a hex member is raw bytes in
        // the column charset, rewritten to UTF-8 at DDL time.
        let parsed = Parser::new()
            .parse_one_stmt(
                "CREATE TABLE t(f set(0xD2BB, 0xC8FD), e enum(0xBAEC,0x6A59)) COLLATE gbk_bin",
            )
            .unwrap();
        let Statement::CreateTable(stmt) = parsed else {
            panic!("not a CREATE TABLE");
        };
        let info = super::build_table_info(&stmt, 1).unwrap();
        assert_eq!(
            info.columns[0].field_type.elems.as_deref(),
            Some(["一".to_string(), "三".to_string()].as_slice())
        );
        assert_eq!(
            info.columns[1].field_type.elems.as_deref(),
            Some(["红".to_string(), "jY".to_string()].as_slice())
        );
        // Under utf8mb4 the same bytes are invalid UTF-8: TiDB substitutes one
        // `?` per bad byte instead of failing.
        let parsed = Parser::new()
            .parse_one_stmt("CREATE TABLE t2(e enum(0xBAEC,0x6A59))")
            .unwrap();
        let Statement::CreateTable(stmt) = parsed else {
            panic!("not a CREATE TABLE");
        };
        let info = super::build_table_info(&stmt, 1).unwrap();
        assert_eq!(
            info.columns[0].field_type.elems.as_deref(),
            Some(["??".to_string(), "jY".to_string()].as_slice())
        );
    }
}

/// Parse the `AUTO_ID_CACHE` table option. TiDB stores it as an `int64`, so a
/// larger value is 1105 `table option auto_id_cache overflows int64`.
pub(crate) fn parse_auto_id_cache(raw: &str) -> Result<i64> {
    raw.trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .parse::<i64>()
        .map_err(|_| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                "table option auto_id_cache overflows int64",
            )
        })
}

use sealdb_errno::{
    ERR_BAD_FIELD, ERR_DUP_FIELDNAME, ERR_GENERATED_COLUMN_FUNCTION_NOT_ALLOWED,
    ERR_GENERATED_COLUMN_REF_AUTO_INC, ERR_INVALID_DEFAULT, ERR_NO_SUCH_TABLE,
    ERR_TOO_BIG_DISPLAYWIDTH, ERR_TTL_INVALID_COLUMN, ERR_TTL_UNSUPPORTED_PK_TYPE,
    ERR_VIEW_WRONG_LIST, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, Mutator, TableInfo};
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, ValueExpr};
use sealdb_parser::ast::{ColumnDef, CreateTableStmt, SelectStmt};
use sealdb_parser::model::CIStr;
use sealdb_types::Datum;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_DURATION,
    TYPE_ENUM, TYPE_FLOAT, TYPE_INT24, TYPE_JSON, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG,
    TYPE_MEDIUM_BLOB, TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP,
    TYPE_TINY, TYPE_TINY_BLOB, TYPE_VAR_STRING, TYPE_YEAR,
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
    let known: Vec<String> = columns.iter().map(|c| c.name.o().to_string()).collect();
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
        for (_table, name) in expr.referenced_columns() {
            if !known.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Unknown column '{name}' in 'generated column function'"),
                ));
            }
        }
    }
    Ok(())
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
    } else if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        rest
    } else {
        return None;
    };
    if text.starts_with("b'") || text.starts_with("B'") {
        if !text.ends_with('\'') {
            return None;
        }
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
    if field_type.tp == TYPE_BIT {
        // `b'111'` on `BIT(2)`: the bit-string's value must fit the width.
        if let Some(bits) = bit_literal_bits(trimmed) {
            let width = field_type.flen.max(1).min(128) as u32;
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
    let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    else {
        return Ok(());
    };
    let value = inner.replace("''", "'");
    let text = value.trim();
    if text.is_empty() {
        return Ok(());
    }
    let fits = if let Some((low, high)) = integer_bounds(field_type) {
        let parsed = text
            .parse::<i128>()
            .ok()
            .or_else(|| text.parse::<f64>().ok().map(|v| v.trunc() as i128));
        parsed.is_some_and(|v| v >= low && v <= high)
    } else {
        match field_type.tp {
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

/// Normalize a quoted literal default to the column type, the way TiDB does at
/// DDL time: temporal defaults are parsed and rounded to the column's
/// fractional precision (`DEFAULT '12:12:13.4'` on `TIME` becomes `12:12:13`),
/// and date/datetime defaults are re-rendered in canonical form.
pub(crate) fn normalize_default(field_type: &FieldType, default: &str) -> String {
    let trimmed = default.trim();
    let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    else {
        return default.to_string();
    };
    let value = inner.replace("''", "'");
    let fsp = field_type.decimal.clamp(0, 6);
    match field_type.tp {
        TYPE_DURATION => normalize_duration_default(&value, fsp)
            .map(|text| format!("'{text}'"))
            .unwrap_or_else(|| default.to_string()),
        TYPE_DATETIME | TYPE_TIMESTAMP => match sealdb_types::parse_datetime_literal(&value)
            .or_else(|_| sealdb_types::parse_datetime_literal(&format!("{value} 00:00:00")))
        {
            Ok(time) => format!("'{}'", round_time(time, fsp).to_string()),
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

pub fn build_table_info(stmt: &CreateTableStmt, table_id: i64) -> Result<TableInfo> {
    if stmt.cols.is_empty() {
        return Err(Error::unsupported("CREATE TABLE without columns"));
    }
    let mut columns = Vec::with_capacity(stmt.cols.len());
    for (offset, col) in stmt.cols.iter().enumerate() {
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
        if let Some(generated) = &col.generated {
            validate_generated_expr(&col.name, &generated.expr)?;
            info.generated_expr = Some(generated.expr.clone());
            info.generated_stored = generated.stored;
        }
        info.default_expr = col
            .default
            .as_deref()
            .map(|default| normalize_default(&info.field_type, default));
        info.auto_increment = col.auto_increment;
        info.auto_random = col.auto_random;
        validate_display_width(info.name.o(), &info.field_type)?;
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
        if col.not_null {
            info.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
        }
        columns.push(info);
    }
    validate_generated_auto_increment(&columns, None)?;
    validate_generated_column_refs(&columns, None)?;
    let mut info = TableInfo::new(table_id, stmt.table.name.clone(), columns);
    for index in stmt.indices.iter() {
        let (idx_cols, hidden) =
            crate::expr_index::resolve_index_parts(&info, &index.name, &index.columns)?;
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
                .with_invisible(index.invisible),
        );
        info.max_idx_id = info.max_idx_id.max(idx_id);
        for c in &mut info.columns {
            if pk_col_offsets.contains(&c.offset) {
                c.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
            }
        }
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
    for (n, fk) in stmt.foreign_keys.iter().enumerate() {
        let name = if fk.name.is_empty() {
            format!("{}_ibfk_{}", info.name.o(), n + 1)
        } else {
            fk.name.clone()
        };
        let cols = fk
            .columns
            .iter()
            .map(|col_name| {
                info.columns
                    .iter()
                    .find(|c| c.name.o().eq_ignore_ascii_case(col_name))
                    .map(|c| (c.name.o().to_string(), c.offset))
                    .ok_or_else(|| {
                        Error::new(
                            sealdb_errno::ERR_BAD_FIELD,
                            format!("Unknown column '{col_name}' in foreign key '{name}'"),
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if !fk_index_covers(&info, &cols) {
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
            ref_cols: fk.ref_columns.clone(),
            on_update_cascade: fk.on_update_cascade,
            on_delete_cascade: fk.on_delete_cascade,
        });
    }
    Ok(info)
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
        not_null,
        charset: None,
        collate: None,
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
    let name = match &field.as_name {
        Some(name) if !name.is_empty() => name.clone(),
        _ => field.expr.name(),
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
        ExprNode::Cast(cast) => Ok(cast.target.clone()),
        ExprNode::Value(ValueExpr { datum }) => Ok(type_name_from_datum(datum).to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("COUNT") => Ok("bigint".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("SUM") => Ok("decimal".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("AVG") => Ok("double".to_string()),
        ExprNode::Window(_) => Ok("bigint".to_string()),
        _ => Ok("varchar".to_string()),
    }
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
        Datum::I64(_) | Datum::U64(_) | Datum::Bit(_) => "int",
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
            let (flen, decimal) = parse_flen_decimal(type_name).unwrap_or((12, -1));
            FieldType {
                tp: TYPE_FLOAT,
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "REAL" if sealdb_expression::sql_mode_flag("REAL_AS_FLOAT") => {
            let (flen, decimal) = parse_flen_decimal(type_name).unwrap_or((12, -1));
            FieldType {
                tp: TYPE_FLOAT,
                flen,
                decimal,
                ..numeric_ft()
            }
        }
        "DOUBLE" | "REAL" => {
            let (flen, decimal) = parse_flen_decimal(type_name).unwrap_or((22, -1));
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
        "CHAR" | "VARCHAR" | "TEXT" | "STRING" => FieldType {
            tp: TYPE_VAR_STRING,
            flen: parse_len(type_name).unwrap_or(-1),
            ..string_ft()
        },
        // ENUM/SET store their member labels as text; comparisons keep the
        // label until numeric-vs-enum coercion lands (TiDB numeric path).
        "ENUM" => FieldType {
            tp: TYPE_ENUM,
            flen: -1,
            elems: Some(parse_elems(type_name)?),
            ..string_ft()
        },
        "SET" => FieldType {
            tp: TYPE_SET,
            flen: -1,
            elems: Some(parse_elems(type_name)?),
            ..string_ft()
        },
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
            tp: TYPE_BLOB,
            flen: -1,
            ..string_ft()
        },
        "TINYBLOB" => FieldType {
            tp: TYPE_TINY_BLOB,
            flen: -1,
            ..string_ft()
        },
        "MEDIUMBLOB" => FieldType {
            tp: TYPE_MEDIUM_BLOB,
            flen: -1,
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

/// Parse the member list of `ENUM('a','b')` / `SET('a','b')`, preserving case.
/// Members are stored unquoted; `''` and `\'` both decode to a quote.
fn parse_elems(type_name: &str) -> Result<Vec<String>> {
    let start = type_name
        .find('(')
        .ok_or_else(|| Error::unsupported(format!("column type {type_name}")))?;
    let end = type_name
        .rfind(')')
        .ok_or_else(|| Error::unsupported(format!("column type {type_name}")))?;
    let inner = &type_name[start + 1..end];
    let mut out = Vec::new();
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
            out.push(hex_literal_to_string(raw).unwrap_or_else(|| raw.to_string()));
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
        out.push(member);
    }
    Ok(out)
}

/// `0x6162` / `X'6162'` member literal → decoded UTF-8 text.
fn hex_literal_to_string(raw: &str) -> Option<String> {
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
    String::from_utf8(bytes).ok()
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
}

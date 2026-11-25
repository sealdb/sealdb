//! TiDB expression indexes: `KEY idx ((`a` + `b` + 1))`.
//!
//! TiDB materialises every expression key part as a hidden VIRTUAL generated
//! column named `_V$_<index>_<pos>` and points the index at that column
//! (`buildTableInfoWithCheck` / `checkAddExpressionIndex`). Keeping the column
//! model means key encoding, uniqueness checks, backfill and DML maintenance
//! all reuse the plain-column machinery; only the visibility rules (the
//! column must never reach clients) and the type inference are new.

use crate::create_table::field_type_from_name;
use sealdb_errno::{
    ERR_BAD_FIELD, ERR_BLOB_USED_AS_KEY, ERR_DUP_FIELDNAME, ERR_FUNCTIONAL_INDEX_ON_AUTO_INCREMENT,
    ERR_FUNCTIONAL_INDEX_ON_BLOB, ERR_FUNCTIONAL_INDEX_ON_BLOB_RESULT,
    ERR_FUNCTIONAL_INDEX_ON_FIELD, ERR_FUNCTIONAL_INDEX_ON_JSON_OR_GEOMETRY_FUNCTION,
    ERR_FUNCTIONAL_INDEX_ROW_VALUE_IS_NOT_ALLOWED, ERR_KEY_COLUMN_DOES_NOT_EXIST, ERR_TOO_LONG_KEY,
    ERR_WRONG_KEY_COLUMN, ERR_WRONG_STORAGE_ENGINE_KEY, ERR_WRONG_SUB_KEY, Error, Result,
};
use sealdb_meta::{ColumnInfo, IndexColumn, TableInfo};
use sealdb_parser::ast::ExprNode;
use sealdb_parser::ast::IndexPart;
use sealdb_parser::opcode::Op;
use sealdb_parser::sqlparser_bridge::lower::parse_expr;
use sealdb_types::Datum;
use sealdb_types::field_type::FieldType;

/// TiDB `buildIndexInfo`: hidden column name for the `pos`-th expression part.
pub(crate) fn hidden_column_name(index_name: &str, pos: usize) -> String {
    format!("_V$_{index_name}_{pos}")
}

/// Turn parsed index key parts into `IndexColumn`s, materialising one hidden
/// generated column per expression part.
///
/// Returns the hidden columns to append to the table (in order); the caller
/// owns `max_col_id` bookkeeping.
pub(crate) fn resolve_index_parts(
    info: &TableInfo,
    index_name: &str,
    parts: &[IndexPart],
    primary: bool,
) -> Result<(Vec<IndexColumn>, Vec<ColumnInfo>)> {
    let mut hidden: Vec<ColumnInfo> = Vec::new();
    let mut columns = Vec::with_capacity(parts.len());
    let mut seen: Vec<String> = Vec::with_capacity(parts.len());
    // TiDB `buildIndexColumns`: the sum of all key parts must fit 3072 bytes
    // (under the default strict mode the DDL fails instead of truncating).
    let mut sum_length: i64 = 0;
    for (pos, part) in parts.iter().enumerate() {
        // TiDB `buildIndexInfo`: a key may not name the same column twice
        // (comparison is case-insensitive, the message keeps the spelling).
        if let IndexPart::Column(name) | IndexPart::Prefix { name, .. } = part {
            if seen.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
                return Err(Error::new(
                    ERR_DUP_FIELDNAME,
                    format!("Duplicate column name '{name}'"),
                ));
            }
            seen.push(name.clone());
        }
        match part {
            IndexPart::Column(name) | IndexPart::Prefix { name, .. } => {
                // TiDB `checkIndexPartLength`: a zero-length key part is
                // `ErrWrongKeyColumn` (1391).
                if part.prefix_length() == 0 && matches!(part, IndexPart::Prefix { .. }) {
                    return Err(Error::new(
                        ERR_WRONG_KEY_COLUMN,
                        format!("Key part '{name}' length cannot be 0"),
                    ));
                }
                let col = info
                    .columns
                    .iter()
                    .find(|c| !c.hidden && c.name.o().eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        // TiDB `indexColumnsLen` / `buildIndexColumns`: a
                        // missing PRIMARY key column reports the build
                        // message, a secondary key the length-check message.
                        if primary {
                            Error::new(
                                ERR_KEY_COLUMN_DOES_NOT_EXIST,
                                format!("Key column '{name}' doesn't exist in table"),
                            )
                        } else {
                            Error::new(
                                ERR_KEY_COLUMN_DOES_NOT_EXIST,
                                format!("column does not exist: {name}"),
                            )
                        }
                    })?;
                let mut ic = IndexColumn::new(col.name.o(), col.offset);
                ic.length = part.prefix_length();
                check_index_part_length(col, ic.length)?;
                sum_length += index_column_length(col, ic.length);
                check_index_sum_length(sum_length, parts.len())?;
                columns.push(ic);
            }
            IndexPart::Expression(text) => {
                if !sealdb_expression::allow_auto_increment_in_generated() {
                    let expr = parse_expr(text)?;
                    for (_table, col_name) in expr.referenced_columns() {
                        if info.columns.iter().any(|c| {
                            !c.hidden
                                && c.auto_increment
                                && c.name.o().eq_ignore_ascii_case(&col_name)
                        }) {
                            return Err(Error::new(
                                ERR_FUNCTIONAL_INDEX_ON_AUTO_INCREMENT,
                                format!(
                                    "Expression index '{index_name}' cannot refer to an auto-increment column"
                                ),
                            ));
                        }
                    }
                }
                let name = hidden_column_name(index_name, pos);
                // TiDB rejects the index when a user column already owns the
                // generated hidden-column name.
                if info
                    .columns
                    .iter()
                    .any(|c| c.name.o().eq_ignore_ascii_case(&name))
                {
                    return Err(Error::new(
                        ERR_DUP_FIELDNAME,
                        format!("Duplicate column name '{name}'"),
                    ));
                }
                let offset = (info.columns.len() + hidden.len()) as i32;
                let id = info.max_col_id + 1 + hidden.len() as i64;
                let field_type = expression_field_type(text, info, index_name)?;
                // A multi-valued index part (`CAST(json_col AS … ARRAY)`) stores
                // array elements; the whole-part BLOB/TEXT/JSON rules do not
                // apply to it.
                let multi_valued_part = is_multi_valued_part(text);
                // TiDB `checkIndexColumn`: a zero-width CHAR/VARCHAR result
                // (e.g. `concat('')`) cannot be indexed by the engine.
                if !multi_valued_part
                    && field_type.flen == 0
                    && matches!(
                        field_type.tp,
                        sealdb_types::field_type::TYPE_STRING
                            | sealdb_types::field_type::TYPE_VARCHAR
                            | sealdb_types::field_type::TYPE_VAR_STRING
                    )
                {
                    // TiDB reports the hidden column's `GeneratedExprString`,
                    // which carries `_utf8mb4` string introducers.
                    let display = parse_expr(text)
                        .map(|expr| expr.restore_string_generated())
                        .unwrap_or_else(|_| text.to_string());
                    return Err(Error::new(
                        ERR_FUNCTIONAL_INDEX_ON_BLOB,
                        format!("The used storage engine cannot index the expression '{display}'"),
                    ));
                }
                // TiDB `checkIndexColumn` on the hidden generated column: JSON
                // results are 3753, length-less BLOB/TEXT results are 3757.
                if !multi_valued_part {
                    if field_type.tp == sealdb_types::field_type::TYPE_JSON {
                        return Err(Error::new(
                            ERR_FUNCTIONAL_INDEX_ON_JSON_OR_GEOMETRY_FUNCTION,
                            "Cannot create an expression index on a function that returns a JSON or GEOMETRY value",
                        ));
                    }
                    if crate::create_table::is_lob_type(&field_type) {
                        return Err(Error::new(
                            ERR_FUNCTIONAL_INDEX_ON_BLOB_RESULT,
                            "Cannot create an expression index on an expression that returns a BLOB or TEXT. Please consider using CAST",
                        ));
                    }
                }
                let mut col = ColumnInfo::new(id, offset, name.clone(), field_type);
                col.generated_expr = Some(text.clone());
                col.generated_stored = false;
                col.hidden = true;
                sum_length += index_column_length(&col, -1);
                check_index_sum_length(sum_length, parts.len())?;
                hidden.push(col);
                columns.push(IndexColumn::new(name, offset));
            }
        }
    }
    Ok((columns, hidden))
}

/// TiDB `checkIndexColumn`: a key part with an explicit prefix must name a
/// string/BLOB column and fit its declared length and the 3072-byte key limit;
/// a BLOB/TEXT column always needs a prefix (1170).
fn check_index_part_length(col: &ColumnInfo, length: i32) -> Result<()> {
    let ft = &col.field_type;
    // TiDB `checkIndexColumn`: a zero-width CHAR/VARCHAR/BINARY column cannot
    // be indexed (1167).
    if ft.flen == 0
        && matches!(
            ft.tp,
            sealdb_types::field_type::TYPE_STRING
                | sealdb_types::field_type::TYPE_VARCHAR
                | sealdb_types::field_type::TYPE_VAR_STRING
        )
    {
        return Err(Error::new(
            ERR_WRONG_STORAGE_ENGINE_KEY,
            format!(
                "The used storage engine can't index column '{}'",
                col.name.o()
            ),
        ));
    }
    let blob = matches!(
        ft.tp,
        sealdb_types::field_type::TYPE_BLOB
            | sealdb_types::field_type::TYPE_TINY_BLOB
            | sealdb_types::field_type::TYPE_MEDIUM_BLOB
            | sealdb_types::field_type::TYPE_LONG_BLOB
    );
    // `TEXT` / `BLOB` are length-less string types.
    let text = ft.tp == sealdb_types::field_type::TYPE_VAR_STRING && ft.flen < 0;
    if (blob || text) && length < 0 {
        return Err(Error::new(
            ERR_BLOB_USED_AS_KEY,
            format!(
                "BLOB/TEXT column '{}' used in key specification without a key length",
                col.name.o()
            ),
        ));
    }
    if length < 0 {
        return Ok(());
    }
    let correct_prefix = matches!(
        ft.tp,
        sealdb_types::field_type::TYPE_STRING
            | sealdb_types::field_type::TYPE_VARCHAR
            | sealdb_types::field_type::TYPE_VAR_STRING
    ) || blob;
    if !correct_prefix {
        return Err(Error::new(ERR_WRONG_SUB_KEY, wrong_sub_key()));
    }
    // `CHAR(n)` cannot carry a prefix longer than `n` (MySQL `VARCHAR` can).
    if ft.tp == sealdb_types::field_type::TYPE_STRING && ft.flen < length {
        return Err(Error::new(ERR_WRONG_SUB_KEY, wrong_sub_key()));
    }
    let bytes = i64::from(length) * i64::from(charset_maxlen(&ft.charset));
    if bytes > 3072 {
        return Err(Error::new(
            ERR_TOO_LONG_KEY,
            format!("Specified key was too long ({bytes} bytes); max key length is 3072 bytes"),
        ));
    }
    Ok(())
}

fn wrong_sub_key() -> &'static str {
    "Incorrect prefix key; the used key part isn't a string, the used length is longer than the key part, or the storage engine doesn't support unique prefix keys"
}

/// Maximum bytes per character for the collation charsets TiDB accepts.
fn charset_maxlen(charset: &str) -> i32 {
    match charset.to_ascii_lowercase().as_str() {
        "binary" | "ascii" | "latin1" => 1,
        "gbk" | "gb18030" | "big5" | "sjis" | "cp932" | "eucjpms" => 2,
        "utf8" | "utf8mb3" => 3,
        _ => 4,
    }
}

/// TiDB `getIndexColumnLength`: bytes one index key part occupies. `prefix` is
/// -1 when the whole column is indexed; `flen` is the declared char length for
/// string types and the precision for decimals.
fn index_column_length(col: &ColumnInfo, prefix: i32) -> i64 {
    use sealdb_types::field_type::*;
    let ft = &col.field_type;
    let length = if prefix != -1 { prefix } else { ft.flen };
    if length < 0 {
        return 0;
    }
    let length = i64::from(length);
    match ft.tp {
        TYPE_BIT => (length + 7) >> 3,
        TYPE_VARCHAR | TYPE_STRING | TYPE_VAR_STRING | TYPE_TINY_BLOB | TYPE_BLOB
        | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB => i64::from(charset_maxlen(&ft.charset)) * length,
        TYPE_TINY => 1,
        TYPE_SHORT => 2,
        TYPE_INT24 => 3,
        TYPE_LONG => 4,
        TYPE_LONG_LONG => 8,
        // TiDB spills to the double length past 24 digits of precision.
        TYPE_FLOAT => {
            if length <= 24 {
                4
            } else {
                8
            }
        }
        TYPE_DOUBLE => 8,
        TYPE_NEW_DECIMAL | TYPE_DECIMAL => {
            // TiDB `calcBytesLengthForDecimal`.
            (length / 9 * 4) + ((length % 9) + 1) / 2
        }
        TYPE_YEAR => 1,
        TYPE_DATE | TYPE_DURATION => 3,
        TYPE_DATETIME => 8,
        TYPE_TIMESTAMP => 4,
        TYPE_ENUM => 2,
        TYPE_SET => 8,
        _ => length,
    }
}

/// Re-check every index on `info` after a column charset/type change. TiDB
/// re-runs `buildIndexColumns`, so a widened key fails the DDL with 1071.
pub(crate) fn revalidate_index_lengths(info: &TableInfo) -> Result<()> {
    for index in &info.indices {
        let mut sum = 0i64;
        for ic in &index.columns {
            if let Some(col) = info.columns.iter().find(|c| c.offset == ic.offset) {
                sum += index_column_length(col, ic.length);
            }
        }
        check_index_sum_length(sum, index.columns.len())?;
    }
    Ok(())
}

/// TiDB `buildIndexColumns`: a key longer than 3072 bytes fails the DDL under
/// strict mode, or whenever it spans multiple key parts.
fn check_index_sum_length(sum: i64, parts: usize) -> Result<()> {
    if sum > 3072
        && (parts > 1
            || sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
            || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES"))
    {
        return Err(Error::new(
            ERR_TOO_LONG_KEY,
            format!("Specified key was too long ({sum} bytes); max key length is 3072 bytes"),
        ));
    }
    Ok(())
}

/// Field type TiDB derives for an expression index part.
fn expression_field_type(text: &str, info: &TableInfo, index_name: &str) -> Result<FieldType> {
    let expr = parse_expr(text)?;
    infer(&expr, info, index_name, text)
}

fn column_field_type<'a>(info: &'a TableInfo, name: &str) -> Option<&'a FieldType> {
    info.columns
        .iter()
        .find(|c| !c.hidden && c.name.o().eq_ignore_ascii_case(name))
        .map(|c| &c.field_type)
}

/// Maximum width of a string-valued argument when it is statically known: a
/// visible column with a declared length, or a constant literal.
fn known_string_flen(expr: &ExprNode, info: &TableInfo) -> Option<i32> {
    match expr {
        ExprNode::Column(c) => column_field_type(info, &c.name)
            .map(|ft| ft.flen)
            .filter(|flen| *flen > 0),
        ExprNode::Value(v) => match &v.datum {
            Datum::Bytes(bytes) => Some(bytes.len() as i32),
            Datum::Null => Some(0),
            _ => None,
        },
        _ => None,
    }
}

/// Fractional-second precision of the temporal argument (`fsp`); 0 when the
/// argument is missing or not temporal.
fn arg_fsp(args: &[ExprNode], info: &TableInfo, index_name: &str, text: &str) -> i32 {
    args.last()
        .and_then(|arg| infer(arg, info, index_name, text).ok())
        .filter(|ft| {
            matches!(
                ft.tp,
                sealdb_types::field_type::TYPE_DATETIME
                    | sealdb_types::field_type::TYPE_TIMESTAMP
                    | sealdb_types::field_type::TYPE_DURATION
            )
        })
        .map(|ft| ft.decimal.max(0))
        .unwrap_or(0)
}

/// Is this index part a multi-valued one (`CAST(json_col AS … ARRAY)`)?
/// Matching on the function name is not enough: builtins such as `json_array`
/// contain `ARRAY` but produce a JSON value, not an array index part.
pub(crate) fn is_multi_valued_part(text: &str) -> bool {
    parse_expr(text).is_ok_and(|expr| match expr {
        ExprNode::Cast(c) => c.target.trim().to_ascii_uppercase().ends_with("ARRAY"),
        _ => false,
    })
}

fn infer(expr: &ExprNode, info: &TableInfo, index_name: &str, text: &str) -> Result<FieldType> {
    match expr {
        ExprNode::Column(c) => column_field_type(info, &c.name).cloned().ok_or_else(|| {
            Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{}' in index '{}'", c.name, index_name),
            )
        }),
        ExprNode::Value(v) => literal_field_type(&v.datum),
        ExprNode::Unary(u) => infer(&u.expr, info, index_name, text),
        ExprNode::Binary(b) => {
            let left = infer(&b.left, info, index_name, text)?;
            let right = infer(&b.right, info, index_name, text)?;
            Ok(match b.op {
                // Comparisons yield 0/1.
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::NullSafeEq => {
                    FieldType::new_longlong()
                }
                Op::Plus | Op::Minus | Op::Mul | Op::IntDiv | Op::Mod | Op::Div => {
                    numeric_result(&left, &right)
                }
                // MySQL shifts cast both operands to unsigned 64-bit.
                Op::LeftShift | Op::RightShift => unsigned_longlong(),
                _ => FieldType::new_longlong(),
            })
        }
        ExprNode::Func(f) => function_field_type(&f.name, f, info, index_name, text),
        ExprNode::Cast(c) => {
            let target = c.target.trim();
            // Multi-valued index part `CAST(json_col AS SIGNED ARRAY)`: the
            // hidden column stores the array elements, compared element-wise
            // by the uniqueness check rather than by whole-column equality.
            if target.to_ascii_uppercase().ends_with("ARRAY") {
                return Ok(FieldType::new_string());
            }
            if target.eq_ignore_ascii_case("json") || target.eq_ignore_ascii_case("geometry") {
                return Err(Error::new(
                    ERR_FUNCTIONAL_INDEX_ON_JSON_OR_GEOMETRY_FUNCTION,
                    "Cannot create an expression index on a function that returns a JSON or GEOMETRY value",
                ));
            }
            field_type_from_name(target)
        }
        ExprNode::Row(_) => Err(Error::new(
            ERR_FUNCTIONAL_INDEX_ROW_VALUE_IS_NOT_ALLOWED,
            format!("Expression of expression index '{index_name}' cannot refer to a row value"),
        )),
        _ => Err(Error::new(
            ERR_FUNCTIONAL_INDEX_ON_BLOB,
            format!("The used storage engine cannot index the expression '{text}'"),
        )),
    }
}

/// `bigint unsigned`, the result of `vitess_hash` and of the bit operators.
fn unsigned_longlong() -> FieldType {
    let mut ty = FieldType::new_longlong();
    ty.unsigned = true;
    ty
}

/// Integer arithmetic widens to BIGINT; anything floating/decimal keeps the
/// widest operand type, matching TiDB's `inferType4Arithmetic`.
fn numeric_result(left: &FieldType, right: &FieldType) -> FieldType {
    use sealdb_types::field_type::EvalType;
    match (left.eval_type(), right.eval_type()) {
        (EvalType::Int, EvalType::Int) => FieldType::new_longlong(),
        (EvalType::Decimal, _) | (_, EvalType::Decimal) => FieldType::new_newdecimal(),
        (EvalType::Real, _) | (_, EvalType::Real) => FieldType::new_double(),
        _ => FieldType::new_double(),
    }
}

fn literal_field_type(datum: &Datum) -> Result<FieldType> {
    let tp = match datum {
        Datum::I64(_) | Datum::U64(_) => FieldType::new_longlong(),
        Datum::F64(_) => FieldType::new_double(),
        Datum::Decimal(_) => FieldType::new_newdecimal(),
        Datum::Bytes(_) => FieldType::new_string(),
        _ => FieldType::new_string(),
    };
    Ok(tp)
}

/// Functions TiDB refuses in an expression index because their value is not a
/// pure function of the row (`rand()`, `now()`).
fn is_disallowed_function(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "rand"
            | "random_bytes"
            | "now"
            | "current_timestamp"
            | "current_date"
            | "curdate"
            | "current_time"
            | "curtime"
            | "utc_timestamp"
            | "utc_date"
            | "utc_time"
            | "unix_timestamp"
            | "sysdate"
            | "uuid"
            | "uuid_short"
            | "database"
            | "schema"
            | "user"
            | "current_user"
            | "connection_id"
            | "last_insert_id"
            | "version"
            | "found_rows"
            | "row_count"
            | "sleep"
    )
}

/// Builtins whose result is JSON (TiDB `FieldType.TypeJSON`). An expression
/// index over one of them is 3753.
fn returns_json(name: &str) -> bool {
    matches!(
        name,
        "json_extract"
            | "json_array"
            | "json_array_append"
            | "json_array_insert"
            | "json_object"
            | "json_merge"
            | "json_merge_preserve"
            | "json_merge_patch"
            | "json_set"
            | "json_insert"
            | "json_replace"
            | "json_remove"
            | "json_search"
            | "json_keys"
    )
}

/// Builtins whose result is a length-less TEXT/BLOB. An expression index over
/// one of them is 3757 (`…returns a BLOB or TEXT. Please consider using CAST`).
fn returns_blob(name: &str) -> bool {
    matches!(name, "json_unquote" | "json_pretty")
}

fn function_field_type(
    name: &str,
    f: &sealdb_parser::ast::FuncCallExpr,
    info: &TableInfo,
    index_name: &str,
    text: &str,
) -> Result<FieldType> {
    if is_disallowed_function(name) {
        return Err(Error::new(
            ERR_FUNCTIONAL_INDEX_ON_FIELD,
            format!("Expression of expression index '{index_name}' contains a disallowed function"),
        ));
    }
    let lower = name.to_ascii_lowercase();
    // The hidden column mirrors the function's real result type; the index
    // build then rejects JSON (3753) and TEXT/BLOB (3757) results.
    if returns_json(&lower) {
        return Ok(FieldType {
            tp: sealdb_types::field_type::TYPE_JSON,
            flen: -1,
            ..FieldType::new_string()
        });
    }
    if returns_blob(&lower) {
        return Ok(FieldType {
            tp: sealdb_types::field_type::TYPE_BLOB,
            flen: -1,
            ..FieldType::new_string()
        });
    }
    let ty = match lower.as_str() {
        // TiDB `tidb_shard` hashes into a 64-bit signed integer.
        "tidb_shard" => FieldType::new_longlong(),
        // `vitess_hash` is the null-key DES hash: `bigint unsigned`.
        "vitess_hash" => unsigned_longlong(),
        "upper" | "ucase" | "lower" | "lcase" | "concat" | "concat_ws" | "substring" | "substr"
        | "trim" | "ltrim" | "rtrim" | "replace" | "left" | "right" | "repeat" | "reverse"
        | "insert" | "format" | "hex" | "to_base64" | "from_base64" => {
            let mut ty = FieldType::new_string();
            // `concat` adds the argument widths; the other string functions
            // keep the width of their first argument. A known width is needed
            // because a zero-width CHAR/VARCHAR result cannot be indexed
            // (3761), while an unknown one is a length-less TEXT-like result.
            let known = if lower == "concat" || lower == "concat_ws" {
                let mut total = 0i32;
                let mut all_known = true;
                for arg in &f.args {
                    match known_string_flen(arg, info) {
                        Some(len) => total += len,
                        None => {
                            all_known = false;
                            break;
                        }
                    }
                }
                all_known.then_some(total)
            } else {
                f.args.first().and_then(|arg| known_string_flen(arg, info))
            };
            if let Some(len) = known {
                ty.flen = len;
            }
            ty
        }
        // TiDB `json_quote`: `flen = arg_flen * 6 + 2` (worst-case escaping),
        // which is what pushes a wide argument past the 3072-byte key limit.
        "json_quote" => {
            let mut ty = FieldType::new_string();
            if let Some(arg) = f.args.first() {
                let src = infer(arg, info, index_name, text)?;
                ty.flen = if src.flen < 0 {
                    src.flen
                } else {
                    src.flen * 6 + 2
                };
            }
            ty
        }
        // TiDB `json_type`: 51-character string (the longest type name).
        "json_type" => {
            let mut ty = FieldType::new_string();
            ty.flen = 51;
            ty
        }
        // TiDB `json_storage_size` / `json_depth` / `json_length`: integers.
        "json_storage_size" | "json_depth" | "json_length" => FieldType::new_longlong(),
        // TiDB `md5` returns a 32-byte hex string.
        "md5" => {
            let mut ty = FieldType::new_string();
            ty.flen = 32;
            ty
        }
        "abs" | "floor" | "ceil" | "ceiling" | "round" | "conv" | "crc32" | "str_to_date" => {
            FieldType::new_longlong()
        }
        // TiDB `TIMEDIFF` returns a TIME (fsp = max of the argument fsp).
        "timediff" | "addtime" | "subtime" => FieldType {
            tp: sealdb_types::TYPE_DURATION,
            flen: 17,
            decimal: 6,
            ..FieldType::default()
        },
        "length" | "char_length" | "character_length" | "octet_length" | "instr" | "locate"
        | "position" | "bit_length" | "unix_timestamp_of" => FieldType::new_longlong(),
        // TiDB `TIMESTAMPADD(unit, interval, datetime_expr)`: a DATETIME. Its
        // first argument is a unit keyword, so the fallback on the first
        // argument would mis-type it as an (unindexable) string.
        "timestampadd" => FieldType {
            tp: sealdb_types::TYPE_DATETIME,
            flen: 19,
            decimal: arg_fsp(&f.args, info, index_name, text),
            ..FieldType::default()
        },
        // TiDB `TIMESTAMPDIFF(unit, dt1, dt2)`: a signed integer.
        "timestampdiff" => FieldType::new_longlong(),
        _ => {
            // Unknown function: fall back to its first argument's type, which is
            // what TiDB's `inferType4FuncCast` does for builtins returning
            // their input.
            if let Some(arg) = f.args.first() {
                infer(arg, info, index_name, text)?
            } else {
                return Err(Error::new(
                    ERR_FUNCTIONAL_INDEX_ON_BLOB,
                    format!("The used storage engine cannot index the expression '{text}'"),
                ));
            }
        }
    };
    Ok(ty)
}

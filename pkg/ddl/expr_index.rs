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
    ERR_BAD_FIELD, ERR_DUP_FIELDNAME, ERR_FUNCTIONAL_INDEX_ON_BLOB, ERR_FUNCTIONAL_INDEX_ON_FIELD,
    ERR_FUNCTIONAL_INDEX_ON_JSON_OR_GEOMETRY_FUNCTION,
    ERR_FUNCTIONAL_INDEX_ROW_VALUE_IS_NOT_ALLOWED, Error, Result,
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
) -> Result<(Vec<IndexColumn>, Vec<ColumnInfo>)> {
    let mut hidden: Vec<ColumnInfo> = Vec::new();
    let mut columns = Vec::with_capacity(parts.len());
    for (pos, part) in parts.iter().enumerate() {
        match part {
            IndexPart::Column(name) | IndexPart::Prefix { name, .. } => {
                let col = info
                    .columns
                    .iter()
                    .find(|c| !c.hidden && c.name.o().eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        Error::new(
                            ERR_BAD_FIELD,
                            format!("Unknown column '{name}' in index '{index_name}'"),
                        )
                    })?;
                let mut ic = IndexColumn::new(col.name.o(), col.offset);
                ic.length = part.prefix_length();
                columns.push(ic);
            }
            IndexPart::Expression(text) => {
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
                let mut col = ColumnInfo::new(
                    id,
                    offset,
                    name.clone(),
                    expression_field_type(text, info, index_name)?,
                );
                col.generated_expr = Some(text.clone());
                col.generated_stored = false;
                col.hidden = true;
                hidden.push(col);
                columns.push(IndexColumn::new(name, offset));
            }
        }
    }
    Ok((columns, hidden))
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
                _ => FieldType::new_longlong(),
            })
        }
        ExprNode::Func(f) => function_field_type(&f.name, f, info, index_name, text),
        ExprNode::Cast(c) => {
            let target = c.target.trim();
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
    let ty = match lower.as_str() {
        // TiDB `tidb_shard` hashes into a 64-bit signed integer.
        "tidb_shard" => FieldType::new_longlong(),
        "upper" | "ucase" | "lower" | "lcase" | "concat" | "concat_ws" | "substring" | "substr"
        | "trim" | "ltrim" | "rtrim" | "replace" | "left" | "right" | "repeat" | "reverse"
        | "insert" | "format" | "hex" | "to_base64" | "from_base64" => {
            let mut ty = FieldType::new_string();
            // `upper(name)` keeps the operand's declared length when known.
            if let Some(arg) = f.args.first()
                && let Some(src) = column_field_type(
                    info,
                    match arg {
                        ExprNode::Column(c) => &c.name,
                        _ => "",
                    },
                )
                && src.flen > 0
            {
                ty.flen = src.flen;
            }
            ty
        }
        "abs" | "floor" | "ceil" | "ceiling" | "round" | "conv" | "crc32" | "str_to_date" => {
            FieldType::new_longlong()
        }
        "length" | "char_length" | "character_length" | "octet_length" | "instr" | "locate"
        | "position" | "bit_length" | "unix_timestamp_of" => FieldType::new_longlong(),
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

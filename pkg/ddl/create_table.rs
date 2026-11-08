use sealdb_errno::{
    ERR_BAD_FIELD, ERR_DUP_FIELDNAME, ERR_NO_SUCH_TABLE, ERR_VIEW_WRONG_LIST, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, Mutator, TableInfo};
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, ValueExpr};
use sealdb_parser::ast::{ColumnDef, CreateTableStmt, SelectStmt};
use sealdb_parser::model::CIStr;
use sealdb_types::Datum;
use std::collections::HashMap;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_DURATION,
    TYPE_ENUM, TYPE_FLOAT, TYPE_JSON, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG, TYPE_MEDIUM_BLOB,
    TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB,
    TYPE_VAR_STRING, TYPE_YEAR,
};

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
        if let Some(generated) = &col.generated {
            info.generated_expr = Some(generated.expr.clone());
            info.generated_stored = generated.stored;
        }
        info.default_expr = col.default.clone();
        info.auto_increment = col.auto_increment;
        if col.not_null {
            info.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
        }
        columns.push(info);
    }
    let mut info = TableInfo::new(table_id, stmt.table.name.clone(), columns);
    for (offset, index) in stmt.indices.iter().enumerate() {
        let mut idx_cols = Vec::with_capacity(index.columns.len());
        for col_name in &index.columns {
            let col = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(col_name))
                .ok_or_else(|| {
                    Error::new(
                        sealdb_errno::ERR_BAD_FIELD,
                        format!("Unknown column '{col_name}' in index '{}'", index.name),
                    )
                })?;
            idx_cols.push(IndexColumn::new(col.name.o(), col.offset));
        }
        let idx_id = (offset as i64) + 1;
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
        info.indices.push(
            IndexInfo::new(idx_id, index.name.clone(), info.name.o(), idx_cols)
                .with_flags(index.unique, index.primary)
                .with_global(index.global),
        );
        info.max_idx_id = idx_id;
        for c in &mut info.columns {
            if pk_col_offsets.contains(&c.offset) {
                c.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
            }
        }
    }
    Ok(info)
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
                    push_select_column(&mut cols, name, type_name.clone())?;
                }
            }
            continue;
        }
        let name = field_column_name(field, i);
        let type_name = type_name_from_expr(&field.expr, &sources)?;
        push_select_column(&mut cols, &name, type_name)?;
    }
    if cols.is_empty() {
        return Err(Error::unsupported("CREATE TABLE AS SELECT without columns"));
    }
    Ok(cols)
}

fn push_select_column(cols: &mut Vec<ColumnDef>, name: &str, type_name: String) -> Result<()> {
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
        not_null: false,
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

fn field_column_name(field: &sealdb_parser::ast::SelectField, offset: usize) -> String {
    if let Some(name) = &field.as_name
        && !name.is_empty()
    {
        return name.clone();
    }
    match &field.expr {
        ExprNode::Column(col) => col.name.clone(),
        other => {
            let restored = other.restore_string();
            if restored.is_empty() {
                format!("_{offset}")
            } else {
                restored
            }
        }
    }
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
        "TINYINT" | "BOOL" | "BOOLEAN" => FieldType {
            tp: TYPE_TINY,
            flen: 4,
            ..numeric_ft()
        },
        "SMALLINT" => FieldType {
            tp: TYPE_SHORT,
            flen: 6,
            ..numeric_ft()
        },
        "INT" | "INTEGER" | "MEDIUMINT" => FieldType {
            tp: TYPE_LONG,
            flen: 11,
            ..numeric_ft()
        },
        "BIGINT" => FieldType {
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
            let (flen, decimal) = parse_flen_decimal(type_name).unwrap_or((10, 0));
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
        "DATETIME" => FieldType {
            tp: TYPE_DATETIME,
            flen: 19,
            ..string_ft()
        },
        "TIMESTAMP" => FieldType {
            tp: TYPE_TIMESTAMP,
            flen: 19,
            ..string_ft()
        },
        "TIME" => FieldType {
            tp: TYPE_DURATION,
            flen: parse_len(type_name).unwrap_or(10),
            ..string_ft()
        },
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

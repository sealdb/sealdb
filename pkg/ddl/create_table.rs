use sealdb_errno::{ERR_BAD_FIELD, ERR_DUP_FIELDNAME, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, Mutator, TableInfo};
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, ValueExpr};
use sealdb_parser::ast::{ColumnDef, CreateTableStmt, SelectStmt, TableName};
use sealdb_parser::model::CIStr;
use sealdb_types::Datum;
use sealdb_types::field_type::{
    FieldType, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_FLOAT, TYPE_JSON, TYPE_LONG,
    TYPE_LONG_LONG, TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_TIMESTAMP, TYPE_TINY, TYPE_VAR_STRING,
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
        // Single integer PRIMARY KEY → clustered int handle (TiDB pk_is_handle).
        if index.primary
            && idx_cols.len() == 1
            && info.columns.iter().any(|c| {
                c.offset == idx_cols[0].offset
                    && c.field_type.eval_type() == sealdb_types::field_type::EvalType::Int
            })
        {
            info.pk_is_handle = true;
        }
        info.indices.push(
            IndexInfo::new(idx_id, index.name.clone(), info.name.o(), idx_cols)
                .with_flags(index.unique, index.primary),
        );
        info.max_idx_id = idx_id;
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
    let sources = select_source_tables(is, select, current_db)?;
    let mut cols = Vec::new();
    for (i, field) in select.fields.iter().enumerate() {
        if is_star_expr(&field.expr) {
            if sources.is_empty() {
                return Err(Error::unsupported("CREATE TABLE AS SELECT * without FROM"));
            }
            for table in &sources {
                for col in &table.columns {
                    push_select_column(&mut cols, col.name.o(), col.field_type.type_name())?;
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
        generated: None,
    });
    Ok(())
}

fn select_source_tables<'a>(
    is: &'a InfoSchema,
    select: &SelectStmt,
    current_db: &str,
) -> Result<Vec<&'a TableInfo>> {
    let mut tables = Vec::new();
    if !select.joins.is_empty() {
        for table in select.join_tables() {
            tables.push(lookup_table(is, table, current_db)?);
        }
    } else if let Some(from) = &select.from {
        tables.push(lookup_table(is, from, current_db)?);
    }
    Ok(tables)
}

fn lookup_table<'a>(
    is: &'a InfoSchema,
    name: &TableName,
    current_db: &str,
) -> Result<&'a TableInfo> {
    let (schema, table) = name.qualified(current_db);
    is.table_by_name(&schema, &table).ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })
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

fn type_name_from_expr(expr: &ExprNode, sources: &[&TableInfo]) -> Result<String> {
    match expr {
        ExprNode::Column(col) => Ok(lookup_select_column(col, sources)?.field_type.type_name()),
        ExprNode::Cast(cast) => Ok(cast.target.clone()),
        ExprNode::Value(ValueExpr { datum }) => Ok(type_name_from_datum(datum).to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("COUNT") => Ok("bigint".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("SUM") => Ok("decimal".to_string()),
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("AVG") => Ok("double".to_string()),
        ExprNode::Window(_) => Ok("bigint".to_string()),
        _ => Ok("varchar".to_string()),
    }
}

fn lookup_select_column<'a>(
    col: &ColumnNameExpr,
    sources: &[&'a TableInfo],
) -> Result<&'a ColumnInfo> {
    let hinted = col.table.as_deref();
    let mut found = None;
    for table in sources {
        if let Some(hint) = hinted
            && !table.name.o().eq_ignore_ascii_case(hint)
        {
            continue;
        }
        if let Some(column) = table
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
        {
            if found.is_some() && hinted.is_none() {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Column '{0}' in field list is ambiguous", col.name),
                ));
            }
            found = Some(column);
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
        Datum::I64(_) | Datum::U64(_) => "int",
        Datum::F64(_) => "double",
        Datum::Decimal(_) => "decimal",
        Datum::Time(_) => "datetime",
        Datum::Bytes(_) | Datum::Null => "varchar",
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
        // ENUM/SET stored as string for now (TiDB TYPE_ENUM value path later).
        "ENUM" | "SET" => FieldType {
            tp: TYPE_VAR_STRING,
            flen: -1,
            ..string_ft()
        },
        "BLOB" => FieldType {
            tp: TYPE_BLOB,
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

use sealdb_errno::{Error, Result};
use sealdb_meta::{ColumnInfo, TableInfo};
use sealdb_parser::ast::CreateTableStmt;
use sealdb_types::field_type::{
    FieldType, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_FLOAT, TYPE_LONG,
    TYPE_LONG_LONG, TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_TIMESTAMP, TYPE_TINY, TYPE_VAR_STRING,
};

pub fn build_table_info(stmt: &CreateTableStmt, table_id: i64) -> Result<TableInfo> {
    if stmt.cols.is_empty() {
        return Err(Error::unsupported("CREATE TABLE without columns"));
    }
    let mut columns = Vec::with_capacity(stmt.cols.len());
    for (offset, col) in stmt.cols.iter().enumerate() {
        columns.push(ColumnInfo::new(
            (offset as i64) + 1,
            offset as i32,
            col.name.clone(),
            field_type_from_name(&col.type_name)?,
        ));
    }
    Ok(TableInfo::new(table_id, stmt.table.name.clone(), columns))
}

fn field_type_from_name(type_name: &str) -> Result<FieldType> {
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
        "FLOAT" => FieldType {
            tp: TYPE_FLOAT,
            flen: 12,
            decimal: -1,
            ..numeric_ft()
        },
        "DOUBLE" | "REAL" => FieldType {
            tp: TYPE_DOUBLE,
            flen: 22,
            decimal: -1,
            ..numeric_ft()
        },
        "DECIMAL" | "NUMERIC" => FieldType {
            tp: TYPE_NEW_DECIMAL,
            flen: 10,
            decimal: 0,
            ..numeric_ft()
        },
        "CHAR" | "VARCHAR" | "TEXT" | "STRING" => FieldType {
            tp: TYPE_VAR_STRING,
            flen: parse_len(type_name).unwrap_or(-1),
            ..string_ft()
        },
        "BLOB" => FieldType {
            tp: TYPE_BLOB,
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
    let start = type_name.find('(')?;
    let end = type_name.find(')')?;
    type_name[start + 1..end]
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

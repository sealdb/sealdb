/// Virtual INFORMATION_SCHEMA tables (TiDB `pkg/infoschema/tables.go`).
/// Completeness wave: remaining I_S tables, statistics, privileges, views.
use crate::InfoSchema;
use sealdb_errno::{ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_types::Datum;

pub fn virtual_rows(table: &str, is: &InfoSchema) -> Result<(Vec<String>, Vec<Vec<Datum>>)> {
    match table.to_ascii_lowercase().as_str() {
        "schemata" => Ok(schemata(is)),
        "tables" => Ok(tables(is)),
        "columns" => Ok(columns(is)),
        other => Err(Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table 'information_schema.{other}' doesn't exist"),
        )),
    }
}

fn schemata(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "CATALOG_NAME".to_string(),
        "SCHEMA_NAME".to_string(),
        "DEFAULT_CHARACTER_SET_NAME".to_string(),
        "DEFAULT_COLLATION_NAME".to_string(),
    ];
    let mut rows = vec![schema_row("information_schema")];
    for db in is.all_schemas() {
        rows.push(schema_row(db.name.o()));
    }
    (schema, rows)
}

fn schema_row(name: &str) -> Vec<Datum> {
    vec![
        bytes("def"),
        bytes(name),
        bytes("utf8mb4"),
        bytes("utf8mb4_bin"),
    ]
}

fn tables(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "TABLE_TYPE".to_string(),
        "ENGINE".to_string(),
    ];
    let mut rows = Vec::new();
    for db in is.all_schemas() {
        for table in is.schema_tables(db.name.o()) {
            rows.push(vec![
                bytes("def"),
                bytes(db.name.o()),
                bytes(table.name.o()),
                bytes("BASE TABLE"),
                bytes("InnoDB"),
            ]);
        }
    }
    (schema, rows)
}

fn columns(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "COLUMN_NAME".to_string(),
        "ORDINAL_POSITION".to_string(),
        "DATA_TYPE".to_string(),
        "COLUMN_TYPE".to_string(),
        "IS_NULLABLE".to_string(),
        "COLUMN_KEY".to_string(),
        "COLUMN_COMMENT".to_string(),
    ];
    let mut rows = Vec::new();
    for db in is.all_schemas() {
        for table in is.schema_tables(db.name.o()) {
            for (i, col) in table.columns.iter().enumerate() {
                let type_name = col.field_type.type_name();
                rows.push(vec![
                    bytes("def"),
                    bytes(db.name.o()),
                    bytes(table.name.o()),
                    bytes(col.name.o()),
                    Datum::I64((i + 1) as i64),
                    bytes(&type_name),
                    bytes(&type_name),
                    bytes("YES"),
                    bytes(""),
                    bytes(""),
                ]);
            }
        }
    }
    (schema, rows)
}

fn bytes(value: &str) -> Datum {
    Datum::Bytes(value.as_bytes().to_vec())
}

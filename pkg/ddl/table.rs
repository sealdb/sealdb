use crate::create_table::build_table_info;
use sealdb_errno::{ERR_BAD_DB, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{Mutator, TableInfo};
use sealdb_parser::ast::{CreateTableStmt, DropTableStmt, TableName};

pub fn create_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateTableStmt,
    current_db: &str,
) -> Result<TableInfo> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    if let Some(existing) = is.table_by_name(&schema, &table) {
        if stmt.if_not_exists {
            return Ok(existing.clone());
        }
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{table}' already exists"),
        ));
    }
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let table_id = meta.gen_global_id()?;
    let info = build_table_info(stmt, table_id)?;
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}

pub fn drop_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropTableStmt,
    current_db: &str,
) -> Result<()> {
    for name in &stmt.tables {
        let (schema, table) = match resolve_name(name, current_db) {
            Ok(pair) => pair,
            Err(err) if stmt.if_exists && err.code == ERR_NO_DB => continue,
            Err(err) => return Err(err),
        };
        match (
            is.schema_by_name(&schema),
            is.table_by_name(&schema, &table),
        ) {
            (Some(db), Some(tbl)) => {
                meta.drop_table_or_view(db.id, tbl.id)?;
                meta.gen_schema_version()?;
            }
            _ if stmt.if_exists => {}
            (None, _) => {
                return Err(Error::new(
                    ERR_BAD_DB,
                    format!("Unknown database '{schema}'"),
                ));
            }
            (_, None) => {
                return Err(Error::new(
                    ERR_NO_SUCH_TABLE,
                    format!("Table '{schema}.{table}' doesn't exist"),
                ));
            }
        }
    }
    Ok(())
}

fn resolve_name(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}

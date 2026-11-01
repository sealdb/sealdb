use sealdb_errno::{ERR_DB_CREATE_EXISTS, ERR_DB_DROP_EXISTS, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{DBInfo, Mutator};
use sealdb_parser::ast::{CreateDatabaseStmt, DropDatabaseStmt};

pub fn create_schema(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateDatabaseStmt,
) -> Result<DBInfo> {
    if let Some(existing) = is.schema_by_name(&stmt.name) {
        if stmt.if_not_exists {
            return Ok(existing.clone());
        }
        return Err(Error::new(
            ERR_DB_CREATE_EXISTS,
            format!("Can't create database '{}'; database exists", stmt.name),
        ));
    }
    let id = meta.gen_global_id()?;
    let db = DBInfo::new(id, stmt.name.clone());
    meta.create_database(&db)?;
    meta.gen_schema_version()?;
    Ok(db)
}

pub fn drop_schema(meta: &mut Mutator, is: &InfoSchema, stmt: &DropDatabaseStmt) -> Result<()> {
    match is.schema_by_name(&stmt.name) {
        Some(db) => {
            meta.drop_database(db.id)?;
            meta.gen_schema_version()?;
            Ok(())
        }
        None if stmt.if_exists => Ok(()),
        None => Err(Error::new(
            ERR_DB_DROP_EXISTS,
            format!(
                "Can't drop database '{}'; database doesn't exist",
                stmt.name
            ),
        )),
    }
}

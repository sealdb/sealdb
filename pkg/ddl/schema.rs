use sealdb_errno::{ERR_BAD_DB, ERR_DB_CREATE_EXISTS, ERR_DB_DROP_EXISTS, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{DBInfo, Mutator};
use sealdb_parser::ast::{AlterDatabaseStmt, CreateDatabaseStmt, DropDatabaseStmt};
use sealdb_parser::charset::Collation;

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

pub fn alter_schema(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterDatabaseStmt,
    current_db: &str,
) -> Result<DBInfo> {
    let name = if stmt.name.is_empty() {
        if current_db.is_empty() {
            return Err(Error::new(ERR_BAD_DB, "No database selected"));
        }
        current_db.to_string()
    } else {
        stmt.name.clone()
    };
    let Some(existing) = is.schema_by_name(&name) else {
        return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{name}'")));
    };
    let mut db = existing.clone();
    if let Some(cs) = &stmt.charset {
        let coll = Collation::default_for_charset(cs);
        db.charset = coll.charset.clone();
        if stmt.collate.is_none() {
            db.collate = coll.name;
        }
    }
    if let Some(coll) = &stmt.collate {
        let parsed = Collation::parse(coll);
        db.collate = parsed.name;
        if stmt.charset.is_none() {
            db.charset = parsed.charset;
        }
    }
    meta.update_database(&db)?;
    meta.gen_schema_version()?;
    Ok(db)
}

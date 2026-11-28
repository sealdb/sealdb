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
    let mut db = DBInfo::new(id, stmt.name.clone());
    apply_schema_charset_collation(&mut db, stmt.charset.as_deref(), stmt.collate.as_deref())?;
    meta.create_database(&db)?;
    meta.gen_schema_version()?;
    Ok(db)
}

/// `DEFAULT CHARACTER SET` / `COLLATE` of `CREATE`/`ALTER DATABASE`: an
/// unknown charset is 1115, an unsupported collation is 1273.
fn apply_schema_charset_collation(
    db: &mut DBInfo,
    charset: Option<&str>,
    collate: Option<&str>,
) -> Result<()> {
    if let Some(cs) = charset {
        if !sealdb_parser::charset::is_supported_charset(cs) {
            return Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_CHARACTER_SET,
                format!("Unknown character set: '{cs}'"),
            ));
        }
        let coll = Collation::default_for_charset(cs);
        db.charset = coll.charset.clone();
        if collate.is_none() {
            db.collate = coll.name;
        }
    }
    if let Some(coll) = collate {
        if let Some(err) = sealdb_parser::charset::collation_validation_error(coll) {
            return Err(err);
        }
        let parsed = Collation::parse(coll);
        db.collate = parsed.name;
        if charset.is_none() {
            db.charset = parsed.charset;
        }
    }
    Ok(())
}

pub fn drop_schema(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropDatabaseStmt,
    fk_checks: bool,
) -> Result<()> {
    match is.schema_by_name(&stmt.name) {
        Some(db) => {
            // TiDB also runs the 3730 check for `DROP DATABASE`: a table in
            // another schema whose foreign key points into this database
            // blocks the drop (unless `foreign_key_checks=0`).
            if fk_checks {
                let dropped: Vec<(String, String)> = is
                    .schema_tables(&db.name.o().to_string())
                    .iter()
                    .map(|tbl| (db.name.o().to_string(), tbl.name.o().to_string()))
                    .collect();
                crate::table::check_drop_tables_referenced_by_fk(is, &dropped)?;
            }
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
    apply_schema_charset_collation(&mut db, stmt.charset.as_deref(), stmt.collate.as_deref())?;
    meta.update_database(&db)?;
    meta.gen_schema_version()?;
    Ok(db)
}

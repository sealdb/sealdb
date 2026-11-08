use crate::create_table::{build_table_info, columns_from_select};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_TABLE_EXISTS, ERR_VIEW_INVALID, ERR_WRONG_OBJECT, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{Mutator, TableInfo, ViewInfo};
use sealdb_parser::ast::{CreateTableStmt, CreateViewStmt};
use sealdb_parser::format::RestoreCtx;
use sealdb_parser::model::CIStr;

pub fn create_view(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateViewStmt,
    current_db: &str,
) -> Result<TableInfo> {
    let (schema, name) = resolve_name(&stmt.view_name, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    if let Some(existing) = is.table_by_name(&schema, &name) {
        if stmt.or_replace {
            if !existing.is_view() {
                return Err(Error::new(
                    ERR_WRONG_OBJECT,
                    format!("'{schema}.{name}' is not VIEW"),
                ));
            }
            meta.drop_table_or_view(db.id, existing.id)?;
        } else {
            return Err(Error::new(
                ERR_TABLE_EXISTS,
                format!("Table '{name}' already exists"),
            ));
        }
    }
    let check = stmt.check_option.trim();
    if stmt.algorithm.eq_ignore_ascii_case("TEMPTABLE")
        && !check.is_empty()
        && !check.eq_ignore_ascii_case("NONE")
    {
        return Err(Error::new(
            ERR_VIEW_INVALID,
            format!(
                "VIEW '{}' WITH CHECK OPTION is invalid for ALGORITHM=TEMPTABLE",
                name
            ),
        ));
    }
    let table_id = meta.gen_global_id()?;
    let mut cols = columns_from_select(is, &stmt.select, current_db)?;
    if !stmt.cols.is_empty() {
        if stmt.cols.len() != cols.len() {
            return Err(Error::unsupported(
                "CREATE VIEW column list length must match SELECT",
            ));
        }
        for (col, rename) in cols.iter_mut().zip(stmt.cols.iter()) {
            col.name = rename.clone();
        }
    }
    let create = CreateTableStmt {
        if_not_exists: false,
        table: stmt.view_name.clone(),
        cols,
        indices: Vec::new(),
        foreign_keys: Vec::new(),
        partition: None,
        ttl: None,
        refer_table: None,
        select: None,
    };
    let mut info = build_table_info(&create, table_id)?;
    let select_text = if stmt.select.text.is_empty() {
        let mut restore = RestoreCtx::new();
        stmt.select.restore(&mut restore)?;
        restore.into_string()
    } else {
        stmt.select.text.clone()
    };
    info.view = Some(ViewInfo {
        algorithm: stmt.algorithm.clone(),
        definer: stmt.definer.clone(),
        security: stmt.security.clone(),
        select_stmt: select_text,
        check_option: stmt.check_option.clone(),
        cols: stmt.cols.iter().map(CIStr::new).collect(),
    });
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}

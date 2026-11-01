// F18 core: ALTER TABLE ADD/DROP COLUMN and ADD/DROP INDEX.
// Completeness wave / not this feature: CHANGE/MODIFY/RENAME, AFTER/FIRST,
// generated columns, partitions, multi-schema-state jobs.
use crate::create_table::field_type_from_name;
use crate::index::{add_index, remove_index};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_CANT_DROP_FIELD_OR_KEY, ERR_DUP_FIELDNAME, ERR_NO_SUCH_TABLE, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{ColumnInfo, Mutator};
use sealdb_parser::ast::{AlterTableSpec, AlterTableStmt};

pub fn alter_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let mut snapshot = is.clone();
    for spec in &stmt.specs {
        match spec {
            AlterTableSpec::AddColumn {
                if_not_exists,
                column,
            } => add_column(meta, &snapshot, stmt, column, *if_not_exists, current_db)?,
            AlterTableSpec::DropColumn { if_exists, name } => {
                drop_column(meta, &snapshot, stmt, name, *if_exists, current_db)?;
            }
            AlterTableSpec::AddIndex(index) => {
                add_index(
                    meta,
                    &snapshot,
                    &stmt.table,
                    index,
                    false,
                    current_db,
                    storage,
                )?;
            }
            AlterTableSpec::DropIndex { if_exists, name } => {
                remove_index(
                    meta,
                    &snapshot,
                    &stmt.table,
                    name,
                    *if_exists,
                    current_db,
                    storage,
                )?;
            }
        }
        snapshot = InfoSchema::from_meta(meta)?;
    }
    Ok(())
}

fn add_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    column: &sealdb_parser::ast::ColumnDef,
    if_not_exists: bool,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    if info
        .columns
        .iter()
        .any(|c| c.name.o().eq_ignore_ascii_case(&column.name))
    {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_DUP_FIELDNAME,
            format!("Duplicate column name '{}'", column.name),
        ));
    }
    info.max_col_id += 1;
    let offset = info.columns.len() as i32;
    info.columns.push(ColumnInfo::new(
        info.max_col_id,
        offset,
        column.name.clone(),
        field_type_from_name(&column.type_name)?,
    ));
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn drop_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    name: &str,
    if_exists: bool,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(pos) = info
        .columns
        .iter()
        .position(|c| c.name.o().eq_ignore_ascii_case(name))
    else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{name}'; check that column/key exists"),
        ));
    };
    if pos == 0 {
        return Err(Error::unsupported("DROP handle column"));
    }
    if info.indices.iter().any(|idx| {
        idx.columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
    }) {
        return Err(Error::unsupported("DROP COLUMN used by INDEX"));
    }
    if info.columns.len() == 1 {
        return Err(Error::unsupported("DROP last column"));
    }
    info.columns.remove(pos);
    for (offset, col) in info.columns.iter_mut().enumerate() {
        col.offset = offset as i32;
    }
    for idx in &mut info.indices {
        for ic in &mut idx.columns {
            if let Some(col) = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(ic.name.o()))
            {
                ic.offset = col.offset;
            }
        }
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

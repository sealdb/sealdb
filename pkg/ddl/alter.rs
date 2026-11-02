// F18 core: ALTER TABLE ADD/DROP COLUMN|INDEX, CHANGE/MODIFY/RENAME.
// Completeness wave / not this feature: AFTER/FIRST, ALGORITHM/LOCK,
// generated columns, partitions, multi-schema-state jobs.
use crate::create_table::field_type_from_name;
use crate::index::{add_index, backfill_index, drop_index_keys, remove_index};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_DUP_FIELDNAME, ERR_DUP_KEYNAME,
    ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{ColumnInfo, Mutator, TableInfo};
use sealdb_parser::ast::{AlterTableSpec, AlterTableStmt, TableName};
use sealdb_parser::model::CIStr;
use sealdb_tablecodec::{decode_row, encode_row, gen_table_record_prefix, next_prefix};
use sealdb_types::{EvalType, FieldType};

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
            AlterTableSpec::ChangeColumn {
                old_name,
                new_name,
                type_name,
            } => change_column(
                meta,
                &snapshot,
                stmt,
                old_name,
                new_name,
                Some(type_name),
                current_db,
                storage,
            )?,
            AlterTableSpec::ModifyColumn { name, type_name } => change_column(
                meta,
                &snapshot,
                stmt,
                name,
                name,
                Some(type_name),
                current_db,
                storage,
            )?,
            AlterTableSpec::RenameColumn { old_name, new_name } => change_column(
                meta, &snapshot, stmt, old_name, new_name, None, current_db, storage,
            )?,
            AlterTableSpec::RenameTable { new_name } => {
                rename_table(meta, &snapshot, stmt, new_name, current_db)?;
            }
            AlterTableSpec::RenameIndex { old_name, new_name } => {
                rename_index(meta, &snapshot, stmt, old_name, new_name, current_db)?
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

/// CHANGE / MODIFY / RENAME COLUMN: update the name and rewrite stored
/// values when the FieldType actually changes.
#[allow(clippy::too_many_arguments)]
fn change_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
    type_name: Option<&str>,
    current_db: &str,
    storage: &dyn Storage,
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
        .position(|c| c.name.o().eq_ignore_ascii_case(old_name))
    else {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{old_name}' in '{table}'"),
        ));
    };
    if !old_name.eq_ignore_ascii_case(new_name)
        && info
            .columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(new_name))
    {
        return Err(Error::new(
            ERR_DUP_FIELDNAME,
            format!("Duplicate column name '{new_name}'"),
        ));
    }
    let new_ft = match type_name {
        Some(name) => field_type_from_name(name)?,
        None => info.columns[pos].field_type.clone(),
    };
    if pos == 0 && new_ft.eval_type() != EvalType::Int {
        return Err(Error::unsupported("MODIFY handle column type"));
    }
    let changed = field_type_changed(&info.columns[pos].field_type, &new_ft);
    if changed {
        rewrite_column_rows(storage, &info, info.columns[pos].id, &new_ft)?;
    }
    let previous = info.columns[pos].name.o().to_string();
    info.columns[pos].name = CIStr::new(new_name);
    info.columns[pos].field_type = new_ft;
    for idx in &mut info.indices {
        for ic in &mut idx.columns {
            if ic.name.o().eq_ignore_ascii_case(&previous) {
                ic.name = CIStr::new(new_name);
            }
        }
    }
    if changed {
        let affected: Vec<i64> = info
            .indices
            .iter()
            .filter(|idx| {
                idx.columns
                    .iter()
                    .any(|c| c.name.o().eq_ignore_ascii_case(new_name))
            })
            .map(|idx| idx.id)
            .collect();
        for idx_id in affected {
            drop_index_keys(storage, &info, idx_id)?;
            if let Some(idx) = info.indices.iter().find(|idx| idx.id == idx_id).cloned() {
                backfill_index(storage, &info, &idx)?;
            }
        }
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn field_type_changed(old: &FieldType, new: &FieldType) -> bool {
    old.tp != new.tp
        || old.unsigned != new.unsigned
        || old.decimal != new.decimal
        || (new.flen >= 0 && old.flen >= 0 && new.flen < old.flen)
}

fn rewrite_column_rows(
    storage: &dyn Storage,
    info: &TableInfo,
    col_id: i64,
    new_ft: &FieldType,
) -> Result<()> {
    let old_types = info.col_field_types();
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let decoded = decode_row(&raw, &old_types);
            let mut col_ids = Vec::with_capacity(info.columns.len());
            let mut values = Vec::with_capacity(info.columns.len());
            for col in &info.columns {
                let mut value = decoded
                    .iter()
                    .find(|(id, _)| *id == col.id)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(sealdb_types::Datum::Null);
                if col.id == col_id {
                    value = convert_column_value(&value, new_ft)?;
                }
                col_ids.push(col.id);
                values.push(value);
            }
            txn.set(&key, &encode_row(&col_ids, &values))?;
        }
    }
    txn.commit()
}

fn convert_column_value(
    value: &sealdb_types::Datum,
    ft: &FieldType,
) -> Result<sealdb_types::Datum> {
    let mut out = value.cast(ft)?;
    if ft.eval_type() == EvalType::String
        && ft.flen > 0
        && let sealdb_types::Datum::Bytes(bytes) = &mut out
    {
        let max = ft.flen as usize;
        if bytes.len() > max {
            bytes.truncate(max);
        }
    }
    Ok(out)
}

fn rename_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    new_name: &TableName,
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
    let (new_schema, new_table) = resolve_name(new_name, &schema)?;
    if schema.eq_ignore_ascii_case(&new_schema) && table.eq_ignore_ascii_case(&new_table) {
        return Ok(());
    }
    if is.table_by_name(&new_schema, &new_table).is_some() {
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{new_table}' already exists"),
        ));
    }
    let new_db = is
        .schema_by_name(&new_schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{new_schema}'")))?;
    info.name = CIStr::new(new_table);
    for idx in &mut info.indices {
        idx.table = info.name.clone();
    }
    if db.id == new_db.id {
        meta.update_table(db.id, &info)?;
    } else {
        meta.drop_table_or_view(db.id, info.id)?;
        meta.create_table_or_view(new_db.id, &info)?;
    }
    meta.gen_schema_version()?;
    Ok(())
}

fn rename_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
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
        .indices
        .iter()
        .position(|idx| idx.name.o().eq_ignore_ascii_case(old_name))
    else {
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{old_name}'; check that column/key exists"),
        ));
    };
    if !old_name.eq_ignore_ascii_case(new_name)
        && info
            .indices
            .iter()
            .any(|idx| idx.name.o().eq_ignore_ascii_case(new_name))
    {
        return Err(Error::new(
            ERR_DUP_KEYNAME,
            format!("Duplicate key name '{new_name}'"),
        ));
    }
    info.indices[pos].name = CIStr::new(new_name);
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

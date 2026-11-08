// CREATE/DROP INDEX schema + UNIQUE INDEX uniqueness + in-place backfill.
// ALGORITHM/LOCK on CREATE INDEX are accepted and ignored.
// Completeness wave / not this feature: async job queue, concurrent ingest,
// invisible.
use crate::reorg::{INDEX_REORG_STATES, is_write_reorg};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_CANT_DROP_FIELD_OR_KEY, ERR_DUP_ENTRY, ERR_DUP_KEYNAME,
    ERR_NO_SUCH_TABLE, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{IndexInfo, Mutator, TableInfo};
use sealdb_parser::ast::{CreateIndexStmt, DropIndexStmt, IndexDef, TableName};
use sealdb_tablecodec::{
    decode_handle_from_row_key, decode_row, encode_index_key, encode_index_seek_key,
    encode_row_key_with_handle, gen_table_record_prefix, next_prefix,
};
use sealdb_types::Datum;

pub fn create_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateIndexStmt,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    add_index(
        meta,
        is,
        &stmt.table,
        &IndexDef {
            name: stmt.name.clone(),
            columns: stmt.columns.clone(),
            unique: stmt.unique,
            primary: false,
            global: false,
            nonclustered: false,
            clustered: false,
        },
        stmt.if_not_exists,
        current_db,
        storage,
    )
}

pub fn drop_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropIndexStmt,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    remove_index(
        meta,
        is,
        &stmt.table,
        &stmt.name,
        stmt.if_exists,
        current_db,
        storage,
    )
}

pub fn add_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    index: &IndexDef,
    if_not_exists: bool,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, name) = resolve_name(table, current_db)?;
    let (db_id, mut info) = load_table(is, &schema, &name)?;
    if info
        .indices
        .iter()
        .any(|idx| idx.name.o().eq_ignore_ascii_case(&index.name))
    {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_DUP_KEYNAME,
            format!("Duplicate key name '{}'", index.name),
        ));
    }
    let (idx_cols, hidden) =
        crate::expr_index::resolve_index_parts(&info, &index.name, &index.columns)?;
    // Expression parts add hidden VIRTUAL columns; rows written before this
    // index lack them, so they are backfilled before the index backfill runs.
    if !hidden.is_empty() {
        let old_types = info.col_field_types();
        let mut new_col_ids = Vec::with_capacity(hidden.len());
        for col in hidden {
            new_col_ids.push(col.id);
            info.columns.push(col);
            info.max_col_id = info.max_col_id.max(info.columns.last().unwrap().id);
        }
        crate::alter::backfill_generated_columns(storage, &info, &old_types, &new_col_ids)?;
    }
    info.max_idx_id += 1;
    let mut built = IndexInfo::new(info.max_idx_id, index.name.clone(), info.name.o(), idx_cols)
        .with_flags(index.unique, index.primary)
        .with_global(index.global);
    for state in INDEX_REORG_STATES {
        built.state = state;
        if is_write_reorg(state) {
            backfill_index(storage, &info, &built)?;
        }
        if let Some(existing) = info.indices.iter_mut().find(|idx| idx.id == built.id) {
            existing.state = state;
        } else {
            info.indices.push(built.clone());
        }
        meta.update_table(db_id, &info)?;
        meta.gen_schema_version()?;
    }
    Ok(())
}

pub fn remove_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    index_name: &str,
    if_exists: bool,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, name) = resolve_name(table, current_db)?;
    let (db_id, mut info) = load_table(is, &schema, &name)?;
    let Some(pos) = info
        .indices
        .iter()
        .position(|idx| idx.name.o().eq_ignore_ascii_case(index_name))
    else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{index_name}'; check that column/key exists"),
        ));
    };
    let idx_id = info.indices[pos].id;
    drop_index_keys(storage, &info, idx_id)?;
    info.indices.remove(pos);
    meta.update_table(db_id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn load_table(is: &InfoSchema, schema: &str, table: &str) -> Result<(i64, TableInfo)> {
    let db = is
        .schema_by_name(schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let info = is.table_by_name(schema, table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    Ok((db.id, info))
}

pub(crate) fn backfill_index(
    storage: &dyn Storage,
    info: &TableInfo,
    index: &IndexInfo,
) -> Result<()> {
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let handle = decode_handle_from_row_key(&key)?;
            let decoded = decode_row(&raw, &info.col_field_types());
            let values: Vec<Datum> = info
                .columns
                .iter()
                .map(|col| {
                    decoded
                        .iter()
                        .find(|(id, _)| *id == col.id)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Datum::Null)
                })
                .collect();
            let idx_vals: Vec<Datum> = index
                .columns
                .iter()
                .map(|col| {
                    values
                        .get(col.offset as usize)
                        .cloned()
                        .unwrap_or(Datum::Null)
                })
                .collect();
            // Unique indexes omit the handle from the key; a second row with the
            // same indexed values is 1062 even during write-reorg backfill.
            let ikey = encode_index_key(physical_id, index.id, &idx_vals, handle, index.unique);
            if index.unique && txn.get(ikey.as_bytes())?.is_some() {
                let entry = idx_vals
                    .iter()
                    .map(Datum::display)
                    .collect::<Vec<_>>()
                    .join("-");
                return Err(Error::new(
                    ERR_DUP_ENTRY,
                    format!(
                        "Duplicate entry '{entry}' for key '{}.{}'",
                        info.name.o(),
                        index.name.o()
                    ),
                ));
            }
            txn.set(
                ikey.as_bytes(),
                encode_row_key_with_handle(physical_id, handle).as_bytes(),
            )?;
        }
    }
    txn.commit()
}

pub(crate) fn drop_index_keys(storage: &dyn Storage, info: &TableInfo, idx_id: i64) -> Result<()> {
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = encode_index_seek_key(physical_id, idx_id, &[]);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, _) in pairs {
            txn.delete(&key)?;
        }
    }
    txn.commit()
}

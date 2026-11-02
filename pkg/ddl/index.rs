// F18 core: CREATE/DROP INDEX schema + in-place backfill.
// Completeness wave / not this feature: TiDB job states, Reorg,
// invisible, concurrent ingest, ALGORITHM/LOCK.
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_DUP_ENTRY, ERR_DUP_KEYNAME,
    ERR_NO_SUCH_TABLE, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{IndexColumn, IndexInfo, Mutator, TableInfo};
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
    let mut idx_cols = Vec::with_capacity(index.columns.len());
    for col_name in &index.columns {
        let col = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(col_name))
            .ok_or_else(|| {
                Error::new(
                    ERR_BAD_FIELD,
                    format!("Unknown column '{col_name}' in index '{}'", index.name),
                )
            })?;
        idx_cols.push(IndexColumn::new(col.name.o(), col.offset));
    }
    info.max_idx_id += 1;
    let built = IndexInfo::new(info.max_idx_id, index.name.clone(), info.name.o(), idx_cols)
        .with_flags(index.unique, index.primary);
    backfill_index(storage, &info, &built)?;
    info.indices.push(built);
    meta.update_table(db_id, &info)?;
    meta.gen_schema_version()?;
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

fn backfill_index(storage: &dyn Storage, info: &TableInfo, index: &IndexInfo) -> Result<()> {
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
            let ikey = encode_index_key(physical_id, index.id, &idx_vals, handle, index.unique);
            if index.unique && txn.get(ikey.as_bytes())?.is_some() {
                return Err(Error::new(
                    ERR_DUP_ENTRY,
                    format!("Duplicate entry for key '{}'", index.name.o()),
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

fn drop_index_keys(storage: &dyn Storage, info: &TableInfo, idx_id: i64) -> Result<()> {
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

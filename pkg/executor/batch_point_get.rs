// BatchPointGet (TiDB pkg/executor/batch_point_get.go).
// Completeness wave / not this feature: coprocessor batch get, lock wait.
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::TableName;
use sealdb_tablecodec::{
    decode_handle_from_row_key, decode_row, encode_index_key, encode_row_key_with_handle,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct BatchPointGetExec {
    domain: Arc<Domain>,
    table: TableName,
    index: Option<String>,
    lookups: Vec<Datum>,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl BatchPointGetExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        index: Option<String>,
        lookups: Vec<Datum>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        lock: RowLock,
        wait: LockWait,
    ) -> Self {
        Self {
            domain,
            table,
            index,
            lookups,
            current_db: current_db.into(),
            txn,
            lock,
            wait,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for BatchPointGetExec {
    fn open(&mut self) -> Result<()> {
        let (schema, name) = self.table.qualified(&self.current_db);
        if schema.is_empty() {
            return Err(Error::new(ERR_NO_DB, "No database selected"));
        }
        let is = self.domain.infoschema();
        let info = is.table_by_name(&schema, &name).cloned().ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            )
        })?;
        self.schema = info
            .columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect();
        let handles = if let Some(index_name) = &self.index {
            let index = info
                .indices
                .iter()
                .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))
                .cloned()
                .ok_or_else(|| Error::unsupported(format!("index {index_name}")))?;
            self.txn.with_txn(|txn| {
                let mut handles = Vec::new();
                for lookup in &self.lookups {
                    for physical_id in info.physical_ids() {
                        let key = encode_index_key(
                            physical_id,
                            index.id,
                            std::slice::from_ref(lookup),
                            0,
                            true,
                        );
                        let raw = txn.get_locked(key.as_bytes(), self.lock, self.wait)?;
                        if let Some(raw) = raw {
                            handles.push(decode_handle_from_row_key(&raw)?);
                            break;
                        }
                    }
                }
                Ok(handles)
            })?
        } else {
            self.lookups
                .iter()
                .filter_map(|value| match value {
                    Datum::I64(handle) => Some(*handle),
                    Datum::U64(handle) => Some(*handle as i64),
                    _ => None,
                })
                .collect()
        };
        self.rows = self.txn.with_txn(|txn| {
            if self.lock == RowLock::None {
                get_rows_batch(txn, &info, &handles)
            } else {
                let mut rows = Vec::new();
                for handle in handles {
                    if let Some(row) = get_row(txn, &info, handle, self.lock, self.wait)? {
                        rows.push(row);
                    }
                }
                Ok(rows)
            }
        })?;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn physical_ids_for_handle(info: &TableInfo, handle: i64) -> Vec<i64> {
    if info.partition.as_ref().is_some_and(|p| {
        p.column()
            .eq_ignore_ascii_case(info.columns.first().map(|c| c.name.o()).unwrap_or(""))
    }) {
        match info.locate_partition(&Datum::I64(handle)) {
            Ok(id) => vec![id],
            Err(_) => Vec::new(),
        }
    } else {
        info.physical_ids()
    }
}

fn decode_table_row(info: &TableInfo, raw: &[u8]) -> Vec<Datum> {
    let decoded = decode_row(raw, &info.col_field_types());
    info.columns
        .iter()
        .map(|col| {
            decoded
                .iter()
                .find(|(id, _)| *id == col.id)
                .map(|(_, v)| v.clone())
                .unwrap_or(Datum::Null)
        })
        .collect()
}

fn get_row(
    txn: &mut dyn sealdb_kv::Transaction,
    info: &TableInfo,
    handle: i64,
    lock: RowLock,
    wait: LockWait,
) -> Result<Option<Vec<Datum>>> {
    for physical_id in physical_ids_for_handle(info, handle) {
        let key = encode_row_key_with_handle(physical_id, handle);
        let raw = txn.get_locked(key.as_bytes(), lock, wait)?;
        if let Some(raw) = raw {
            return Ok(Some(decode_table_row(info, &raw)));
        }
    }
    Ok(None)
}

/// Unlocked BatchPointGet: one `batch_get` RPC (or Mem loop) for all row keys.
fn get_rows_batch(
    txn: &mut dyn sealdb_kv::Transaction,
    info: &TableInfo,
    handles: &[i64],
) -> Result<Vec<Vec<Datum>>> {
    let mut handle_keys: Vec<(i64, Vec<Vec<u8>>)> = Vec::with_capacity(handles.len());
    let mut all_keys = Vec::new();
    for &handle in handles {
        let keys: Vec<Vec<u8>> = physical_ids_for_handle(info, handle)
            .into_iter()
            .map(|physical_id| {
                encode_row_key_with_handle(physical_id, handle)
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        all_keys.extend(keys.iter().cloned());
        handle_keys.push((handle, keys));
    }
    let pairs = txn.batch_get(&all_keys)?;
    let by_key: std::collections::HashMap<Vec<u8>, Vec<u8>> = pairs.into_iter().collect();
    let mut rows = Vec::new();
    for (_handle, keys) in handle_keys {
        for key in keys {
            if let Some(raw) = by_key.get(&key) {
                rows.push(decode_table_row(info, raw));
                break;
            }
        }
    }
    Ok(rows)
}

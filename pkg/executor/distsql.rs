// F10 core path: IndexLookUpExecutor / covering IndexReader
// (TiDB pkg/executor/distsql.go).
// Completeness wave / not this feature: IndexMerge, lookup push-down,
// parallel workers, composite prefix ranges.
use crate::coprocessor::filter_rows;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{Key, LockWait, RowLock, TxnHandle};
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_parser::ast::{ExprNode, TableName};
use sealdb_tablecodec::{
    decode_handle_from_index_key, decode_handle_from_row_key, decode_index_key_values, decode_row,
    encode_index_seek_key, encode_index_seek_prefix, encode_row_key_with_handle, next_prefix,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum IndexAccess {
    Equal(Datum),
    Range {
        low: Option<Datum>,
        low_inclusive: bool,
        high: Option<Datum>,
        high_inclusive: bool,
    },
}

pub struct IndexLookUpExecutor {
    domain: Arc<Domain>,
    table: TableName,
    index: String,
    access: IndexAccess,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    covering: bool,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl IndexLookUpExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        index: String,
        access: IndexAccess,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        lock: RowLock,
        wait: LockWait,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
        covering: bool,
    ) -> Self {
        Self {
            domain,
            table,
            index,
            access,
            current_db: current_db.into(),
            txn,
            lock,
            wait,
            partitions,
            pushed,
            covering,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for IndexLookUpExecutor {
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
        let index = info
            .indices
            .iter()
            .find(|idx| idx.name.o().eq_ignore_ascii_case(&self.index))
            .cloned()
            .ok_or_else(|| Error::unsupported(format!("index {}", self.index)))?;
        self.schema = info
            .columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect();
        let physical = if self.partitions.is_empty() {
            info.physical_ids()
        } else {
            self.partitions.clone()
        };
        self.rows = self.txn.with_txn(|txn| {
            let mut rows = Vec::new();
            for physical_id in physical {
                let (start, end) = scan_bounds(physical_id, index.id, &self.access);
                if start.as_bytes() >= end.as_slice() {
                    continue;
                }
                let pairs = txn.scan_locked(start.as_bytes(), &end, self.lock, self.wait)?;
                for (key, value) in pairs {
                    let handle = index_handle(&index, &key, &value)?;
                    if self.covering {
                        rows.push(covering_row(&info, &index, &key, handle)?);
                        continue;
                    }
                    let row_key = encode_row_key_with_handle(physical_id, handle);
                    let raw = txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?;
                    if let Some(raw) = raw {
                        let decoded = decode_row(&raw, &info.col_field_types());
                        rows.push(
                            info.columns
                                .iter()
                                .map(|col| {
                                    decoded
                                        .iter()
                                        .find(|(id, _)| *id == col.id)
                                        .map(|(_, v)| v.clone())
                                        .unwrap_or(Datum::Null)
                                })
                                .collect(),
                        );
                    }
                }
            }
            Ok(rows)
        })?;
        self.rows = filter_rows(
            std::mem::take(&mut self.rows),
            &self.schema,
            self.pushed.as_ref(),
        )?;
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

fn scan_bounds(physical_id: i64, index_id: i64, access: &IndexAccess) -> (Key, Vec<u8>) {
    match access {
        IndexAccess::Equal(value) => {
            let start =
                encode_index_seek_prefix(physical_id, index_id, std::slice::from_ref(value));
            let end = next_prefix(start.as_bytes());
            (start, end)
        }
        IndexAccess::Range {
            low,
            low_inclusive,
            high,
            high_inclusive,
        } => {
            let start = match low {
                None => encode_index_seek_key(physical_id, index_id, &[]),
                Some(value) => {
                    let prefix = encode_index_seek_prefix(
                        physical_id,
                        index_id,
                        std::slice::from_ref(value),
                    );
                    if *low_inclusive {
                        prefix
                    } else {
                        Key::from_raw(next_prefix(prefix.as_bytes()))
                    }
                }
            };
            let end = match high {
                None => next_prefix(encode_index_seek_key(physical_id, index_id, &[]).as_bytes()),
                Some(value) => {
                    let prefix = encode_index_seek_prefix(
                        physical_id,
                        index_id,
                        std::slice::from_ref(value),
                    );
                    if *high_inclusive {
                        next_prefix(prefix.as_bytes())
                    } else {
                        prefix.as_bytes().to_vec()
                    }
                }
            };
            (start, end)
        }
    }
}

fn index_handle(index: &IndexInfo, key: &[u8], value: &[u8]) -> Result<i64> {
    if index.unique {
        decode_handle_from_row_key(value)
    } else {
        decode_handle_from_index_key(key)
    }
}

fn covering_row(
    info: &TableInfo,
    index: &IndexInfo,
    key: &[u8],
    handle: i64,
) -> Result<Vec<Datum>> {
    let (values, _) = decode_index_key_values(key, index.columns.len(), index.unique)?;
    let mut row = vec![Datum::Null; info.columns.len()];
    for (col, value) in index.columns.iter().zip(values) {
        if let Some(slot) = row.get_mut(col.offset as usize) {
            *slot = value;
        }
    }
    if let Some(handle_col) = info.handle_column()
        && let Some(slot) = row.get_mut(handle_col.offset as usize)
    {
        *slot = Datum::I64(handle);
    }
    Ok(row)
}

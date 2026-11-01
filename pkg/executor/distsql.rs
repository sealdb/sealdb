// F10 core path: IndexLookUpExecutor (TiDB pkg/executor/distsql.go).
// Completeness wave / not this feature: covering IndexReader, range scan,
// IndexMerge, lookup push-down, parallel workers, unique index PointGet.
use crate::coprocessor::filter_rows;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::{ExprNode, TableName};
use sealdb_tablecodec::{
    decode_handle_from_index_key, decode_row, encode_index_seek_prefix, encode_row_key_with_handle,
    next_prefix,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct IndexLookUpExecutor {
    domain: Arc<Domain>,
    table: TableName,
    index: String,
    lookup: Datum,
    current_db: String,
    txn: Arc<TxnHandle>,
    for_update: bool,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
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
        lookup: Datum,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        for_update: bool,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
    ) -> Self {
        Self {
            domain,
            table,
            index,
            lookup,
            current_db: current_db.into(),
            txn,
            for_update,
            partitions,
            pushed,
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
                let start = encode_index_seek_prefix(
                    physical_id,
                    index.id,
                    std::slice::from_ref(&self.lookup),
                );
                let end = next_prefix(start.as_bytes());
                let pairs = if self.for_update {
                    txn.scan_for_update(start.as_bytes(), &end)?
                } else {
                    txn.scan(start.as_bytes(), &end)?
                };
                for (key, _) in pairs {
                    let handle = decode_handle_from_index_key(&key)?;
                    let row_key = encode_row_key_with_handle(physical_id, handle);
                    let raw = if self.for_update {
                        txn.get_for_update(row_key.as_bytes())?
                    } else {
                        txn.get(row_key.as_bytes())?
                    };
                    if let Some(raw) = raw {
                        let decoded = decode_row(&raw);
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

// F13: FOR UPDATE uses scan_for_update (latest + exclusive lock).
// Completeness wave / not this feature: blocking lock wait, FOR SHARE,
// NOWAIT / SKIP LOCKED / WAIT N, OF table list.
use crate::coprocessor::filter_rows;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_meta::TableInfo;
use sealdb_parser::ast::{ExprNode, TableName};
use sealdb_tablecodec::{
    decode_handle_from_row_key, decode_row, gen_table_record_prefix, next_prefix,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct TableReaderExec {
    domain: Arc<Domain>,
    table: TableName,
    current_db: String,
    txn: Arc<TxnHandle>,
    for_update: bool,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl TableReaderExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        for_update: bool,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
    ) -> Self {
        Self {
            domain,
            table,
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

impl Executor for TableReaderExec {
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
        let physical = if self.partitions.is_empty() {
            info.physical_ids()
        } else {
            self.partitions.clone()
        };
        self.rows = filter_rows(
            scan_table(&self.txn, &info, self.for_update, &physical)?
                .into_iter()
                .map(|row| row.values)
                .collect(),
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

pub(crate) struct ScannedRow {
    pub handle: i64,
    pub physical_id: i64,
    pub values: Vec<Datum>,
}

pub(crate) fn scan_table(
    handle: &TxnHandle,
    info: &TableInfo,
    for_update: bool,
    physical_ids: &[i64],
) -> Result<Vec<ScannedRow>> {
    handle.with_txn(|txn| {
        let mut rows = Vec::new();
        for physical_id in physical_ids {
            let start = gen_table_record_prefix(*physical_id);
            let end = next_prefix(start.as_bytes());
            let pairs = if for_update {
                txn.scan_for_update(start.as_bytes(), &end)?
            } else {
                txn.scan(start.as_bytes(), &end)?
            };
            rows.extend(decode_scanned_rows(pairs, info, *physical_id)?);
        }
        Ok(rows)
    })
}

fn decode_scanned_rows(
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    info: &TableInfo,
    physical_id: i64,
) -> Result<Vec<ScannedRow>> {
    pairs
        .into_iter()
        .map(|(key, raw)| {
            let decoded = decode_row(&raw, &info.col_field_types());
            Ok(ScannedRow {
                handle: decode_handle_from_row_key(&key)?,
                physical_id,
                values: info
                    .columns
                    .iter()
                    .map(|col| {
                        decoded
                            .iter()
                            .find(|(id, _)| *id == col.id)
                            .map(|(_, v)| v.clone())
                            .unwrap_or(Datum::Null)
                    })
                    .collect(),
            })
        })
        .collect()
}

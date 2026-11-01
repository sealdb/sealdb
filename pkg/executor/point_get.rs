use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::TableName;
use sealdb_tablecodec::{decode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct PointGetExec {
    domain: Arc<Domain>,
    table: TableName,
    handle: i64,
    current_db: String,
    txn: Arc<TxnHandle>,
    for_update: bool,
    schema: Vec<String>,
    row: Option<Vec<Datum>>,
    emitted: bool,
}

impl PointGetExec {
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        handle: i64,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        for_update: bool,
    ) -> Self {
        Self {
            domain,
            table,
            handle,
            current_db: current_db.into(),
            txn,
            for_update,
            schema: Vec::new(),
            row: None,
            emitted: false,
        }
    }
}

impl Executor for PointGetExec {
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
        let key = encode_row_key_with_handle(info.id, self.handle);
        self.row = self
            .txn
            .with_txn(|txn| {
                if self.for_update {
                    txn.get_for_update(key.as_bytes())
                } else {
                    txn.get(key.as_bytes())
                }
            })?
            .map(|raw| {
                let decoded = decode_row(&raw);
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
            });
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let Some(row) = self.row.take() else {
            return Ok(None);
        };
        let mut chunk = Chunk::new(self.schema.len(), 1);
        chunk.append_row(row);
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

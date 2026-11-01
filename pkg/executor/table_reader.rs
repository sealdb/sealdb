use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::Storage;
use sealdb_meta::TableInfo;
use sealdb_parser::ast::TableName;
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
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl TableReaderExec {
    pub fn new(domain: Arc<Domain>, table: TableName, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            table,
            current_db: current_db.into(),
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
        self.rows = scan_table(self.domain.as_ref(), &info)?
            .into_iter()
            .map(|row| row.values)
            .collect();
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
    pub values: Vec<Datum>,
}

pub(crate) fn scan_table(domain: &Domain, info: &TableInfo) -> Result<Vec<ScannedRow>> {
    let start = gen_table_record_prefix(info.id);
    let end = next_prefix(start.as_bytes());
    let mut txn = domain.begin()?;
    let pairs = txn.scan(start.as_bytes(), &end)?;
    pairs
        .into_iter()
        .map(|(key, raw)| {
            let decoded = decode_row(&raw);
            Ok(ScannedRow {
                handle: decode_handle_from_row_key(&key)?,
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

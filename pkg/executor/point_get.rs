// F06 core path: handle PointGet + unique-index PointGet
// (TiDB pkg/executor/point_get.go).
// Completeness wave / not this feature: clustered PK beyond integer handle.
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::TableName;
use sealdb_tablecodec::{
    decode_handle_from_row_key, decode_row, encode_index_key, encode_row_key_with_handle,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct PointGetExec {
    domain: Arc<Domain>,
    table: TableName,
    handle: Option<i64>,
    index: Option<String>,
    lookup: Option<Datum>,
    current_db: String,
    txn: Arc<TxnHandle>,
    for_update: bool,
    schema: Vec<String>,
    row: Option<Vec<Datum>>,
    emitted: bool,
}

impl PointGetExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        handle: Option<i64>,
        index: Option<String>,
        lookup: Option<Datum>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        for_update: bool,
    ) -> Self {
        Self {
            domain,
            table,
            handle,
            index,
            lookup,
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
        let handle = if let Some(index_name) = &self.index {
            let index = info
                .indices
                .iter()
                .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))
                .cloned()
                .ok_or_else(|| Error::unsupported(format!("index {index_name}")))?;
            let lookup = self
                .lookup
                .clone()
                .ok_or_else(|| Error::unsupported("index point get without lookup"))?;
            self.txn.with_txn(|txn| {
                for physical_id in info.physical_ids() {
                    let key = encode_index_key(
                        physical_id,
                        index.id,
                        std::slice::from_ref(&lookup),
                        0,
                        true,
                    );
                    let raw = if self.for_update {
                        txn.get_for_update(key.as_bytes())?
                    } else {
                        txn.get(key.as_bytes())?
                    };
                    if let Some(raw) = raw {
                        return decode_handle_from_row_key(&raw).map(Some);
                    }
                }
                Ok(None)
            })?
        } else {
            self.handle
        };
        let Some(handle) = handle else {
            self.row = None;
            return Ok(());
        };
        let physical_ids = if info.partition.as_ref().is_some_and(|p| {
            p.column()
                .eq_ignore_ascii_case(info.columns.first().map(|c| c.name.o()).unwrap_or(""))
        }) {
            match info.locate_partition(&Datum::I64(handle)) {
                Ok(id) => vec![id],
                Err(_) => Vec::new(),
            }
        } else {
            info.physical_ids()
        };
        self.row = self.txn.with_txn(|txn| {
            for physical_id in physical_ids {
                let key = encode_row_key_with_handle(physical_id, handle);
                let raw = if self.for_update {
                    txn.get_for_update(key.as_bytes())?
                } else {
                    txn.get(key.as_bytes())?
                };
                if let Some(raw) = raw {
                    let decoded = decode_row(&raw, &info.col_field_types());
                    return Ok(Some(
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
                    ));
                }
            }
            Ok(None)
        })?;
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

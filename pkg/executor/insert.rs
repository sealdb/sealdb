use crate::index::write_index_keys;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_WRONG_VALUE_COUNT, Error, Result};
use sealdb_expression::eval_expr;
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::InsertStmt;
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct InsertExec {
    domain: Arc<Domain>,
    stmt: InsertStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    done: bool,
}

impl InsertExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: InsertStmt,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
            done: false,
        }
    }
}

impl Executor for InsertExec {
    fn open(&mut self) -> Result<()> {
        let (schema, table) = self.stmt.table.qualified(&self.current_db);
        if schema.is_empty() {
            return Err(Error::new(ERR_NO_DB, "No database selected"));
        }
        let is = self.domain.infoschema();
        let info = is.table_by_name(&schema, &table).ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{table}' doesn't exist"),
            )
        })?;
        self.txn.with_txn(|txn| {
            for row in &self.stmt.rows {
                if row.len() != info.columns.len() && self.stmt.columns.is_empty() {
                    return Err(Error::new(
                        ERR_WRONG_VALUE_COUNT,
                        "Column count doesn't match value count",
                    ));
                }
                let mut values = Vec::with_capacity(info.columns.len());
                let mut col_ids = Vec::with_capacity(info.columns.len());
                if self.stmt.columns.is_empty() {
                    for (col, expr) in info.columns.iter().zip(row.iter()) {
                        col_ids.push(col.id);
                        values.push(eval_expr(expr)?);
                    }
                } else {
                    for col in &info.columns {
                        col_ids.push(col.id);
                        if let Some(idx) = self
                            .stmt
                            .columns
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(col.name.o()))
                        {
                            values.push(eval_expr(&row[idx])?);
                        } else {
                            values.push(sealdb_types::Datum::Null);
                        }
                    }
                }
                let handle = values
                    .first()
                    .and_then(|d| match d {
                        sealdb_types::Datum::I64(v) => Some(*v),
                        _ => None,
                    })
                    .unwrap_or_else(|| self.domain.next_handle(info.id));
                let part_val = info
                    .partition_value(&values)
                    .cloned()
                    .unwrap_or(sealdb_types::Datum::Null);
                let physical_id = info.locate_partition(&part_val)?;
                let key = encode_row_key_with_handle(physical_id, handle);
                txn.set(key.as_bytes(), &encode_row(&col_ids, &values))?;
                write_index_keys(txn, info, handle, &values, physical_id)?;
            }
            Ok(())
        })
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

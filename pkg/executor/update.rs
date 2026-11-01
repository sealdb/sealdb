use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_BAD_FIELD, ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_expression::{eval_expr_with_row, eval_filter};
use sealdb_kv::Storage;
use sealdb_parser::ast::UpdateStmt;
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct UpdateExec {
    domain: Arc<Domain>,
    stmt: UpdateStmt,
    current_db: String,
    done: bool,
}

impl UpdateExec {
    pub fn new(domain: Arc<Domain>, stmt: UpdateStmt, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            done: false,
        }
    }
}

impl Executor for UpdateExec {
    fn open(&mut self) -> Result<()> {
        let (schema, table) = self.stmt.table.qualified(&self.current_db);
        if schema.is_empty() {
            return Err(Error::new(ERR_NO_DB, "No database selected"));
        }
        let is = self.domain.infoschema();
        let info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{table}' doesn't exist"),
            )
        })?;
        let col_names: Vec<String> = info
            .columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect();
        let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
        let rows = scan_table(self.domain.as_ref(), &info)?;
        let mut txn = self.domain.begin()?;
        for scanned in rows {
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &col_names, &scanned.values)?
            {
                continue;
            }
            let mut new_values = scanned.values.clone();
            for item in &self.stmt.list {
                let idx = col_names
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(&item.column))
                    .ok_or_else(|| {
                        Error::new(
                            ERR_BAD_FIELD,
                            format!("Unknown column '{}' in 'field list'", item.column),
                        )
                    })?;
                new_values[idx] = eval_expr_with_row(&item.expr, &col_names, &scanned.values)?;
            }
            let new_handle = new_values
                .first()
                .and_then(|d| match d {
                    Datum::I64(v) => Some(*v),
                    _ => None,
                })
                .unwrap_or(scanned.handle);
            if new_handle != scanned.handle {
                txn.delete(encode_row_key_with_handle(info.id, scanned.handle).as_bytes())?;
            }
            txn.set(
                encode_row_key_with_handle(info.id, new_handle).as_bytes(),
                &encode_row(&col_ids, &new_values),
            )?;
        }
        txn.commit()?;
        Ok(())
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

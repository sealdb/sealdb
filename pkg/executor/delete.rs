// F07 core path: single-table DELETE, including ORDER BY / LIMIT.
// F13: pessimistic DELETE reads latest and takes exclusive locks.
// Completeness wave / not this feature: multi-table DELETE, JOIN / USING,
// RETURNING, MySQL-protocol affected-row counts.
use crate::index::delete_index_keys;
use crate::internal::exec::Executor;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_expression::eval_filter;
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::DeleteStmt;
use sealdb_tablecodec::encode_row_key_with_handle;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct DeleteExec {
    domain: Arc<Domain>,
    stmt: DeleteStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    done: bool,
}

impl DeleteExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: DeleteStmt,
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

impl Executor for DeleteExec {
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
        let for_update = self.txn.with_txn(|txn| Ok(txn.is_pessimistic()))?;
        let physical = info.prune_partitions(self.stmt.selection.as_ref());
        let rows = scan_table(&self.txn, &info, for_update, &physical)?;
        let mut matched = Vec::new();
        for scanned in rows {
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &col_names, &scanned.values)?
            {
                continue;
            }
            matched.push(scanned);
        }
        sort_rows_by(&mut matched, &self.stmt.order_by, &col_names, |row| {
            &row.values
        });
        if let Some(limit) = self.stmt.limit {
            matched.truncate(limit as usize);
        }
        self.txn.with_txn(|txn| {
            for scanned in matched {
                delete_index_keys(
                    txn,
                    &info,
                    scanned.handle,
                    &scanned.values,
                    scanned.physical_id,
                )?;
                txn.delete(
                    encode_row_key_with_handle(scanned.physical_id, scanned.handle).as_bytes(),
                )?;
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

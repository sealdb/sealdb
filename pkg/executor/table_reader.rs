// FOR UPDATE uses exclusive lock; FOR SHARE uses shared lock.
// NOWAIT returns 3572; SKIP LOCKED omits locked rows; Block waits
// until release or SEALDB_LOCK_WAIT_MS. Completeness wave / not this
// feature: WAIT N, OF table list.
use crate::coprocessor::filter_rows;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::{ExprNode, TableName};
use sealdb_tablecodec::{decode_handle_from_row_key, decode_row};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct TableReaderExec {
    domain: Arc<Domain>,
    table: TableName,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    /// Project TiDB's `_tidb_rowid` (the heap table handle) as an extra column.
    with_rowid: bool,
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
        lock: RowLock,
        wait: LockWait,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
        with_rowid: bool,
    ) -> Self {
        Self {
            domain,
            table,
            current_db: current_db.into(),
            txn,
            lock,
            wait,
            partitions,
            pushed,
            with_rowid,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for TableReaderExec {
    fn open(&mut self) -> Result<()> {
        // Recursive working set, or a sibling CTE already materialized this stmt.
        // Prefer physical/resolve name so `FROM cte AS m` still hits working `cte`.
        let cte_key = self.table.resolve_name();
        if let Some((schema, rows)) = crate::cte::lookup_cte_rows(cte_key) {
            self.schema = schema;
            self.rows = filter_rows(rows, &self.schema, self.pushed.as_ref(), None)?;
            return Ok(());
        }
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
            .map(|c| {
                // Hidden expression-index columns keep their place in the row
                // layout but never reach a client (`\u{1}` marks them hidden,
                // see `ExecutorBuilder::run`).
                if c.hidden {
                    format!("\u{1}{}", c.name.o())
                } else {
                    c.name.o().to_string()
                }
            })
            .collect();
        // Empty partitions + pushed filter = dynamic prune at execute.
        let physical = if !self.partitions.is_empty() {
            self.partitions.clone()
        } else if let Some(filter) = self.pushed.as_ref() {
            sealdb_planner::core::prune_partitions(&info, Some(filter))
        } else {
            info.physical_ids()
        };
        let scanned = scan_table_filtered(
            &self.txn,
            &info,
            self.lock,
            self.wait,
            &physical,
            self.pushed.as_ref(),
        )?;
        let scanned_rows = scanned.len();
        // Heap tables expose the row handle as `_tidb_rowid` when referenced.
        let expose_rowid = self.with_rowid && !info.pk_is_handle;
        self.rows = filter_rows(
            scanned
                .into_iter()
                .map(|row| {
                    if expose_rowid {
                        let mut values = row.values;
                        values.push(Datum::I64(row.handle));
                        values
                    } else {
                        row.values
                    }
                })
                .collect(),
            &self.schema,
            self.pushed.as_ref(),
            Some(&info),
        )?;
        // `EXPLAIN ANALYZE` reports the rows a `TableFullScan` read from
        // storage, which is not the row count that reaches its parent.
        // Record under the SQL alias as well: a self join scans one table under
        // several names and the plan labels each scan with its alias.
        sealdb_planner::core::explain::record_scan_stats(&name, scanned_rows, self.rows.len());
        if self.table.name != name {
            sealdb_planner::core::explain::record_scan_stats(
                &self.table.name,
                scanned_rows,
                self.rows.len(),
            );
        }
        if expose_rowid {
            self.schema.push("\u{1}_tidb_rowid".to_string());
        }
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

/// One table row plus the handle / physical table id used to rewrite KV keys.
#[derive(Clone)]
pub(crate) struct ScannedRow {
    pub handle: i64,
    pub physical_id: i64,
    pub values: Vec<Datum>,
}

pub(crate) fn scan_table(
    handle: &TxnHandle,
    info: &TableInfo,
    lock: RowLock,
    wait: LockWait,
    physical_ids: &[i64],
) -> Result<Vec<ScannedRow>> {
    scan_table_filtered(handle, info, lock, wait, physical_ids, None)
}

/// Like [`scan_table`], with the coprocessor selection fed into the scan so
/// row locking only covers rows the query actually returns (TiDB
/// `SelectLockExec` sits above the `Selection`).
pub(crate) fn scan_table_filtered(
    handle: &TxnHandle,
    info: &TableInfo,
    lock: RowLock,
    wait: LockWait,
    physical_ids: &[i64],
    filter: Option<&ExprNode>,
) -> Result<Vec<ScannedRow>> {
    handle.with_txn(|txn| {
        let mut rows = Vec::new();
        let start_ts = 0;
        for physical_id in physical_ids {
            // DistSQL / Cop scheduling: Region-split tipb TableScan tasks, then
            // local KV execute (tipb chunk decode from TiKV RPC still deferred).
            let tasks = crate::cop_client::build_table_cop_tasks(info, *physical_id, start_ts)?;
            if let Some(remote) =
                crate::cop_client::try_execute_cop_tasks_remote(info, *physical_id, &tasks)
            {
                rows.extend(remote);
            } else {
                rows.extend(crate::cop_client::execute_cop_tasks_locally(
                    txn,
                    info,
                    *physical_id,
                    &tasks,
                    lock,
                    wait,
                    filter,
                )?);
            }
        }
        Ok(rows)
    })
}

pub(crate) fn decode_scanned_rows(
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

//! TableReader DistSQL / Cop scheduling: build tipb CopTasks, execute locally
//! via KV scan (always), optionally probe TiKV Coprocessor RPC when
//! `SEALDB_USE_COP_RPC=1` (tipb chunk→Datum decode still deferred).

use crate::table_reader::{decode_scanned_rows, ScannedRow};
use sealdb_errno::Result;
use sealdb_kv::{LockWait, RowLock, Transaction};
use sealdb_meta::TableInfo;
use sealdb_store::{build_table_scan_tasks, pd_endpoints, ColumnInfo, CopTask};
use sealdb_tablecodec::{gen_table_record_prefix, next_prefix};

/// Build Region-split TableScan CopTasks for a physical table id.
pub(crate) fn build_table_cop_tasks(
    info: &TableInfo,
    physical_id: i64,
    start_ts: u64,
) -> Result<Vec<CopTask>> {
    let start = gen_table_record_prefix(physical_id);
    let end = next_prefix(start.as_bytes());
    let handle_id = info.handle_column().map(|c| c.id);
    let columns: Vec<ColumnInfo> = info
        .columns
        .iter()
        .map(|c| ColumnInfo {
            column_id: c.id,
            tp: i32::from(c.field_type.tp),
            pk_handle: info.pk_is_handle && handle_id == Some(c.id),
        })
        .collect();
    let offsets: Vec<i64> = (0..columns.len() as i64).collect();
    build_table_scan_tasks(
        &pd_endpoints(),
        start_ts,
        physical_id,
        &columns,
        &offsets,
        start.as_bytes(),
        &end,
        None,
        None,
    )
}

/// Run CopTasks as local KV scans (MemStorage and TiKV txn buffer path).
pub(crate) fn execute_cop_tasks_locally(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    physical_id: i64,
    tasks: &[CopTask],
    lock: RowLock,
    wait: LockWait,
) -> Result<Vec<ScannedRow>> {
    let mut rows = Vec::new();
    for task in tasks {
        for range in &task.ranges {
            let pairs = txn.scan_locked(&range.start, &range.end, lock, wait)?;
            rows.extend(decode_scanned_rows(pairs, info, physical_id)?);
        }
    }
    Ok(rows)
}

/// Whether to probe TiKV Coprocessor RPC (does not yet replace local decode).
pub(crate) fn cop_rpc_probe_enabled() -> bool {
    std::env::var("SEALDB_USE_COP_RPC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fire-and-forget Cop RPC probe when PD is configured; ignores results.
pub(crate) fn probe_cop_rpc(tasks: &[CopTask]) {
    if !cop_rpc_probe_enabled() {
        return;
    }
    let pd = pd_endpoints();
    if pd.is_empty() {
        return;
    }
    for task in tasks {
        match sealdb_store::execute_cop_task_blocking(&pd, task) {
            Ok(_) => {}
            Err(err) => eprintln!("sealdb cop rpc probe: {err}"),
        }
    }
}

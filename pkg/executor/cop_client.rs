//! TableReader DistSQL / Cop scheduling: build tipb CopTasks, execute locally
//! via KV scan, and optionally consume TiKV Coprocessor TypeDefault rows when
//! `SEALDB_USE_COP_RPC=1`.

use crate::table_reader::{ScannedRow, decode_scanned_rows};
use sealdb_errno::Result;
use sealdb_kv::{LockWait, RowLock, Transaction};
use sealdb_meta::TableInfo;
use sealdb_store::{ColumnInfo, CopTask, build_table_scan_tasks, decode_select_rows, pd_endpoints};
use sealdb_tablecodec::{gen_table_record_prefix, next_prefix};
use sealdb_types::Datum;

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

/// Whether to use TiKV Coprocessor RPC results (TypeDefault decode).
pub(crate) fn cop_rpc_enabled() -> bool {
    std::env::var("SEALDB_USE_COP_RPC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Try remote Cop RPC + tipb TypeDefault decode; `None` means fall back to local KV.
pub(crate) fn try_execute_cop_tasks_remote(
    info: &TableInfo,
    physical_id: i64,
    tasks: &[CopTask],
) -> Option<Vec<ScannedRow>> {
    if !cop_rpc_enabled() {
        return None;
    }
    let pd = pd_endpoints();
    if pd.is_empty() {
        return None;
    }
    let col_tps: Vec<u8> = info.columns.iter().map(|c| c.field_type.tp).collect();
    let col_count = col_tps.len();
    let mut rows = Vec::new();
    for task in tasks {
        let resp = match sealdb_store::execute_cop_task_blocking(&pd, task) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("sealdb cop rpc: {err}; falling back to local scan");
                return None;
            }
        };
        let decoded = match decode_select_rows(&resp, &col_tps) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("sealdb tipb decode: {err}; falling back to local scan");
                return None;
            }
        };
        for (i, values) in decoded.into_iter().enumerate() {
            rows.push(ScannedRow {
                handle: (i as i64) + 1,
                physical_id,
                values: pad_row(values, col_count),
            });
        }
    }
    Some(rows)
}

fn pad_row(mut values: Vec<Datum>, col_count: usize) -> Vec<Datum> {
    if values.len() < col_count {
        values.resize(col_count, Datum::Null);
    } else if values.len() > col_count {
        values.truncate(col_count);
    }
    values
}

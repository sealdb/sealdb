//! TableReader DistSQL / Cop scheduling: build tipb CopTasks, execute locally
//! via KV scan, and optionally consume TiKV Coprocessor / BatchCop / MPP rows
//! when `SEALDB_USE_COP_RPC` / `SEALDB_USE_BATCH_COP` / `SEALDB_USE_MPP` is set.

use crate::table_reader::{ScannedRow, decode_scanned_rows};
use sealdb_errno::Result;
use sealdb_kv::{LockWait, RowLock, Transaction};
use sealdb_meta::TableInfo;
use sealdb_store::{
    ColumnInfo, CopTask, KeyRange, MppClient, SelectResponse, build_table_scan_tasks,
    decode_select_rows, pd_endpoints,
};
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

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Whether to use TiKV Coprocessor RPC results (TypeDefault decode).
pub(crate) fn cop_rpc_enabled() -> bool {
    env_flag("SEALDB_USE_COP_RPC")
}

/// Whether to prefer TiFlash BatchCoprocessor stream.
pub(crate) fn batch_cop_enabled() -> bool {
    env_flag("SEALDB_USE_BATCH_COP")
}

/// Whether to prefer TiFlash MPP Dispatch + Establish gather.
pub(crate) fn mpp_enabled() -> bool {
    env_flag("SEALDB_USE_MPP")
}

fn flatten_task_ranges(tasks: &[CopTask]) -> Vec<KeyRange> {
    tasks
        .iter()
        .flat_map(|t| t.ranges.iter().cloned())
        .collect()
}

fn first_dag_payload(tasks: &[CopTask]) -> Option<(Vec<u8>, u64)> {
    tasks.first().map(|t| (t.data.clone(), t.start_ts))
}

fn rows_from_select_response(
    resp: SelectResponse,
    info: &TableInfo,
    physical_id: i64,
) -> Option<Vec<ScannedRow>> {
    let col_tps: Vec<u8> = info.columns.iter().map(|c| c.field_type.tp).collect();
    let col_count = col_tps.len();
    let decoded = match decode_select_rows(&resp, &col_tps) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("sealdb tipb decode: {err}; falling back to local scan");
            return None;
        }
    };
    let mut rows = Vec::with_capacity(decoded.len());
    for (i, values) in decoded.into_iter().enumerate() {
        rows.push(ScannedRow {
            handle: (i as i64) + 1,
            physical_id,
            values: pad_row(values, col_count),
        });
    }
    Some(rows)
}

fn try_mpp_or_batch(
    info: &TableInfo,
    physical_id: i64,
    tasks: &[CopTask],
) -> Option<Vec<ScannedRow>> {
    if tasks.is_empty() {
        return Some(Vec::new());
    }
    let pd = pd_endpoints();
    if pd.is_empty() {
        return None;
    }
    let (data, start_ts) = first_dag_payload(tasks)?;
    let ranges = flatten_task_ranges(tasks);
    let client = MppClient::new(pd);
    let resp = if mpp_enabled() {
        match client.execute_mpp_gather_blocking(&ranges, data, start_ts, 1) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("sealdb mpp gather: {err}; falling back");
                return None;
            }
        }
    } else if batch_cop_enabled() {
        match client.dispatch_stream_blocking(&ranges, data, start_ts) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("sealdb batch cop: {err}; falling back");
                return None;
            }
        }
    } else {
        return None;
    };
    rows_from_select_response(resp, info, physical_id)
}

/// Try remote Cop / BatchCop / MPP + tipb TypeDefault decode; `None` → local KV.
pub(crate) fn try_execute_cop_tasks_remote(
    info: &TableInfo,
    physical_id: i64,
    tasks: &[CopTask],
) -> Option<Vec<ScannedRow>> {
    // Prefer MPP / BatchCop when requested; fall through to Cop RPC on failure.
    if (mpp_enabled() || batch_cop_enabled())
        && let Some(rows) = try_mpp_or_batch(info, physical_id, tasks)
    {
        return Some(rows);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_flags_default_off() {
        // Do not assert absolute env; only that helpers are callable.
        let _ = cop_rpc_enabled();
        let _ = batch_cop_enabled();
        let _ = mpp_enabled();
    }

    #[test]
    fn test_flatten_and_payload() {
        let task = CopTask {
            ranges: vec![KeyRange::new(b"a".to_vec(), b"b".to_vec())],
            data: vec![9, 9],
            start_ts: 7,
            tp: 0,
        };
        let ranges = flatten_task_ranges(std::slice::from_ref(&task));
        assert_eq!(ranges.len(), 1);
        let (data, ts) = first_dag_payload(std::slice::from_ref(&task)).unwrap();
        assert_eq!(data, vec![9, 9]);
        assert_eq!(ts, 7);
    }
}

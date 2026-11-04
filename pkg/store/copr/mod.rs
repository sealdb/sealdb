//! TiDB `pkg/store/copr`: Region-aware key-range split + Cop task scheduling.
//!
//! `split_key_ranges` is the Mem / no-PD fallback (single span).
//! `split_key_ranges_via_pd` walks PD HTTP `/pd/api/v1/region/key/{hex}` to
//! produce Region-aligned half-open ranges for Cop / TableReader.
//! `split_key_ranges_for_scan` uses process PD endpoints set by `store::open`.
//! tipb DAG encode / CopTask / Coprocessor RPC live in `dag` / `task` / `rpc`.

pub mod batch_coprocessor;
pub mod batch_request_sender;
pub mod coprocessor;
pub mod coprocessor_cache;
pub mod dag;
pub mod key_ranges;
pub mod mpp;
pub mod mpp_probe;
pub mod pd;
pub mod region_cache;
pub mod rpc;
pub mod store;
pub mod task;

pub use dag::{
    decode_default_rows, decode_select_response, decode_select_rows, encode_default_row,
    encode_select_response, encode_select_response_chunk, encode_table_scan_dag, Chunk, ColumnInfo,
    DagRequest, EncodeType, ExecType, Executor, IndexScan, Limit, RowMeta, SelectResponse,
    Selection, TableScan, TipbError, REQ_TYPE_DAG,
};
pub use rpc::{execute_cop_task, execute_cop_task_blocking};
pub use task::{build_table_scan_tasks, CopTask};

use pd::{decode_pd_key, pd_get_region};
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::sync::Mutex;

static PD_ENDPOINTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record PD HTTP endpoints used by `split_key_ranges_for_scan` (called from `store::open`).
pub fn set_pd_endpoints(endpoints: Vec<String>) {
    if let Ok(mut guard) = PD_ENDPOINTS.lock() {
        *guard = endpoints;
    }
}

/// Clear process PD endpoints (Mem store / tests).
pub fn clear_pd_endpoints() {
    set_pd_endpoints(Vec::new());
}

/// Current process PD endpoints (empty for MemStorage).
pub fn pd_endpoints() -> Vec<String> {
    PD_ENDPOINTS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Half-open key range `[start, end)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Vec<u8>,
}

impl KeyRange {
    pub fn new(start: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>) -> Self {
        Self {
            start: start.into(),
            end: end.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Split `[start, end)` into a single span (MemStorage / offline fallback).
pub fn split_key_ranges(start: &[u8], end: &[u8]) -> Vec<KeyRange> {
    if end <= start {
        return Vec::new();
    }
    vec![KeyRange::new(start.to_vec(), end.to_vec())]
}

/// Prefer PD Region locate when endpoints are configured; otherwise single span.
/// On PD errors, falls back to a single contiguous range so scans stay available.
pub fn split_key_ranges_for_scan(start: &[u8], end: &[u8]) -> Vec<KeyRange> {
    let endpoints = PD_ENDPOINTS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if endpoints.is_empty() {
        return split_key_ranges(start, end);
    }
    match split_key_ranges_via_pd(&endpoints, start, end) {
        Ok(ranges) if !ranges.is_empty() => ranges,
        _ => split_key_ranges(start, end),
    }
}

/// Split `[start, end)` by querying PD for the Region containing each cursor.
///
/// Empty `end_key` from PD means the last Region (extends to `end`).
pub fn split_key_ranges_via_pd(
    pd_endpoints: &[String],
    start: &[u8],
    end: &[u8],
) -> Result<Vec<KeyRange>> {
    if end <= start {
        return Ok(Vec::new());
    }
    if pd_endpoints.is_empty() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "empty PD endpoints"));
    }
    let mut out = Vec::new();
    let mut cursor = start.to_vec();
    // Cap iterations so a bad PD cannot loop forever.
    for _ in 0..10_000 {
        if cursor.as_slice() >= end {
            break;
        }
        let region = pd_get_region(pd_endpoints, &cursor)?;
        let region_start = decode_pd_key(&region.start_key)?;
        let region_end = if region.end_key.is_empty() {
            end.to_vec()
        } else {
            decode_pd_key(&region.end_key)?
        };
        let range_start = if cursor.as_slice() > region_start.as_slice() {
            cursor.clone()
        } else {
            region_start
        };
        let range_end = if region_end.as_slice() < end {
            region_end.clone()
        } else {
            end.to_vec()
        };
        if range_start.as_slice() < range_end.as_slice() {
            out.push(KeyRange::new(range_start, range_end.clone()));
        }
        if region.end_key.is_empty() || region_end.as_slice() >= end {
            break;
        }
        cursor = region_end;
    }
    if out.is_empty() {
        // PD unreachable / empty meta: fall back to a single span.
        out.push(KeyRange::new(start.to_vec(), end.to_vec()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_key_ranges_mvb_single() {
        let ranges = split_key_ranges(b"a", b"z");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, b"a");
        assert_eq!(ranges[0].end, b"z");
        assert!(split_key_ranges(b"z", b"a").is_empty());
        assert!(split_key_ranges(b"a", b"a").is_empty());
    }

    #[test]
    fn test_hex_roundtrip() {
        use super::pd::hex_encode;
        let raw = b"\x01\xab\xff";
        let hex = hex_encode(raw);
        assert_eq!(hex, "01abff");
        assert_eq!(decode_pd_key(&hex).unwrap(), raw);
        assert!(decode_pd_key("").unwrap().is_empty());
    }

    #[test]
    fn test_split_key_ranges_via_pd_if_available() {
        let ranges = match split_key_ranges_via_pd(
            &["127.0.0.1:2379".to_string()],
            b"t",
            b"u",
        ) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("skip pd region split: {err}");
                return;
            }
        };
        assert!(!ranges.is_empty());
        assert_eq!(ranges.first().unwrap().start, b"t");
        assert_eq!(ranges.last().unwrap().end, b"u");
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }
}

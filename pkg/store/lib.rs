#![forbid(unsafe_code)]

pub mod copr;
pub mod driver;

use driver::TikvStorage;
use sealdb_config::{Config, StoreKind};
use sealdb_errno::Result;
use sealdb_kv::{MemStorage, Storage};
use std::sync::Arc;

pub use copr::pd::{
    pd_get_region, pd_get_store, pd_list_stores, pd_scan_regions, pd_store_address, PdClient,
    PdPeer, PdRegion, PdRegionEpoch, PdStoreInfo,
};
pub use copr::{
    build_table_scan_tasks, clear_pd_endpoints, decode_default_rows, decode_select_response,
    decode_select_rows, encode_default_row, encode_select_response, encode_table_scan_dag,
    execute_cop_task, execute_cop_task_blocking, pd_endpoints, set_pd_endpoints, split_key_ranges,
    split_key_ranges_for_scan, split_key_ranges_via_pd, Chunk, ColumnInfo, CopTask, DagRequest,
    EncodeType, ExecType, Executor, IndexScan, KeyRange, Limit, RowMeta, SelectResponse,
    Selection, TableScan, TipbError, REQ_TYPE_DAG,
};
pub use driver::TikvStorage as TikvStore;

pub fn open(cfg: &Config) -> Result<Arc<dyn Storage>> {
    match cfg.store {
        StoreKind::Memory => {
            clear_pd_endpoints();
            Ok(Arc::new(MemStorage::new()))
        }
        StoreKind::Tikv => {
            set_pd_endpoints(cfg.pd_endpoints.clone());
            Ok(Arc::new(TikvStorage::connect(&cfg.pd_endpoints)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_config::StoreKind;

    #[test]
    fn test_open_memory() {
        let cfg = Config::default();
        assert_eq!(cfg.store, StoreKind::Memory);
        let storage = open(&cfg).unwrap();
        let mut txn = storage.begin().unwrap();
        txn.set(b"sealdb:mem:k", b"v").unwrap();
        txn.commit().unwrap();
        let mut read = storage.begin().unwrap();
        assert_eq!(read.get(b"sealdb:mem:k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_mem_current_timestamp_monotonic() {
        let storage = open(&Config::default()).unwrap();
        let a = storage.current_timestamp().unwrap();
        let b = storage.current_timestamp().unwrap();
        assert!(b > a, "expected monotonic TSO-like counter, got {a} then {b}");
    }

    #[test]
    fn test_tikv_current_timestamp_if_available() {
        let store = match TikvStorage::connect(&["127.0.0.1:2379".to_string()]) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("skip live tikv tso: {err}");
                return;
            }
        };
        let a = store.current_timestamp().unwrap();
        let b = store.current_timestamp().unwrap();
        assert!(b >= a, "PD TSO should be non-decreasing, got {a} then {b}");
    }

    #[test]
    fn test_tikv_roundtrip_if_available() {
        let store = match TikvStorage::connect(&["127.0.0.1:2379".to_string()]) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("skip live tikv: {err}");
                return;
            }
        };
        let key = format!(
            "sealdb:test:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let key = key.into_bytes();
        let mut txn = store.begin().unwrap();
        txn.set(&key, b"v1").unwrap();
        txn.commit().unwrap();
        let mut read = store.begin().unwrap();
        assert_eq!(read.get(&key).unwrap(), Some(b"v1".to_vec()));
        read.delete(&key).unwrap();
        read.commit().unwrap();
    }
}

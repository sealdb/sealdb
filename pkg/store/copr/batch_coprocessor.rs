//! TiDB `pkg/store/copr/batch_coprocessor.go` — BatchCop task grouping / balance.
//!
//! Unary Cop per Region remains in `rpc`. This module builds store-batched tasks:
//! Region-split ranges assigned to TiKV leaders or TiFlash stores (round-robin).

use super::dag::REQ_TYPE_DAG;
use super::pd::{decode_pd_key, pd_get_region, pd_store_address, PdRegion, PdStoreInfo};
use super::KeyRange;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::collections::HashMap;

/// One BatchCop unit: ranges destined for a single store (TiDB `batchCopTask`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchCopTask {
    pub store_id: u64,
    pub store_addr: String,
    pub region_id: u64,
    pub region_epoch_conf_ver: u64,
    pub region_epoch_version: u64,
    pub peer_id: u64,
    pub ranges: Vec<KeyRange>,
    /// Encoded tipb.DAGRequest (shared payload across ranges on this store).
    pub data: Vec<u8>,
    pub tp: i64,
    pub start_ts: u64,
    pub is_tiflash: bool,
}

impl BatchCopTask {
    pub fn to_cop_task(&self) -> super::task::CopTask {
        super::task::CopTask {
            ranges: self.ranges.clone(),
            data: self.data.clone(),
            tp: self.tp,
            start_ts: self.start_ts,
        }
    }
}

/// Preference when picking stores for BatchCop fragments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchStorePrefer {
    /// Region leader TiKV store (default BatchCop-to-TiKV).
    TiKvLeader,
    /// Round-robin across provided TiFlash stores.
    TiFlash,
}

/// Locate Region-aligned fragments for `[start, end)` (same walk as
/// `split_key_ranges_via_pd`, but keeps PD region metadata).
pub fn locate_region_fragments(
    pd_endpoints: &[String],
    start: &[u8],
    end: &[u8],
) -> Result<Vec<(KeyRange, PdRegion)>> {
    if end <= start {
        return Ok(Vec::new());
    }
    if pd_endpoints.is_empty() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "empty PD endpoints"));
    }
    let mut out = Vec::new();
    let mut cursor = start.to_vec();
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
            out.push((KeyRange::new(range_start, range_end), region.clone()));
        }
        if region.end_key.is_empty() || region_end.as_slice() >= end {
            break;
        }
        cursor = region_end;
    }
    Ok(out)
}

/// Build BatchCop tasks for the given half-open ranges.
///
/// - [`BatchStorePrefer::TiKvLeader`]: each fragment goes to its Region leader.
/// - [`BatchStorePrefer::TiFlash`]: fragments round-robin across `tiflash_stores`
///   (must be non-empty).
///
/// Fragments that share `(store_id, region_id)` are coalesced into one task.
pub fn build_batch_cop_tasks(
    pd_endpoints: &[String],
    prefer: BatchStorePrefer,
    tiflash_stores: &[PdStoreInfo],
    ranges: &[KeyRange],
    data: Vec<u8>,
    start_ts: u64,
) -> Result<Vec<BatchCopTask>> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(prefer, BatchStorePrefer::TiFlash) && tiflash_stores.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            "build_batch_cop_tasks: no TiFlash stores for TiFlash prefer",
        ));
    }

    // key: (store_id, region_id) → task under construction
    let mut grouped: HashMap<(u64, u64), BatchCopTask> = HashMap::new();
    let mut rr = 0usize;

    for span in ranges {
        if span.is_empty() {
            continue;
        }
        let fragments = locate_region_fragments(pd_endpoints, &span.start, &span.end)?;
        for (range, region) in fragments {
            let (store_id, store_addr, is_tiflash, peer_id) = match prefer {
                BatchStorePrefer::TiKvLeader => {
                    let store_id = region
                        .leader
                        .as_ref()
                        .map(|p| p.store_id)
                        .filter(|id| *id != 0)
                        .ok_or_else(|| {
                            Error::new(
                                ERR_UNKNOWN_ERROR,
                                format!("region {} missing leader store_id", region.id),
                            )
                        })?;
                    let addr = pd_store_address(pd_endpoints, store_id)?;
                    let peer_id = region.leader.as_ref().map(|p| p.id).unwrap_or(0);
                    (store_id, addr, false, peer_id)
                }
                BatchStorePrefer::TiFlash => {
                    let store = &tiflash_stores[rr % tiflash_stores.len()];
                    rr = rr.wrapping_add(1);
                    (store.id, store.address.clone(), true, 0)
                }
            };
            let conf_ver = region.epoch.as_ref().map(|e| e.conf_ver).unwrap_or(0);
            let version = region.epoch.as_ref().map(|e| e.version).unwrap_or(0);
            let key = (store_id, region.id);
            grouped
                .entry(key)
                .and_modify(|t| t.ranges.push(range.clone()))
                .or_insert_with(|| BatchCopTask {
                    store_id,
                    store_addr: store_addr.clone(),
                    region_id: region.id,
                    region_epoch_conf_ver: conf_ver,
                    region_epoch_version: version,
                    peer_id,
                    ranges: vec![range],
                    data: data.clone(),
                    tp: REQ_TYPE_DAG,
                    start_ts,
                    is_tiflash,
                });
        }
    }

    let mut tasks: Vec<BatchCopTask> = grouped.into_values().collect();
    tasks.sort_by(|a, b| {
        (a.store_id, a.region_id).cmp(&(b.store_id, b.region_id))
    });
    Ok(tasks)
}

/// Re-balance tasks across `stores` by round-robin reassignment of ranges.
///
/// MVB of TiDB `balanceBatchCopTask`: equalizes fragment count per store without
/// region-affinity heuristics.
pub fn balance_batch_cop_tasks(
    stores: &[PdStoreInfo],
    tasks: Vec<BatchCopTask>,
    is_tiflash: bool,
) -> Vec<BatchCopTask> {
    if stores.is_empty() || tasks.is_empty() {
        return tasks;
    }
    // Flatten to (range, region meta, payload) then redistribute.
    let mut flat: Vec<(KeyRange, BatchCopTask)> = Vec::new();
    for t in tasks {
        for r in &t.ranges {
            let mut meta = t.clone();
            meta.ranges = vec![r.clone()];
            flat.push((r.clone(), meta));
        }
    }
    if flat.is_empty() {
        return Vec::new();
    }

    let mut by_store: HashMap<u64, BatchCopTask> = HashMap::new();
    for (i, (range, meta)) in flat.into_iter().enumerate() {
        let store = &stores[i % stores.len()];
        let key = store.id;
        by_store
            .entry(key)
            .and_modify(|t| t.ranges.push(range.clone()))
            .or_insert_with(|| BatchCopTask {
                store_id: store.id,
                store_addr: store.address.clone(),
                region_id: meta.region_id,
                region_epoch_conf_ver: meta.region_epoch_conf_ver,
                region_epoch_version: meta.region_epoch_version,
                peer_id: if is_tiflash { 0 } else { meta.peer_id },
                ranges: vec![range],
                data: meta.data,
                tp: meta.tp,
                start_ts: meta.start_ts,
                is_tiflash,
            });
    }
    let mut out: Vec<BatchCopTask> = by_store.into_values().collect();
    out.sort_by_key(|t| t.store_id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copr::pd::{PdStoreInfo, PdStoreLabel};

    fn flash_store(id: u64, addr: &str) -> PdStoreInfo {
        PdStoreInfo {
            id,
            address: addr.into(),
            state_name: "Up".into(),
            labels: vec![PdStoreLabel {
                key: "engine".into(),
                value: "tiflash".into(),
            }],
        }
    }

    #[test]
    fn test_balance_round_robin() {
        let stores = vec![
            flash_store(10, "127.0.0.1:3930"),
            flash_store(20, "127.0.0.1:3931"),
        ];
        let seed = BatchCopTask {
            store_id: 10,
            store_addr: "127.0.0.1:3930".into(),
            region_id: 1,
            region_epoch_conf_ver: 1,
            region_epoch_version: 1,
            peer_id: 0,
            ranges: vec![
                KeyRange::new(b"a".to_vec(), b"b".to_vec()),
                KeyRange::new(b"c".to_vec(), b"d".to_vec()),
                KeyRange::new(b"e".to_vec(), b"f".to_vec()),
            ],
            data: vec![1, 2, 3],
            tp: REQ_TYPE_DAG,
            start_ts: 42,
            is_tiflash: true,
        };
        let balanced = balance_batch_cop_tasks(&stores, vec![seed], true);
        assert_eq!(balanced.len(), 2);
        let total: usize = balanced.iter().map(|t| t.ranges.len()).sum();
        assert_eq!(total, 3);
        assert!(balanced.iter().all(|t| t.is_tiflash));
        assert!(balanced.iter().any(|t| t.store_id == 10));
        assert!(balanced.iter().any(|t| t.store_id == 20));
    }

    #[test]
    fn test_build_batch_cop_tasks_tiflash_requires_stores() {
        let err = build_batch_cop_tasks(
            &["127.0.0.1:2379".to_string()],
            BatchStorePrefer::TiFlash,
            &[],
            &[KeyRange::new(b"a".to_vec(), b"b".to_vec())],
            vec![],
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no TiFlash"));
    }

    #[test]
    fn test_build_batch_cop_tasks_if_pd_available() {
        let pd = vec!["127.0.0.1:2379".to_string()];
        let tasks = match build_batch_cop_tasks(
            &pd,
            BatchStorePrefer::TiKvLeader,
            &[],
            &[KeyRange::new(b"t".to_vec(), b"u".to_vec())],
            vec![9],
            7,
        ) {
            Ok(t) => t,
            Err(err) => {
                eprintln!("skip batch build: {err}");
                return;
            }
        };
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|t| !t.is_tiflash));
        assert!(tasks.iter().all(|t| t.start_ts == 7 && t.data == vec![9]));
        assert!(tasks.iter().all(|t| !t.store_addr.is_empty()));
    }
}

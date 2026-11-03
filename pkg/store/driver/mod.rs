/// TiKV driver (TiDB `pkg/store/driver`). Planner/Executor must not
/// import `tikv-client` types; they talk to `sealdb_kv::Storage` only.
/// Completeness wave: coprocessor, async/pipelined commit, stale read, GC,
/// Region cache. Pessimistic begin + `get_for_update` + `batch_get` landed.
/// READ COMMITTED `refresh_snapshot` uses a fresh PD TSO snapshot for plain
/// reads while keeping the underlying txn buffer for writes/locks.
use sealdb_errno::{
    ERR_LOCK_DEADLOCK, ERR_LOCK_NOWAIT, ERR_LOCK_WAIT_TIMEOUT, ERR_UNKNOWN_ERROR,
    ERR_WRITE_CONFLICT, Error, Result,
};
use sealdb_kv::{Storage, Transaction};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::{
    Key, KvPair, Snapshot, Transaction as TikvTxn, TransactionClient, TransactionOptions,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const SCAN_LIMIT: u32 = 1024;

pub struct TikvStorage {
    client: TransactionClient,
    rt: Arc<tokio::runtime::Runtime>,
    pd_endpoints: Vec<String>,
}

impl TikvStorage {
    pub fn connect(pd_endpoints: &[String]) -> Result<Self> {
        if pd_endpoints.is_empty() {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "empty PD endpoints"));
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("sealdb-tikv")
            .build()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tokio runtime: {e}")))?;
        let pd = pd_endpoints.to_vec();
        let client = match rt.block_on(async {
            tokio::time::timeout(CONNECT_TIMEOUT, TransactionClient::new(pd)).await
        }) {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => return Err(map_tikv_err(err)),
            Err(_) => {
                return Err(Error::new(ERR_UNKNOWN_ERROR, "timed out connecting to PD"));
            }
        };
        Ok(Self {
            client,
            rt: Arc::new(rt),
            pd_endpoints: pd_endpoints.to_vec(),
        })
    }

    /// Region-aligned split of `[start, end)` via PD HTTP locate.
    pub fn split_key_ranges(&self, start: &[u8], end: &[u8]) -> Result<Vec<crate::copr::KeyRange>> {
        crate::copr::split_key_ranges_via_pd(&self.pd_endpoints, start, end)
    }

    /// Build tipb TableScan CopTasks split by Region (send RPC lands next).
    pub fn build_table_scan_tasks(
        &self,
        start_ts: u64,
        table_id: i64,
        columns: &[crate::copr::ColumnInfo],
        output_offsets: &[i64],
        start: &[u8],
        end: &[u8],
        selection: Option<crate::copr::Selection>,
        limit: Option<u64>,
    ) -> Result<Vec<crate::copr::CopTask>> {
        crate::copr::build_table_scan_tasks(
            &self.pd_endpoints,
            start_ts,
            table_id,
            columns,
            output_offsets,
            start,
            end,
            selection,
            limit,
        )
    }

    fn begin_txn(&self, pessimistic: bool) -> Result<Box<dyn Transaction>> {
        let txn = if pessimistic {
            self.rt
                .block_on(self.client.begin_pessimistic())
                .map_err(map_tikv_err)?
        } else {
            self.rt
                .block_on(self.client.begin_optimistic())
                .map_err(map_tikv_err)?
        };
        Ok(Box::new(TikvTransaction {
            inner: Some(txn),
            client: self.client.clone(),
            rt: Arc::clone(&self.rt),
            pessimistic,
            read_snapshot: None,
            writes: HashMap::new(),
        }))
    }
}

impl Storage for TikvStorage {
    fn begin(&self) -> Result<Box<dyn Transaction>> {
        self.begin_txn(false)
    }

    fn begin_pessimistic(&self) -> Result<Box<dyn Transaction>> {
        self.begin_txn(true)
    }

    fn current_timestamp(&self) -> Result<u64> {
        use tikv_client::TimestampExt;
        let ts = self
            .rt
            .block_on(self.client.current_timestamp())
            .map_err(map_tikv_err)?;
        Ok(ts.version())
    }
}

struct TikvTransaction {
    inner: Option<TikvTxn>,
    client: TransactionClient,
    rt: Arc<tokio::runtime::Runtime>,
    pessimistic: bool,
    /// When set (RC after `refresh_snapshot`), plain get/scan read at this TS.
    read_snapshot: Option<Snapshot>,
    /// Own writes so RC snapshot reads still see uncommitted mutations.
    writes: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl TikvTransaction {
    fn take_txn(&mut self) -> Result<TikvTxn> {
        self.inner.take().ok_or_else(finished_txn)
    }

    fn put_txn(&mut self, txn: TikvTxn) {
        self.inner = Some(txn);
    }

    fn overlay_get(&self, key: &[u8]) -> Option<Option<Vec<u8>>> {
        self.writes.get(key).cloned()
    }

    fn scan_pairs(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if end <= start {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut cursor = start.to_vec();
        loop {
            let range_end = end.to_vec();
            let range_start = cursor.clone();
            let batch: Vec<(Vec<u8>, Vec<u8>)> = if let Some(snap) = self.read_snapshot.as_mut() {
                let pairs = self.rt.block_on(snap.scan(
                    Key::from(range_start)..Key::from(range_end),
                    SCAN_LIMIT,
                ));
                pairs.map_err(map_tikv_err)?.map(pair_owned).collect()
            } else {
                let mut txn = self.take_txn()?;
                let pairs = self
                    .rt
                    .block_on(txn.scan(Key::from(range_start)..Key::from(range_end), SCAN_LIMIT));
                self.put_txn(txn);
                pairs.map_err(map_tikv_err)?.map(pair_owned).collect()
            };
            let n = batch.len();
            for pair in batch {
                out.push(pair);
            }
            if n < SCAN_LIMIT as usize {
                break;
            }
            let Some((last, _)) = out.last() else {
                break;
            };
            cursor = successor(last);
            if cursor.as_slice() >= end {
                break;
            }
        }
        // Apply local overlay within [start, end): drop scanned keys we
        // mutated, then insert non-deleted overlay entries.
        out.retain(|(k, _)| !self.writes.contains_key(k));
        for (key, value) in &self.writes {
            if key.as_slice() < start || key.as_slice() >= end {
                continue;
            }
            if let Some(v) = value {
                out.push((key.clone(), v.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

impl Transaction for TikvTransaction {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(overlay) = self.overlay_get(key) {
            return Ok(overlay);
        }
        let key_owned = key.to_vec();
        if let Some(snap) = self.read_snapshot.as_mut() {
            let out = self.rt.block_on(snap.get(key_owned));
            return out.map_err(map_tikv_err);
        }
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.get(key_owned));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn batch_get(&mut self, keys: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(keys.len());
        let mut miss = Vec::new();
        for key in keys {
            match self.overlay_get(key) {
                Some(Some(value)) => out.push((key.clone(), value)),
                Some(None) => {}
                None => miss.push(key.clone()),
            }
        }
        if miss.is_empty() {
            return Ok(out);
        }
        let fetched: Vec<(Vec<u8>, Vec<u8>)> = if let Some(snap) = self.read_snapshot.as_mut() {
            let pairs = self.rt.block_on(snap.batch_get(miss));
            pairs
                .map_err(map_tikv_err)?
                .map(pair_owned)
                .collect()
        } else {
            let mut txn = self.take_txn()?;
            let pairs = self.rt.block_on(txn.batch_get(miss));
            self.put_txn(txn);
            pairs
                .map_err(map_tikv_err)?
                .map(pair_owned)
                .collect()
        };
        out.extend(fetched);
        Ok(out)
    }

    fn batch_get_for_update(&mut self, keys: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        // Overlay hits still need a lock on the live key.
        let mut out = Vec::with_capacity(keys.len());
        let mut miss = Vec::new();
        for key in keys {
            match self.overlay_get(key) {
                Some(Some(value)) => {
                    let key_owned = key.clone();
                    let mut txn = self.take_txn()?;
                    let _ = self.rt.block_on(txn.get_for_update(key_owned));
                    self.put_txn(txn);
                    out.push((key.clone(), value));
                }
                Some(None) => {
                    let key_owned = key.clone();
                    let mut txn = self.take_txn()?;
                    let _ = self.rt.block_on(txn.get_for_update(key_owned));
                    self.put_txn(txn);
                }
                None => miss.push(key.clone()),
            }
        }
        if !miss.is_empty() {
            let mut txn = self.take_txn()?;
            let pairs = self.rt.block_on(txn.batch_get_for_update(miss));
            self.put_txn(txn);
            out.extend(
                pairs
                    .map_err(map_tikv_err)?
                    .into_iter()
                    .map(pair_owned),
            );
        }
        Ok(out)
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let key = key.to_vec();
        let value = value.to_vec();
        self.writes.insert(key.clone(), Some(value.clone()));
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.put(key, value));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        let key = key.to_vec();
        self.writes.insert(key.clone(), None);
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.delete(key));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_pairs(start, end)
    }

    fn get_for_update(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(overlay) = self.overlay_get(key) {
            // Still take the pessimistic lock on the live key.
            let key_owned = key.to_vec();
            let mut txn = self.take_txn()?;
            let _ = self.rt.block_on(txn.get_for_update(key_owned));
            self.put_txn(txn);
            return Ok(overlay);
        }
        let key = key.to_vec();
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.get_for_update(key));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn scan_for_update(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let pairs = self.scan_pairs(start, end)?;
        let mut locked = Vec::with_capacity(pairs.len());
        for (key, _) in pairs {
            if let Some(value) = self.get_for_update(&key)? {
                locked.push((key, value));
            }
        }
        Ok(locked)
    }

    fn set_pessimistic(&mut self) {
        self.pessimistic = true;
    }

    fn is_pessimistic(&self) -> bool {
        self.pessimistic
    }

    fn commit(&mut self) -> Result<()> {
        let mut txn = self.inner.take().ok_or_else(finished_txn)?;
        self.read_snapshot = None;
        self.writes.clear();
        self.rt
            .block_on(txn.commit())
            .map(|_| ())
            .map_err(map_tikv_err)
    }

    fn rollback(&mut self) -> Result<()> {
        self.read_snapshot = None;
        self.writes.clear();
        let Some(mut txn) = self.inner.take() else {
            return Ok(());
        };
        self.rt.block_on(txn.rollback()).map_err(map_tikv_err)
    }

    fn refresh_snapshot(&mut self) -> Result<()> {
        // RC statement snapshot: new PD TSO for subsequent plain reads.
        // Writes stay in `writes` + the underlying txn buffer.
        let ts = self
            .rt
            .block_on(self.client.current_timestamp())
            .map_err(map_tikv_err)?;
        self.read_snapshot = Some(
            self.client
                .snapshot(ts, TransactionOptions::default().read_only()),
        );
        Ok(())
    }
}

impl Drop for TikvTransaction {
    fn drop(&mut self) {
        self.read_snapshot = None;
        self.writes.clear();
        if let Some(mut txn) = self.inner.take() {
            let _ = self.rt.block_on(txn.rollback());
        }
    }
}

fn pair_owned(pair: KvPair) -> (Vec<u8>, Vec<u8>) {
    let key = pair.key().clone();
    let value = pair.value().clone();
    (Vec::<u8>::from(key), value)
}

fn successor(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn finished_txn() -> Error {
    Error::new(ERR_UNKNOWN_ERROR, "transaction already finished")
}

fn map_tikv_err(err: tikv_client::Error) -> Error {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("writeconflict") || lower.contains("write conflict") {
        Error::new(ERR_WRITE_CONFLICT, msg)
    } else if lower.contains("deadlock") {
        Error::new(ERR_LOCK_DEADLOCK, msg)
    } else if lower.contains("nowait") {
        Error::new(ERR_LOCK_NOWAIT, msg)
    } else if lower.contains("lock") && (lower.contains("timeout") || lower.contains("wait")) {
        Error::new(ERR_LOCK_WAIT_TIMEOUT, msg)
    } else {
        Error::new(ERR_UNKNOWN_ERROR, format!("tikv: {msg}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tikv_rc_refresh_snapshot_if_available() {
        let store = match TikvStorage::connect(&["127.0.0.1:2379".to_string()]) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("skip live tikv rc refresh: {err}");
                return;
            }
        };
        let key = format!(
            "sealdb:test:rc:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
        .into_bytes();

        let mut setup = store.begin().unwrap();
        setup.set(&key, b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = store.begin().unwrap();
        a.refresh_snapshot().unwrap();
        assert_eq!(a.get(&key).unwrap(), Some(b"v1".to_vec()));

        let mut b = store.begin().unwrap();
        b.set(&key, b"v2").unwrap();
        b.commit().unwrap();

        // Without refresh, RR-style txn start_ts would still see v1; after
        // refresh, RC reads at a new PD TSO and must observe v2.
        a.refresh_snapshot().unwrap();
        assert_eq!(a.get(&key).unwrap(), Some(b"v2".to_vec()));
        a.rollback().unwrap();

        let mut cleanup = store.begin().unwrap();
        cleanup.delete(&key).unwrap();
        cleanup.commit().unwrap();
    }
}

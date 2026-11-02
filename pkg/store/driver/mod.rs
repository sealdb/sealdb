/// TiKV driver (TiDB `pkg/store/driver`). Planner/Executor must not
/// import `tikv-client` types; they talk to `sealdb_kv::Storage` only.
/// Completeness wave: coprocessor, async/pipelined commit, stale read, GC,
/// Region cache. Pessimistic begin + `get_for_update` is leftover-closed.
use sealdb_errno::{
    ERR_LOCK_DEADLOCK, ERR_LOCK_NOWAIT, ERR_LOCK_WAIT_TIMEOUT, ERR_UNKNOWN_ERROR,
    ERR_WRITE_CONFLICT, Error, Result,
};
use sealdb_kv::{Storage, Transaction};
use std::sync::Arc;
use std::time::Duration;
use tikv_client::{Key, KvPair, Transaction as TikvTxn, TransactionClient};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const SCAN_LIMIT: u32 = 1024;

pub struct TikvStorage {
    client: TransactionClient,
    rt: Arc<tokio::runtime::Runtime>,
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
        })
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
            rt: Arc::clone(&self.rt),
            pessimistic,
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
}

struct TikvTransaction {
    inner: Option<TikvTxn>,
    rt: Arc<tokio::runtime::Runtime>,
    pessimistic: bool,
}

impl TikvTransaction {
    fn take_txn(&mut self) -> Result<TikvTxn> {
        self.inner.take().ok_or_else(finished_txn)
    }

    fn put_txn(&mut self, txn: TikvTxn) {
        self.inner = Some(txn);
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
            let mut txn = self.take_txn()?;
            let pairs = self
                .rt
                .block_on(txn.scan(Key::from(range_start)..Key::from(range_end), SCAN_LIMIT));
            self.put_txn(txn);
            let batch: Vec<(Vec<u8>, Vec<u8>)> =
                pairs.map_err(map_tikv_err)?.map(pair_owned).collect();
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
        Ok(out)
    }
}

impl Transaction for TikvTransaction {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let key = key.to_vec();
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.get(key));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let key = key.to_vec();
        let value = value.to_vec();
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.put(key, value));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        let key = key.to_vec();
        let mut txn = self.take_txn()?;
        let out = self.rt.block_on(txn.delete(key));
        self.put_txn(txn);
        out.map_err(map_tikv_err)
    }

    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_pairs(start, end)
    }

    fn get_for_update(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
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
        self.rt
            .block_on(txn.commit())
            .map(|_| ())
            .map_err(map_tikv_err)
    }

    fn rollback(&mut self) -> Result<()> {
        let Some(mut txn) = self.inner.take() else {
            return Ok(());
        };
        self.rt.block_on(txn.rollback()).map_err(map_tikv_err)
    }

    fn refresh_snapshot(&mut self) -> Result<()> {
        // Completeness wave: TiKV READ COMMITTED statement snapshot (new
        // start_ts without dropping buffered writes).
        Ok(())
    }
}

impl Drop for TikvTransaction {
    fn drop(&mut self) {
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

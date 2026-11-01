use super::kv::{Storage, Transaction};
use sealdb_errno::{ERR_WRITE_CONFLICT, Error, Result};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct VersionedValue {
    commit_ts: u64,
    value: Option<Vec<u8>>,
}

#[derive(Default)]
struct Store {
    next_ts: u64,
    data: BTreeMap<Vec<u8>, VersionedValue>,
}

#[derive(Clone, Default)]
pub struct MemStorage {
    store: Arc<Mutex<Store>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemStorage {
    fn begin(&self) -> Result<Box<dyn Transaction>> {
        let mut store = self.store.lock().expect("mem kv");
        store.next_ts += 1;
        let start_ts = store.next_ts;
        Ok(Box::new(MemTransaction {
            store: Arc::clone(&self.store),
            start_ts,
            snapshot: store.data.clone(),
            writes: BTreeMap::new(),
            deletes: HashSet::new(),
        }))
    }
}

/// Optimistic Repeatable Read transaction.
/// Completeness wave / not this feature: pessimistic locks (F13), 2PC/TiKV TSO,
/// stale read, async commit, pipelined DML.
struct MemTransaction {
    store: Arc<Mutex<Store>>,
    start_ts: u64,
    snapshot: BTreeMap<Vec<u8>, VersionedValue>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: HashSet<Vec<u8>>,
}

impl Transaction for MemTransaction {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.deletes.contains(key) {
            return Ok(None);
        }
        if let Some(v) = self.writes.get(key) {
            return Ok(Some(v.clone()));
        }
        Ok(self.snapshot.get(key).and_then(|v| v.value.clone()))
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.deletes.remove(key);
        self.writes.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.writes.remove(key);
        self.deletes.insert(key.to_vec());
        Ok(())
    }

    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for (k, v) in self.snapshot.range(start.to_vec()..end.to_vec()) {
            if self.deletes.contains(k) {
                continue;
            }
            let value = self.writes.get(k).cloned().or_else(|| v.value.clone());
            if let Some(value) = value {
                out.push((k.clone(), value));
            }
        }
        for (k, v) in &self.writes {
            if k.as_slice() >= start && k.as_slice() < end && !out.iter().any(|(ek, _)| ek == k) {
                out.push((k.clone(), v.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn commit(&mut self) -> Result<()> {
        let mut store = self.store.lock().expect("mem kv");
        for key in self.writes.keys().chain(self.deletes.iter()) {
            if let Some(current) = store.data.get(key)
                && current.commit_ts > self.start_ts
            {
                return Err(Error::new(
                    ERR_WRITE_CONFLICT,
                    format!(
                        "Write conflict, txnStartTS={}, conflictCommitTS={}",
                        self.start_ts, current.commit_ts
                    ),
                ));
            }
        }
        store.next_ts += 1;
        let commit_ts = store.next_ts;
        for k in std::mem::take(&mut self.deletes) {
            store.data.insert(
                k,
                VersionedValue {
                    commit_ts,
                    value: None,
                },
            );
        }
        for (k, v) in std::mem::take(&mut self.writes) {
            store.data.insert(
                k,
                VersionedValue {
                    commit_ts,
                    value: Some(v),
                },
            );
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.writes.clear();
        self.deletes.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimistic_rr_snapshot_and_conflict() {
        let storage = MemStorage::new();
        let mut setup = storage.begin().unwrap();
        setup.set(b"k", b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = storage.begin().unwrap();
        assert_eq!(a.get(b"k").unwrap(), Some(b"v1".to_vec()));

        let mut b = storage.begin().unwrap();
        b.set(b"k", b"v2").unwrap();
        b.commit().unwrap();

        assert_eq!(a.get(b"k").unwrap(), Some(b"v1".to_vec()));
        a.set(b"k", b"v3").unwrap();
        let err = a.commit().unwrap_err();
        assert_eq!(err.code, ERR_WRITE_CONFLICT);
    }
}

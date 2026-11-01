use super::kv::{Storage, Transaction};
use sealdb_errno::{ERR_LOCK_WAIT_TIMEOUT, ERR_WRITE_CONFLICT, Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    locks: HashMap<Vec<u8>, u64>,
}

#[derive(Clone, Default)]
pub struct MemStorage {
    store: Arc<Mutex<Store>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin_txn(&self, pessimistic: bool) -> Result<Box<dyn Transaction>> {
        let mut store = self.store.lock().expect("mem kv");
        store.next_ts += 1;
        let start_ts = store.next_ts;
        Ok(Box::new(MemTransaction {
            store: Arc::clone(&self.store),
            start_ts,
            snapshot: store.data.clone(),
            writes: BTreeMap::new(),
            deletes: HashSet::new(),
            locked: HashSet::new(),
            pessimistic,
        }))
    }
}

impl Storage for MemStorage {
    fn begin(&self) -> Result<Box<dyn Transaction>> {
        self.begin_txn(false)
    }

    fn begin_pessimistic(&self) -> Result<Box<dyn Transaction>> {
        self.begin_txn(true)
    }
}

/// In-memory optimistic/pessimistic Repeatable Read.
/// Completeness wave / not this feature: blocking lock wait, deadlock
/// detection, NOWAIT/SKIP LOCKED, 2PC/TiKV TSO, stale read.
struct MemTransaction {
    store: Arc<Mutex<Store>>,
    start_ts: u64,
    snapshot: BTreeMap<Vec<u8>, VersionedValue>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: HashSet<Vec<u8>>,
    locked: HashSet<Vec<u8>>,
    pessimistic: bool,
}

impl MemTransaction {
    fn latest_value(
        store: &Store,
        writes: &BTreeMap<Vec<u8>, Vec<u8>>,
        deletes: &HashSet<Vec<u8>>,
        key: &[u8],
    ) -> Option<Vec<u8>> {
        if deletes.contains(key) {
            return None;
        }
        if let Some(v) = writes.get(key) {
            return Some(v.clone());
        }
        store.data.get(key).and_then(|v| v.value.clone())
    }

    fn acquire(
        store: &mut Store,
        locked: &mut HashSet<Vec<u8>>,
        start_ts: u64,
        key: &[u8],
    ) -> Result<()> {
        if let Some(owner) = store.locks.get(key)
            && *owner != start_ts
        {
            return Err(Error::new(
                ERR_LOCK_WAIT_TIMEOUT,
                "Lock wait timeout exceeded; try restarting transaction",
            ));
        }
        store.locks.insert(key.to_vec(), start_ts);
        locked.insert(key.to_vec());
        Ok(())
    }

    fn release_locks(&mut self) {
        if self.locked.is_empty() {
            return;
        }
        let mut store = self.store.lock().expect("mem kv");
        for key in self.locked.drain() {
            if store.locks.get(&key) == Some(&self.start_ts) {
                store.locks.remove(&key);
            }
        }
    }
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
        if self.pessimistic {
            let mut store = self.store.lock().expect("mem kv");
            Self::acquire(&mut store, &mut self.locked, self.start_ts, key)?;
        }
        self.deletes.remove(key);
        self.writes.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        if self.pessimistic {
            let mut store = self.store.lock().expect("mem kv");
            Self::acquire(&mut store, &mut self.locked, self.start_ts, key)?;
        }
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

    fn get_for_update(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut store = self.store.lock().expect("mem kv");
        Self::acquire(&mut store, &mut self.locked, self.start_ts, key)?;
        Ok(Self::latest_value(&store, &self.writes, &self.deletes, key))
    }

    fn scan_for_update(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut store = self.store.lock().expect("mem kv");
        let mut keys = Vec::new();
        for (k, v) in store.data.range(start.to_vec()..end.to_vec()) {
            if self.deletes.contains(k) {
                continue;
            }
            if self.writes.contains_key(k) || v.value.is_some() {
                keys.push(k.clone());
            }
        }
        for k in self.writes.keys() {
            if k.as_slice() >= start && k.as_slice() < end && !keys.iter().any(|ek| ek == k) {
                keys.push(k.clone());
            }
        }
        let mut out = Vec::new();
        for key in keys {
            Self::acquire(&mut store, &mut self.locked, self.start_ts, &key)?;
            if let Some(value) = Self::latest_value(&store, &self.writes, &self.deletes, &key) {
                out.push((key, value));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn set_pessimistic(&mut self) {
        self.pessimistic = true;
    }

    fn is_pessimistic(&self) -> bool {
        self.pessimistic
    }

    fn commit(&mut self) -> Result<()> {
        let mut store = self.store.lock().expect("mem kv");
        for key in self.writes.keys().chain(self.deletes.iter()) {
            if self.locked.contains(key) {
                continue;
            }
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
        for key in self.locked.drain() {
            if store.locks.get(&key) == Some(&self.start_ts) {
                store.locks.remove(&key);
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.writes.clear();
        self.deletes.clear();
        self.release_locks();
        Ok(())
    }

    fn refresh_snapshot(&mut self) -> Result<()> {
        let store = self.store.lock().expect("mem kv");
        self.snapshot = store.data.clone();
        Ok(())
    }
}

impl Drop for MemTransaction {
    fn drop(&mut self) {
        self.release_locks();
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

    #[test]
    fn test_pessimistic_for_update_lock() {
        let storage = MemStorage::new();
        let mut setup = storage.begin().unwrap();
        setup.set(b"k", b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = storage.begin_pessimistic().unwrap();
        assert_eq!(a.get_for_update(b"k").unwrap(), Some(b"v1".to_vec()));

        let mut b = storage.begin().unwrap();
        let err = b.get_for_update(b"k").unwrap_err();
        assert_eq!(err.code, ERR_LOCK_WAIT_TIMEOUT);

        a.commit().unwrap();
        assert_eq!(b.get_for_update(b"k").unwrap(), Some(b"v1".to_vec()));
    }
}

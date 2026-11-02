use super::kv::{LockWait, RowLock, Storage, Transaction};
use sealdb_errno::{
    ERR_LOCK_NOWAIT, ERR_LOCK_WAIT_TIMEOUT, ERR_UNKNOWN_FUNCTION, ERR_WRITE_CONFLICT, Error, Result,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
struct VersionedValue {
    commit_ts: u64,
    value: Option<Vec<u8>>,
}

#[derive(Default)]
struct KeyLock {
    exclusive: Option<u64>,
    shared: HashSet<u64>,
}

impl KeyLock {
    fn is_empty(&self) -> bool {
        self.exclusive.is_none() && self.shared.is_empty()
    }
}

#[derive(Default)]
struct Store {
    next_ts: u64,
    data: BTreeMap<Vec<u8>, VersionedValue>,
    locks: HashMap<Vec<u8>, KeyLock>,
}

#[derive(Clone)]
pub struct MemStorage {
    store: Arc<Mutex<Store>>,
    wait: Arc<Condvar>,
}

impl Default for MemStorage {
    fn default() -> Self {
        Self {
            store: Arc::new(Mutex::new(Store::default())),
            wait: Arc::new(Condvar::new()),
        }
    }
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
            wait: Arc::clone(&self.wait),
            start_ts,
            snapshot: store.data.clone(),
            writes: BTreeMap::new(),
            deletes: HashSet::new(),
            locked: HashSet::new(),
            savepoints: Vec::new(),
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

/// Buffered write state captured by `SAVEPOINT`.
struct Savepoint {
    name: String,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: HashSet<Vec<u8>>,
    locked: HashSet<Vec<u8>>,
}

/// In-memory optimistic/pessimistic Repeatable Read.
/// Completeness wave / not this feature: deadlock detection, 2PC/TiKV TSO,
/// stale read. `SEALDB_LOCK_WAIT_MS` (default 50) caps blocking wait.
struct MemTransaction {
    store: Arc<Mutex<Store>>,
    wait: Arc<Condvar>,
    start_ts: u64,
    snapshot: BTreeMap<Vec<u8>, VersionedValue>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: HashSet<Vec<u8>>,
    locked: HashSet<Vec<u8>>,
    savepoints: Vec<Savepoint>,
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

    fn lock_wait_timeout() -> Duration {
        let ms = std::env::var("SEALDB_LOCK_WAIT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);
        Duration::from_millis(ms)
    }

    fn conflicts(store: &Store, start_ts: u64, key: &[u8], exclusive: bool) -> bool {
        let Some(entry) = store.locks.get(key) else {
            return false;
        };
        if let Some(owner) = entry.exclusive
            && owner != start_ts
        {
            return true;
        }
        exclusive && entry.shared.iter().any(|owner| *owner != start_ts)
    }

    fn take(
        store: &mut Store,
        locked: &mut HashSet<Vec<u8>>,
        start_ts: u64,
        key: &[u8],
        exclusive: bool,
    ) {
        let entry = store.locks.entry(key.to_vec()).or_default();
        if exclusive {
            entry.shared.remove(&start_ts);
            entry.exclusive = Some(start_ts);
        } else if entry.exclusive.is_none() {
            entry.shared.insert(start_ts);
        }
        locked.insert(key.to_vec());
    }

    /// Exclusive lock for `FOR UPDATE` / pessimistic writes; shared for
    /// `FOR SHARE`. `LockWait::Block` waits until the holder releases or
    /// `SEALDB_LOCK_WAIT_MS` elapses (errno 1205).
    fn lock_key(&mut self, key: &[u8], exclusive: bool, wait: LockWait) -> Result<bool> {
        let store = Arc::clone(&self.store);
        let cond = Arc::clone(&self.wait);
        let start_ts = self.start_ts;
        let mut guard = store.lock().expect("mem kv");
        let deadline = Instant::now() + Self::lock_wait_timeout();
        loop {
            if !Self::conflicts(&guard, start_ts, key, exclusive) {
                Self::take(&mut guard, &mut self.locked, start_ts, key, exclusive);
                return Ok(true);
            }
            match wait {
                LockWait::SkipLocked => return Ok(false),
                LockWait::Nowait => {
                    return Err(Error::new(
                        ERR_LOCK_NOWAIT,
                        "Statement aborted because lock(s) could not be acquired immediately and NOWAIT is set.",
                    ));
                }
                LockWait::Block => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(Error::new(
                            ERR_LOCK_WAIT_TIMEOUT,
                            "Lock wait timeout exceeded; try restarting transaction",
                        ));
                    }
                    let (g, timed) = cond
                        .wait_timeout(guard, deadline.saturating_duration_since(now))
                        .expect("mem kv wait");
                    guard = g;
                    if timed.timed_out() {
                        return Err(Error::new(
                            ERR_LOCK_WAIT_TIMEOUT,
                            "Lock wait timeout exceeded; try restarting transaction",
                        ));
                    }
                }
            }
        }
    }

    fn notify_waiters(&self) {
        self.wait.notify_all();
    }

    fn release_one(store: &mut Store, start_ts: u64, key: &[u8]) {
        let Some(entry) = store.locks.get_mut(key) else {
            return;
        };
        if entry.exclusive == Some(start_ts) {
            entry.exclusive = None;
        }
        entry.shared.remove(&start_ts);
        if entry.is_empty() {
            store.locks.remove(key);
        }
    }

    fn release_locks(&mut self) {
        if self.locked.is_empty() {
            return;
        }
        {
            let mut store = self.store.lock().expect("mem kv");
            for key in self.locked.drain() {
                Self::release_one(&mut store, self.start_ts, &key);
            }
        }
        self.notify_waiters();
    }

    fn scan_with_lock(
        &mut self,
        start: &[u8],
        end: &[u8],
        exclusive: bool,
        wait: LockWait,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut keys = {
            let store = self.store.lock().expect("mem kv");
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
            keys
        };
        let mut out = Vec::new();
        for key in keys.drain(..) {
            if !self.lock_key(&key, exclusive, wait)? {
                continue;
            }
            let store = self.store.lock().expect("mem kv");
            if let Some(value) = Self::latest_value(&store, &self.writes, &self.deletes, &key) {
                out.push((key, value));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
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
            self.lock_key(key, true, LockWait::Block)?;
        }
        self.deletes.remove(key);
        self.writes.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        if self.pessimistic {
            self.lock_key(key, true, LockWait::Block)?;
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
        self.get_locked(key, RowLock::Exclusive, LockWait::Block)
    }

    fn scan_for_update(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_locked(start, end, RowLock::Exclusive, LockWait::Block)
    }

    fn get_for_share(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_locked(key, RowLock::Share, LockWait::Block)
    }

    fn scan_for_share(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_locked(start, end, RowLock::Share, LockWait::Block)
    }

    fn get_locked(&mut self, key: &[u8], lock: RowLock, wait: LockWait) -> Result<Option<Vec<u8>>> {
        if lock == RowLock::None {
            return self.get(key);
        }
        if !self.lock_key(key, lock == RowLock::Exclusive, wait)? {
            return Ok(None);
        }
        let store = self.store.lock().expect("mem kv");
        Ok(Self::latest_value(&store, &self.writes, &self.deletes, key))
    }

    fn scan_locked(
        &mut self,
        start: &[u8],
        end: &[u8],
        lock: RowLock,
        wait: LockWait,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        match lock {
            RowLock::None => self.scan(start, end),
            RowLock::Exclusive => self.scan_with_lock(start, end, true, wait),
            RowLock::Share => self.scan_with_lock(start, end, false, wait),
        }
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
            Self::release_one(&mut store, self.start_ts, &key);
        }
        drop(store);
        self.notify_waiters();
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.writes.clear();
        self.deletes.clear();
        self.savepoints.clear();
        self.release_locks();
        Ok(())
    }

    /// Snapshot current writes/deletes/locks under `name` (replacing a prior
    /// marker of the same name, matching MySQL SAVEPOINT semantics).
    fn savepoint(&mut self, name: &str) -> Result<()> {
        self.savepoints
            .retain(|sp| !sp.name.eq_ignore_ascii_case(name));
        self.savepoints.push(Savepoint {
            name: name.to_string(),
            writes: self.writes.clone(),
            deletes: self.deletes.clone(),
            locked: self.locked.clone(),
        });
        Ok(())
    }

    /// Restore the write set at `name`, release locks taken after it, and
    /// keep the named marker so ROLLBACK TO can be repeated.
    fn rollback_to(&mut self, name: &str) -> Result<()> {
        let idx = self
            .savepoints
            .iter()
            .rposition(|sp| sp.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                Error::new(
                    ERR_UNKNOWN_FUNCTION,
                    format!("SAVEPOINT {name} does not exist"),
                )
            })?;
        let sp = &self.savepoints[idx];
        self.writes = sp.writes.clone();
        self.deletes = sp.deletes.clone();
        let keep = sp.locked.clone();
        let extra: Vec<_> = self.locked.difference(&keep).cloned().collect();
        {
            let mut store = self.store.lock().expect("mem kv");
            for key in extra {
                Self::release_one(&mut store, self.start_ts, &key);
                self.locked.remove(&key);
            }
        }
        self.notify_waiters();
        self.savepoints.truncate(idx + 1);
        Ok(())
    }

    /// Drop `name` and later markers. Buffered writes are left unchanged.
    fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let idx = self
            .savepoints
            .iter()
            .rposition(|sp| sp.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                Error::new(
                    ERR_UNKNOWN_FUNCTION,
                    format!("SAVEPOINT {name} does not exist"),
                )
            })?;
        self.savepoints.truncate(idx);
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

    #[test]
    fn test_for_share_compatible_and_conflicts_for_update() {
        let storage = MemStorage::new();
        let mut setup = storage.begin().unwrap();
        setup.set(b"k", b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = storage.begin_pessimistic().unwrap();
        assert_eq!(a.get_for_share(b"k").unwrap(), Some(b"v1".to_vec()));

        let mut b = storage.begin_pessimistic().unwrap();
        assert_eq!(b.get_for_share(b"k").unwrap(), Some(b"v1".to_vec()));

        let mut c = storage.begin().unwrap();
        let err = c.get_for_update(b"k").unwrap_err();
        assert_eq!(err.code, ERR_LOCK_WAIT_TIMEOUT);

        a.commit().unwrap();
        b.commit().unwrap();
        assert_eq!(c.get_for_update(b"k").unwrap(), Some(b"v1".to_vec()));
    }

    #[test]
    fn test_nowait_and_skip_locked() {
        let storage = MemStorage::new();
        let mut setup = storage.begin().unwrap();
        setup.set(b"k", b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = storage.begin_pessimistic().unwrap();
        assert_eq!(a.get_for_update(b"k").unwrap(), Some(b"v1".to_vec()));

        let mut b = storage.begin().unwrap();
        let err = b
            .get_locked(b"k", RowLock::Exclusive, LockWait::Nowait)
            .unwrap_err();
        assert_eq!(err.code, ERR_LOCK_NOWAIT);
        assert_eq!(
            b.get_locked(b"k", RowLock::Exclusive, LockWait::SkipLocked)
                .unwrap(),
            None
        );
        a.commit().unwrap();
        assert_eq!(
            b.get_locked(b"k", RowLock::Exclusive, LockWait::Nowait)
                .unwrap(),
            Some(b"v1".to_vec())
        );
    }

    #[test]
    fn test_blocking_wait_wakes_on_commit() {
        let storage = MemStorage::new();
        let mut setup = storage.begin().unwrap();
        setup.set(b"k", b"v1").unwrap();
        setup.commit().unwrap();

        let mut a = storage.begin_pessimistic().unwrap();
        assert_eq!(a.get_for_update(b"k").unwrap(), Some(b"v1".to_vec()));
        let storage_b = storage.clone();
        let waiter = std::thread::spawn(move || {
            let mut b = storage_b.begin().unwrap();
            let value = b.get_for_update(b"k").unwrap();
            b.commit().unwrap();
            value
        });
        std::thread::sleep(Duration::from_millis(10));
        a.commit().unwrap();
        assert_eq!(waiter.join().expect("waiter"), Some(b"v1".to_vec()));
    }

    #[test]
    fn test_savepoint_rollback_to() {
        let storage = MemStorage::new();
        let mut txn = storage.begin().unwrap();
        txn.set(b"k", b"v1").unwrap();
        txn.savepoint("sp1").unwrap();
        txn.set(b"k", b"v2").unwrap();
        txn.set(b"k2", b"x").unwrap();
        txn.rollback_to("sp1").unwrap();
        assert_eq!(txn.get(b"k").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(txn.get(b"k2").unwrap(), None);
        txn.commit().unwrap();
        let mut read = storage.begin().unwrap();
        assert_eq!(read.get(b"k").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(read.get(b"k2").unwrap(), None);
    }
}

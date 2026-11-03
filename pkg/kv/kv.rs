use sealdb_errno::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Version(pub u64);

/// Row lock taken by `SELECT FOR UPDATE` / `FOR SHARE` and pessimistic writes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RowLock {
    #[default]
    None,
    Share,
    Exclusive,
}

/// Wait policy for a row lock. `Block` currently returns 1205 immediately
/// (blocking wait is a later leftover). `SkipLocked` omits the row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LockWait {
    #[default]
    Block,
    Nowait,
    SkipLocked,
}

pub trait Storage: Send + Sync {
    fn begin(&self) -> Result<Box<dyn Transaction>>;
    fn begin_pessimistic(&self) -> Result<Box<dyn Transaction>>;
    /// Logical timestamp for snapshot / RC refresh. TiKV uses PD TSO;
    /// MemStorage returns a monotonic local counter.
    fn current_timestamp(&self) -> Result<u64> {
        Err(Error::unsupported("current_timestamp"))
    }
}

pub trait Transaction: Send {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, key: &[u8]) -> Result<()>;
    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    /// Batch get. Default loops `get`; TiKV overrides with `batch_get` RPC.
    /// Missing keys are omitted from the result (TiKV semantics).
    fn batch_get(&mut self, keys: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(value) = self.get(key)? {
                out.push((key.clone(), value));
            }
        }
        Ok(out)
    }
    fn get_for_update(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn scan_for_update(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    /// Batch pessimistic lock+get. Default loops `get_for_update`.
    fn batch_get_for_update(&mut self, keys: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(value) = self.get_for_update(key)? {
                out.push((key.clone(), value));
            }
        }
        Ok(out)
    }
    /// Shared lock (`FOR SHARE`). Default is exclusive; MemStorage overrides.
    fn get_for_share(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_for_update(key)
    }
    fn scan_for_share(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_for_update(start, end)
    }
    fn get_locked(&mut self, key: &[u8], lock: RowLock, wait: LockWait) -> Result<Option<Vec<u8>>> {
        let _ = wait;
        match lock {
            RowLock::Exclusive => self.get_for_update(key),
            RowLock::Share => self.get_for_share(key),
            RowLock::None => self.get(key),
        }
    }
    fn scan_locked(
        &mut self,
        start: &[u8],
        end: &[u8],
        lock: RowLock,
        wait: LockWait,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let _ = wait;
        match lock {
            RowLock::Exclusive => self.scan_for_update(start, end),
            RowLock::Share => self.scan_for_share(start, end),
            RowLock::None => self.scan(start, end),
        }
    }
    fn set_pessimistic(&mut self);
    fn is_pessimistic(&self) -> bool;
    fn commit(&mut self) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
    fn refresh_snapshot(&mut self) -> Result<()> {
        Ok(())
    }
    /// Record a named marker of the current buffered writes/deletes/locks.
    /// A later `SAVEPOINT` with the same name replaces the earlier one.
    fn savepoint(&mut self, _name: &str) -> Result<()> {
        Err(Error::unsupported("SAVEPOINT"))
    }
    /// Restore buffered writes to `name` and drop later savepoints. The
    /// named marker itself remains. Unknown names are errno 1305.
    fn rollback_to(&mut self, _name: &str) -> Result<()> {
        Err(Error::unsupported("ROLLBACK TO SAVEPOINT"))
    }
    /// Drop `name` and every savepoint created after it, without restoring
    /// writes. Unknown names are errno 1305.
    fn release_savepoint(&mut self, _name: &str) -> Result<()> {
        Err(Error::unsupported("RELEASE SAVEPOINT"))
    }
}

pub fn unsupported_storage() -> Error {
    Error::unsupported("kv storage")
}

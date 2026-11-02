use sealdb_errno::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Version(pub u64);

pub trait Storage: Send + Sync {
    fn begin(&self) -> Result<Box<dyn Transaction>>;
    fn begin_pessimistic(&self) -> Result<Box<dyn Transaction>>;
}

pub trait Transaction: Send {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, key: &[u8]) -> Result<()>;
    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn get_for_update(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn scan_for_update(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
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

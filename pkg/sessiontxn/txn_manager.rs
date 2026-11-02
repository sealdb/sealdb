use sealdb_errno::Result;
use sealdb_kv::{IsolationLevel, Storage, TxnHandle};
use std::sync::Arc;

/// Session-owned transaction lifecycle for F12/F13/C3.
/// Completeness wave / not this feature: AND CHAIN, XA, transactional DDL,
/// blocking lock wait.
pub struct TxnManager {
    handle: Option<Arc<TxnHandle>>,
    explicit: bool,
    schema_ver: i64,
    isolation: IsolationLevel,
}

impl TxnManager {
    pub fn new() -> Self {
        Self {
            handle: None,
            explicit: false,
            schema_ver: 0,
            isolation: IsolationLevel::RepeatableRead,
        }
    }

    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    pub fn schema_ver(&self) -> i64 {
        self.schema_ver
    }

    pub fn isolation(&self) -> IsolationLevel {
        self.isolation
    }

    pub fn enter_begin(
        &mut self,
        storage: &dyn Storage,
        schema_ver: i64,
        isolation: IsolationLevel,
    ) -> Result<()> {
        self.enter(storage, false, schema_ver, isolation)
    }

    pub fn enter_begin_pessimistic(
        &mut self,
        storage: &dyn Storage,
        schema_ver: i64,
        isolation: IsolationLevel,
    ) -> Result<()> {
        self.enter(storage, true, schema_ver, isolation)
    }

    fn enter(
        &mut self,
        storage: &dyn Storage,
        pessimistic: bool,
        schema_ver: i64,
        isolation: IsolationLevel,
    ) -> Result<()> {
        if self.handle.is_some() {
            self.commit()?;
        }
        let txn = if pessimistic {
            storage.begin_pessimistic()?
        } else {
            storage.begin()?
        };
        self.handle = Some(Arc::new(TxnHandle::new(txn)));
        self.explicit = true;
        self.schema_ver = schema_ver;
        self.isolation = isolation;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        let result = if let Some(handle) = self.handle.take() {
            handle.with_txn(|txn| txn.commit())
        } else {
            Ok(())
        };
        self.explicit = false;
        result
    }

    pub fn rollback(&mut self) -> Result<()> {
        let result = if let Some(handle) = self.handle.take() {
            handle.with_txn(|txn| txn.rollback())
        } else {
            Ok(())
        };
        self.explicit = false;
        result
    }

    /// Open an explicit transaction if the session has none, then record
    /// `name` on the KV txn (MySQL SAVEPOINT starts a transaction).
    pub fn savepoint(&mut self, storage: &dyn Storage, schema_ver: i64, name: &str) -> Result<()> {
        if self.handle.is_none() {
            self.enter(storage, false, schema_ver, self.isolation)?;
        }
        self.handle
            .as_ref()
            .expect("savepoint handle")
            .with_txn(|txn| txn.savepoint(name))
    }

    /// Restore writes to `name` without ending the transaction.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        let Some(handle) = &self.handle else {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_UNKNOWN_FUNCTION,
                format!("SAVEPOINT {name} does not exist"),
            ));
        };
        handle.with_txn(|txn| txn.rollback_to(name))
    }

    /// Drop `name` without restoring writes. The transaction stays open.
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let Some(handle) = &self.handle else {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_UNKNOWN_FUNCTION,
                format!("SAVEPOINT {name} does not exist"),
            ));
        };
        handle.with_txn(|txn| txn.release_savepoint(name))
    }

    pub fn ensure(&mut self, storage: &dyn Storage) -> Result<Arc<TxnHandle>> {
        if let Some(handle) = &self.handle {
            return Ok(Arc::clone(handle));
        }
        let handle = Arc::new(TxnHandle::new(storage.begin()?));
        self.handle = Some(Arc::clone(&handle));
        self.explicit = false;
        Ok(handle)
    }

    pub fn refresh_snapshot(&self) -> Result<()> {
        if let Some(handle) = &self.handle {
            handle.with_txn(|txn| txn.refresh_snapshot())?;
        }
        Ok(())
    }

    pub fn finish_autocommit(&mut self, ok: bool) -> Result<()> {
        if self.explicit {
            return Ok(());
        }
        if ok { self.commit() } else { self.rollback() }
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::new()
    }
}

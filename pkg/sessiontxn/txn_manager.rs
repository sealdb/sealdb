use sealdb_errno::Result;
use sealdb_kv::{Storage, TxnHandle};
use std::sync::Arc;

/// Session-owned transaction lifecycle for F12/F13.
/// Completeness wave / not this feature: SAVEPOINT, AND CHAIN, XA,
/// transactional DDL, isolation switching, blocking lock wait.
pub struct TxnManager {
    handle: Option<Arc<TxnHandle>>,
    explicit: bool,
}

impl TxnManager {
    pub fn new() -> Self {
        Self {
            handle: None,
            explicit: false,
        }
    }

    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    pub fn enter_begin(&mut self, storage: &dyn Storage) -> Result<()> {
        self.enter(storage, false)
    }

    pub fn enter_begin_pessimistic(&mut self, storage: &dyn Storage) -> Result<()> {
        self.enter(storage, true)
    }

    fn enter(&mut self, storage: &dyn Storage, pessimistic: bool) -> Result<()> {
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

    pub fn ensure(&mut self, storage: &dyn Storage) -> Result<Arc<TxnHandle>> {
        if let Some(handle) = &self.handle {
            return Ok(Arc::clone(handle));
        }
        let handle = Arc::new(TxnHandle::new(storage.begin()?));
        self.handle = Some(Arc::clone(&handle));
        self.explicit = false;
        Ok(handle)
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

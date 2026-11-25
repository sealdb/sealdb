use sealdb_errno::Result;
use sealdb_kv::{IsolationLevel, Storage, TxnHandle};
use std::sync::Arc;

/// Session-owned transaction lifecycle for //.
/// Completeness wave / not this feature: XA, blocking lock wait.
/// CREATE/DROP DATABASE|TABLE in an explicit txn is leftover-closed on
/// Domain in-memory schema; CREATE INDEX backfill KV stays .
pub struct TxnManager {
    handle: Option<Arc<TxnHandle>>,
    explicit: bool,
    schema_ver: i64,
    isolation: IsolationLevel,
    read_only: bool,
    pessimistic: bool,
}

impl TxnManager {
    pub fn new() -> Self {
        Self {
            handle: None,
            explicit: false,
            schema_ver: 0,
            isolation: IsolationLevel::RepeatableRead,
            read_only: false,
            pessimistic: false,
        }
    }

    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    /// The open transaction handle, if a transaction is in progress.
    pub fn handle(&self) -> Option<Arc<TxnHandle>> {
        self.handle.clone()
    }

    pub fn schema_ver(&self) -> i64 {
        self.schema_ver
    }

    /// After this session's own in-txn DDL so SchemaChecker 8028 does not
    /// reject later statements in the same transaction.
    pub fn set_schema_ver(&mut self, schema_ver: i64) {
        self.schema_ver = schema_ver;
    }

    pub fn isolation(&self) -> IsolationLevel {
        self.isolation
    }

    /// True when the current explicit txn was started `READ ONLY`.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn enter_begin(
        &mut self,
        storage: &dyn Storage,
        schema_ver: i64,
        isolation: IsolationLevel,
        read_only: bool,
        stale_at: Option<u64>,
    ) -> Result<()> {
        self.enter(storage, false, schema_ver, isolation, read_only, stale_at)
    }

    pub fn enter_begin_pessimistic(
        &mut self,
        storage: &dyn Storage,
        schema_ver: i64,
        isolation: IsolationLevel,
        read_only: bool,
        stale_at: Option<u64>,
    ) -> Result<()> {
        self.enter(storage, true, schema_ver, isolation, read_only, stale_at)
    }

    fn enter(
        &mut self,
        storage: &dyn Storage,
        pessimistic: bool,
        schema_ver: i64,
        isolation: IsolationLevel,
        read_only: bool,
        stale_at: Option<u64>,
    ) -> Result<()> {
        if self.handle.is_some() {
            self.commit()?;
        }
        // `tidb_enable_external_ts_read`: the explicit transaction also starts
        // from the external snapshot, like TiDB's `BEGIN` path.
        let txn = match stale_at {
            Some(ts) => storage.begin_stale_read_at(ts)?,
            None if pessimistic => storage.begin_pessimistic()?,
            None => storage.begin()?,
        };
        self.handle = Some(Arc::new(TxnHandle::new(txn)));
        self.explicit = true;
        self.schema_ver = schema_ver;
        self.isolation = isolation;
        self.read_only = read_only;
        self.pessimistic = pessimistic;
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

    /// Commit (or roll back) then open a new explicit txn with the same
    /// isolation, access mode, and pessimistic flag (`AND CHAIN`).
    pub fn chain(&mut self, storage: &dyn Storage, schema_ver: i64, commit: bool) -> Result<()> {
        let isolation = self.isolation;
        let read_only = self.read_only;
        let pessimistic = self.pessimistic;
        if commit {
            self.commit()?;
        } else {
            self.rollback()?;
        }
        self.enter(storage, pessimistic, schema_ver, isolation, read_only, None)
    }

    /// Open an explicit transaction if the session has none, then record
    /// `name` on the KV txn (MySQL SAVEPOINT starts a transaction).
    pub fn savepoint(&mut self, storage: &dyn Storage, schema_ver: i64, name: &str) -> Result<()> {
        if self.handle.is_none() {
            self.enter(
                storage,
                false,
                schema_ver,
                self.isolation,
                self.read_only,
                None,
            )?;
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

    /// Autocommit stale read (`SELECT ... AS OF TIMESTAMP`): open a snapshot
    /// at `unix_millis`. An explicit transaction keeps its own snapshot.
    pub fn ensure_stale(
        &mut self,
        storage: &dyn Storage,
        unix_millis: u64,
    ) -> Result<Arc<TxnHandle>> {
        if let Some(handle) = &self.handle {
            return Ok(Arc::clone(handle));
        }
        let handle = Arc::new(TxnHandle::new(storage.begin_stale_read(unix_millis)?));
        self.handle = Some(Arc::clone(&handle));
        self.explicit = false;
        Ok(handle)
    }

    /// External-timestamp stale read (`tidb_external_ts`): open a snapshot
    /// pinned at an explicit version. An explicit transaction keeps its own
    /// snapshot, so toggling the feature mid-transaction changes nothing.
    pub fn ensure_stale_at(&mut self, storage: &dyn Storage, ts: u64) -> Result<Arc<TxnHandle>> {
        if let Some(handle) = &self.handle {
            return Ok(Arc::clone(handle));
        }
        let handle = Arc::new(TxnHandle::new(storage.begin_stale_read_at(ts)?));
        self.handle = Some(Arc::clone(&handle));
        self.explicit = false;
        Ok(handle)
    }

    /// `@@tidb_current_ts`: the open transaction's start version, else a fresh
    /// storage timestamp.
    pub fn current_ts(&self, storage: &dyn Storage) -> Result<u64> {
        if let Some(handle) = &self.handle
            && let Some(ts) = handle.with_txn(|txn| Ok(txn.start_timestamp()))?
        {
            return Ok(ts);
        }
        storage.current_timestamp()
    }

    /// Keys written by the current transaction (empty when there is none).
    pub fn dirty_keys(&self) -> Vec<Vec<u8>> {
        let Some(handle) = &self.handle else {
            return Vec::new();
        };
        handle
            .with_txn(|txn| Ok(txn.dirty_keys()))
            .unwrap_or_default()
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

use crate::mdl::MdlTable;
use crate::schema_checker;
use sealdb_bindinfo::SessionBindingHandle;
use sealdb_ddl::execute_ddl;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::{MemStorage, Storage};
use sealdb_meta::{Job, JobState, Mutator};
use sealdb_parser::ast::Statement;
use sealdb_planner::InstancePlanCache;
use sealdb_privilege::{Manager, PRIVILEGE_STORAGE_KEY};
use sealdb_statistics::{Handle, STATS_STORAGE_KEY};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Domain-default system variables for `SET GLOBAL` / `@@GLOBAL.*` (C5 MVB).
/// New sessions copy these into session-local state at construction.
#[derive(Clone, Debug)]
pub struct GlobalSysVars {
    pub sql_mode: String,
    pub enable_auto_analyze: bool,
    /// Default connection charset (`character_set_connection` / SET NAMES default).
    pub character_set_connection: String,
}

impl Default for GlobalSysVars {
    fn default() -> Self {
        Self {
            sql_mode: "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES".to_string(),
            enable_auto_analyze: true,
            character_set_connection: "utf8mb4".to_string(),
        }
    }
}

/// Schema and NextGlobalID / NextHandle live in the `m` prefix of Storage
/// so `--store tikv` can reload tables after process restart.
pub struct Domain {
    inner: Mutex<Inner>,
    storage: Arc<dyn Storage>,
    privilege: Arc<Manager>,
    stats: Arc<Handle>,
    /// Domain-shared SQL bindings (`CREATE BINDING` / `SHOW BINDINGS`).
    bind_handle: SessionBindingHandle,
    /// Domain-shared prepared plan cache (TiDB instance plan cache, MVB).
    plan_cache: Arc<InstancePlanCache>,
    /// `SET GLOBAL` defaults visible to new sessions and `@@GLOBAL.*`.
    global_vars: Mutex<GlobalSysVars>,
    /// In-memory metadata locks (DDL exclusive vs DML shared).
    mdl: Mutex<MdlTable>,
    /// Monotonic session ids for MDL holders.
    next_session_id: AtomicU64,
    /// Schema lease: Duration::ZERO disables 8027 checks (default).
    schema_lease: Mutex<SchemaLeaseState>,
    /// Set by `SHUTDOWN`; tidb-server listen loop exits when true.
    shutdown_requested: AtomicBool,
}

/// C3 MVB schema lease / reload hook state.
struct SchemaLeaseState {
    lease: Duration,
    expire_at: Instant,
}

struct Inner {
    meta: Mutator,
    infoschema: InfoSchema,
    /// Skip Storage writes of Mutator until COMMIT after in-txn DDL.
    defer_persist: bool,
    jobs: Vec<Job>,
}

/// Mutator + InfoSchema captured before the first DDL in an explicit
/// transaction. Restored on ROLLBACK so CREATE/DROP does not survive.
#[derive(Clone)]
pub struct SchemaSnap {
    meta: Mutator,
    infoschema: InfoSchema,
}

const META_START: &[u8] = b"m";
const META_END: &[u8] = b"n";

impl Domain {
    pub fn new() -> Self {
        Self::with_privilege(Manager::skip_with_grant())
    }

    pub fn with_grant_table() -> Self {
        Self::with_privilege(Manager::with_grant_table())
    }

    pub fn with_storage(storage: Arc<dyn Storage>) -> Result<Self> {
        Self::with_storage_and_privilege(storage, Manager::skip_with_grant(), 0)
    }

    pub fn with_storage_id_base(storage: Arc<dyn Storage>, id_base: i64) -> Result<Self> {
        Self::with_storage_and_privilege(storage, Manager::skip_with_grant(), id_base)
    }

    /// Like `with_grant_table` but keeps grants in the given Storage (mysql.user persist).
    pub fn with_storage_and_grant_table(storage: Arc<dyn Storage>) -> Result<Self> {
        Self::with_storage_and_privilege(storage, Manager::with_grant_table(), 0)
    }

    fn with_privilege(privilege: Manager) -> Self {
        Self::with_storage_and_privilege(Arc::new(MemStorage::new()), privilege, 0)
            .expect("memory meta load")
    }

    fn with_storage_and_privilege(
        storage: Arc<dyn Storage>,
        privilege: Manager,
        id_base: i64,
    ) -> Result<Self> {
        let mut meta = load_meta(storage.as_ref())?;
        if id_base > 0 && meta.get_global_id().unwrap_or(0) == 0 {
            meta.set_global_id(id_base);
        }
        let infoschema = InfoSchema::from_meta(&meta)?;
        // Reload mysql.user-equivalent grant snapshot when present.
        if !privilege.skip_grant_table() {
            load_privileges(storage.as_ref(), &privilege)?;
        }
        let stats = Arc::new(Handle::new());
        load_stats(storage.as_ref(), &stats)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                meta,
                infoschema,
                defer_persist: false,
                jobs: Vec::new(),
            }),
            storage,
            privilege: Arc::new(privilege),
            stats,
            bind_handle: SessionBindingHandle::new(),
            plan_cache: Arc::new(InstancePlanCache::new()),
            global_vars: Mutex::new(GlobalSysVars::default()),
            mdl: Mutex::new(MdlTable::new()),
            next_session_id: AtomicU64::new(1),
            schema_lease: Mutex::new(SchemaLeaseState {
                lease: Duration::ZERO,
                expire_at: Instant::now(),
            }),
            shutdown_requested: AtomicBool::new(false),
        })
    }

    /// Mark that `SHUTDOWN` was requested (F22 server ops).
    pub fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
    }

    /// True after a successful `SHUTDOWN` statement.
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    /// Reload privilege cache from Storage (`FLUSH PRIVILEGES`).
    pub fn reload_privileges(&self) -> Result<()> {
        if self.privilege.skip_grant_table() {
            return Ok(());
        }
        load_privileges(self.storage.as_ref(), self.privilege.as_ref())
    }

    /// Allocate a Domain-local session id for MDL tracking.
    pub fn alloc_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Shared MDL for DML on `schema.table` (lowercase key).
    pub fn mdl_lock_shared(&self, table_key: &str, session_id: u64) -> Result<()> {
        self.mdl
            .lock()
            .expect("domain mdl lock")
            .lock_shared(table_key, session_id)
    }

    /// Exclusive MDL for DDL on `schema.table`.
    pub fn mdl_lock_exclusive(&self, table_key: &str, session_id: u64) -> Result<()> {
        self.mdl
            .lock()
            .expect("domain mdl lock")
            .lock_exclusive(table_key, session_id)
    }

    /// Release every MDL held by `session_id`.
    pub fn mdl_unlock_session(&self, session_id: u64) {
        self.mdl
            .lock()
            .expect("domain mdl lock")
            .unlock_session(session_id);
    }

    /// Configure schema lease duration. `Duration::ZERO` disables 8027.
    pub fn set_schema_lease(&self, lease: Duration) {
        let mut state = self.schema_lease.lock().expect("domain schema lease lock");
        state.lease = lease;
        if lease.is_zero() {
            state.expire_at = Instant::now();
        } else {
            state.expire_at = Instant::now() + lease;
        }
    }

    /// Refresh the lease expiry without reloading meta.
    pub fn touch_schema_lease(&self) {
        let mut state = self.schema_lease.lock().expect("domain schema lease lock");
        if !state.lease.is_zero() {
            state.expire_at = Instant::now() + state.lease;
        }
    }

    /// Return 8027 when the configured lease has elapsed.
    pub fn check_schema_lease(&self) -> Result<()> {
        let state = self.schema_lease.lock().expect("domain schema lease lock");
        if state.lease.is_zero() {
            return Ok(());
        }
        if Instant::now() > state.expire_at {
            return Err(schema_checker::expired());
        }
        Ok(())
    }

    /// Reload Mutator + InfoSchema from Storage and renew the lease (C3 MVB).
    pub fn reload_schema_from_storage(&self) -> Result<()> {
        let mut meta = load_meta(self.storage.as_ref())?;
        let infoschema = InfoSchema::from_meta(&meta)?;
        {
            let mut inner = self.inner.lock().expect("domain reload lock");
            // Preserve NextGlobalID bump already in memory when Storage is empty.
            if meta.get_global_id().unwrap_or(0) == 0 {
                let current = inner.meta.get_global_id().unwrap_or(0);
                if current > 0 {
                    meta.set_global_id(current);
                }
            }
            inner.meta = meta;
            inner.infoschema = infoschema;
            inner.defer_persist = false;
        }
        self.touch_schema_lease();
        Ok(())
    }

    /// Snapshot of Domain-default sysvars (`SET GLOBAL` / new-session defaults).
    pub fn global_vars(&self) -> GlobalSysVars {
        self.global_vars
            .lock()
            .expect("domain global vars lock")
            .clone()
    }

    pub fn set_global_sql_mode(&self, mode: String) {
        self.global_vars
            .lock()
            .expect("domain global vars lock")
            .sql_mode = mode;
    }

    pub fn set_global_enable_auto_analyze(&self, on: bool) {
        self.global_vars
            .lock()
            .expect("domain global vars lock")
            .enable_auto_analyze = on;
    }

    pub fn set_global_character_set_connection(&self, charset: String) {
        self.global_vars
            .lock()
            .expect("domain global vars lock")
            .character_set_connection = charset;
    }

    pub fn privilege(&self) -> Arc<Manager> {
        Arc::clone(&self.privilege)
    }

    /// Shared SQL binding handle (TiDB bindinfo session/global handle, MVB Domain-shared).
    pub fn bind_handle(&self) -> &SessionBindingHandle {
        &self.bind_handle
    }

    /// Shared instance plan cache (cross-session hits for same SQL + schema version).
    pub fn plan_cache(&self) -> Arc<InstancePlanCache> {
        Arc::clone(&self.plan_cache)
    }

    /// Test hook: successful plan-cache hits on this Domain.
    pub fn plan_cache_hit_count(&self) -> u64 {
        self.plan_cache.hit_count()
    }

    /// Persist grant tables (TiDB mysql.user / db / tables_priv) to Storage.
    pub fn persist_privileges(&self) -> Result<()> {
        if self.privilege.skip_grant_table() {
            return Ok(());
        }
        let bytes = self.privilege.encode()?;
        let mut txn = self.storage.begin()?;
        txn.set(PRIVILEGE_STORAGE_KEY, &bytes)?;
        txn.commit()
    }

    /// Persist table statistics (TiDB mysql.stats_* equivalent).
    pub fn persist_stats(&self) -> Result<()> {
        let bytes = self.stats.encode()?;
        let mut txn = self.storage.begin()?;
        txn.set(STATS_STORAGE_KEY, &bytes)?;
        txn.commit()
    }

    pub fn stats(&self) -> Arc<Handle> {
        Arc::clone(&self.stats)
    }

    pub fn infoschema(&self) -> InfoSchema {
        self.inner
            .lock()
            .expect("domain infoschema lock")
            .infoschema
            .clone()
    }

    pub fn storage(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.storage)
    }

    pub fn next_handle(&self, table_id: i64) -> Result<i64> {
        let mut inner = self.inner.lock().expect("domain handle lock");
        let next = inner.meta.alloc_handle(table_id)?;
        if !inner.defer_persist {
            persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        }
        Ok(next)
    }

    /// Clone Mutator + InfoSchema as they are now, before in-txn DDL.
    pub fn schema_snap(&self) -> SchemaSnap {
        let inner = self.inner.lock().expect("domain schema snap lock");
        SchemaSnap {
            meta: inner.meta.clone(),
            infoschema: inner.infoschema.clone(),
        }
    }

    /// Apply `stmt`. Autocommit (`persist`) writes meta immediately;
    /// an explicit txn leaves meta in memory until [`persist_schema`].
    pub fn execute_ddl(&self, stmt: &Statement, current_db: &str, persist: bool) -> Result<()> {
        let mut inner = self.inner.lock().expect("domain ddl lock");
        let id = inner.jobs.last().map(|job| job.id + 1).unwrap_or(1);
        inner.jobs.push(Job::from_stmt(id, stmt, current_db));
        let snapshot = inner.infoschema.clone();
        let result = execute_ddl(
            &mut inner.meta,
            &snapshot,
            stmt,
            current_db,
            self.storage.as_ref(),
        );
        if let Some(job) = inner.jobs.last_mut() {
            job.state = if result.is_ok() {
                JobState::Synced
            } else {
                JobState::Cancelled
            };
        }
        result?;
        inner.infoschema = InfoSchema::from_meta(&inner.meta)?;
        if persist {
            persist_meta(self.storage.as_ref(), &mut inner.meta)?;
            inner.defer_persist = false;
        } else {
            inner.defer_persist = true;
        }
        Ok(())
    }

    /// Newest-first snapshot for `ADMIN SHOW DDL JOBS`.
    pub fn ddl_jobs(&self) -> Vec<Job> {
        let inner = self.inner.lock().expect("domain ddl jobs lock");
        inner.jobs.iter().rev().cloned().collect()
    }

    /// Flush in-memory Mutator after COMMIT of a txn that ran DDL.
    pub fn persist_schema(&self) -> Result<()> {
        let mut inner = self.inner.lock().expect("domain persist schema lock");
        persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        inner.defer_persist = false;
        Ok(())
    }

    /// Put Mutator + InfoSchema back as they were before in-txn DDL.
    pub fn restore_schema(&self, snap: SchemaSnap) {
        let mut inner = self.inner.lock().expect("domain restore schema lock");
        inner.meta = snap.meta;
        inner.infoschema = snap.infoschema;
        inner.defer_persist = false;
    }
}

fn load_meta(storage: &dyn Storage) -> Result<Mutator> {
    let mut txn = storage.begin()?;
    let pairs = txn.scan(META_START, META_END)?;
    txn.rollback()?;
    let mut meta = Mutator::new();
    meta.load_encoded(pairs);
    Ok(meta)
}

fn load_privileges(storage: &dyn Storage, privilege: &Manager) -> Result<()> {
    let mut txn = storage.begin()?;
    let raw = txn.get(PRIVILEGE_STORAGE_KEY)?;
    txn.rollback()?;
    if let Some(bytes) = raw {
        privilege.decode(&bytes)?;
    }
    Ok(())
}

fn load_stats(storage: &dyn Storage, stats: &Handle) -> Result<()> {
    let mut txn = storage.begin()?;
    let raw = txn.get(STATS_STORAGE_KEY)?;
    txn.rollback()?;
    if let Some(bytes) = raw {
        stats.decode(&bytes)?;
    }
    Ok(())
}

fn persist_meta(storage: &dyn Storage, meta: &mut Mutator) -> Result<()> {
    let _lock = meta_file_lock()?;
    let removed = meta.take_removed();
    let mut txn = storage.begin()?;
    for key in removed {
        txn.delete(&key)?;
    }
    for (key, value) in meta.encoded_pairs() {
        txn.set(key, value)?;
    }
    txn.commit()
}

struct MetaFileLock {
    path: PathBuf,
}

impl Drop for MetaFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn meta_file_lock() -> Result<MetaFileLock> {
    let path = std::env::temp_dir().join("sealdb-meta.lock");
    for _ in 0..200 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(MetaFileLock { path }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                return Err(Error::new(ERR_UNKNOWN_ERROR, format!("meta lock: {err}")));
            }
        }
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        "timed out locking meta persist",
    ))
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for Domain {
    fn begin(&self) -> Result<Box<dyn sealdb_kv::Transaction>> {
        self.storage.begin()
    }

    fn begin_pessimistic(&self) -> Result<Box<dyn sealdb_kv::Transaction>> {
        self.storage.begin_pessimistic()
    }
}

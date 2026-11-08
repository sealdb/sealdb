use crate::mdl::MdlTable;
use crate::schema_checker;
use sealdb_bindinfo::SessionBindingHandle;
use sealdb_ddl::execute_ddl;
use sealdb_errno::{ERR_LOCK_WAIT_TIMEOUT, ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::{MemStorage, Storage};
use sealdb_meta::{Job, JobState, Mutator};
use sealdb_parser::ast::Statement;
use sealdb_planner::InstancePlanCache;
use sealdb_privilege::{Manager, PRIVILEGE_STORAGE_KEY};
use sealdb_statistics::{Handle, STATS_STORAGE_KEY};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Domain-default system variables for `SET GLOBAL` / `@@GLOBAL.*` (MVB).
/// New sessions copy these into session-local state at construction.
#[derive(Clone, Debug)]
pub struct GlobalSysVars {
    pub sql_mode: String,
    pub enable_auto_analyze: bool,
    /// Default connection charset (`character_set_connection` / SET NAMES default).
    pub character_set_connection: String,
    /// `tidb_partition_prune_mode`; new sessions inherit this value.
    pub partition_prune_mode: String,
}

impl Default for GlobalSysVars {
    fn default() -> Self {
        Self {
            sql_mode: "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES".to_string(),
            enable_auto_analyze: true,
            character_set_connection: "utf8mb4".to_string(),
            partition_prune_mode: "dynamic".to_string(),
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
    /// Connection/session registry for `KILL`.
    connections: Mutex<HashMap<u64, ConnState>>,
    /// Explicit `LOCK TABLES` holds; distinct from short-lived MDL.
    table_locks: Mutex<TableLockTable>,
}

#[derive(Clone, Debug, Default)]
struct ConnState {
    /// Set by `KILL [CONNECTION|QUERY] id`.
    killed: bool,
}

/// In-memory LOCK TABLES registry keyed by lowercase `schema.table`.
#[derive(Default)]
struct TableLockTable {
    /// table_key → (holder session_id, write?)
    held: HashMap<String, (u64, bool)>,
    /// session_id → table keys held.
    by_session: HashMap<u64, HashSet<String>>,
}

/// MVB schema lease / reload hook state.
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
        // TiDB `bootstrap()`: the `mysql` system schema and its grant tables.
        if sealdb_ddl::bootstrap::bootstrap_system_tables(&mut meta)? {
            persist_meta(storage.as_ref(), &mut meta)?;
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
            connections: Mutex::new(HashMap::new()),
            table_locks: Mutex::new(TableLockTable::default()),
        })
    }

    /// Mark that `SHUTDOWN` was requested ( server ops).
    pub fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
    }

    /// True after a successful `SHUTDOWN` statement.
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    /// Register a connection/session id for `KILL`.
    pub fn register_connection(&self, id: u64) {
        self.connections
            .lock()
            .expect("domain connections lock")
            .entry(id)
            .or_default();
    }

    /// `KILL` — mark connection/query for close. Returns false if id unknown.
    pub fn kill_connection(&self, id: u64) -> bool {
        let mut conns = self.connections.lock().expect("domain connections lock");
        if let Some(state) = conns.get_mut(&id) {
            state.killed = true;
            return true;
        }
        // Allow tests / admin to register-on-kill a synthetic id.
        conns.insert(id, ConnState { killed: true });
        true
    }

    /// True after `KILL` for this connection id.
    pub fn is_connection_killed(&self, id: u64) -> bool {
        self.connections
            .lock()
            .expect("domain connections lock")
            .get(&id)
            .is_some_and(|s| s.killed)
    }

    /// `LOCK TABLES` — acquire READ (shared) or WRITE (exclusive) table locks.
    pub fn lock_tables(&self, session_id: u64, locks: &[(String, bool)]) -> Result<()> {
        let mut table = self.table_locks.lock().expect("domain table locks");
        // MySQL: LOCK TABLES releases previous locks for this session first.
        table.unlock_session(session_id);
        for (key, _write) in locks {
            if let Some((holder, _)) = table.held.get(key)
                && *holder != session_id
            {
                // MVB: any cross-session hold conflicts (MySQL allows multi-READ;
                // SealDB serializes LOCK TABLES holders for simplicity).
                return Err(Error::new(
                    ERR_LOCK_WAIT_TIMEOUT,
                    format!(
                        "Lock wait timeout exceeded; try restarting transaction (LOCK TABLES on {key})"
                    ),
                ));
            }
        }
        for (key, write) in locks {
            table.held.insert(key.clone(), (session_id, *write));
            table
                .by_session
                .entry(session_id)
                .or_default()
                .insert(key.clone());
        }
        Ok(())
    }

    /// `UNLOCK TABLES` / empty `ADMIN CLEANUP TABLE LOCK` — drop session locks.
    pub fn unlock_tables(&self, session_id: u64) {
        self.table_locks
            .lock()
            .expect("domain table locks")
            .unlock_session(session_id);
    }

    /// `ADMIN CLEANUP TABLE LOCK t[,…]` — drop locks on the listed tables.
    pub fn cleanup_table_locks(&self, table_keys: &[String]) {
        let mut table = self.table_locks.lock().expect("domain table locks");
        for key in table_keys {
            if let Some((holder, _)) = table.held.remove(key)
                && let Some(set) = table.by_session.get_mut(&holder)
            {
                set.remove(key);
                if set.is_empty() {
                    table.by_session.remove(&holder);
                }
            }
        }
    }

    /// True when `session_id` holds a `LOCK TABLES … READ` entry for the table.
    pub fn session_read_locks_table(&self, session_id: u64, table_key: &str) -> bool {
        self.table_locks
            .lock()
            .expect("domain table locks")
            .held
            .get(table_key)
            .is_some_and(|(holder, write)| *holder == session_id && !*write)
    }

    /// True when `session_id` currently holds a LOCK TABLES entry.
    pub fn session_holds_table_lock(&self, session_id: u64, table_key: &str) -> bool {
        self.table_locks
            .lock()
            .expect("domain table locks")
            .by_session
            .get(&session_id)
            .is_some_and(|s| s.contains(table_key))
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
        let id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        self.register_connection(id);
        id
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

    /// Reload Mutator + InfoSchema from Storage and renew the lease (MVB).
    pub fn reload_schema_from_storage(&self) -> Result<()> {
        let mut meta = load_meta(self.storage.as_ref())?;
        // TiDB `bootstrap()`: the `mysql` system schema and its grant tables.
        if sealdb_ddl::bootstrap::bootstrap_system_tables(&mut meta)? {
            persist_meta(self.storage.as_ref(), &mut meta)?;
        }
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

    pub fn set_global_partition_prune_mode(&self, mode: String) {
        self.global_vars
            .lock()
            .expect("domain global vars lock")
            .partition_prune_mode = mode;
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

    /// Allocate the next value for a table's `AUTO_INCREMENT` column.
    pub fn next_auto_id(&self, table_id: i64) -> Result<i64> {
        let mut inner = self.inner.lock().expect("domain handle lock");
        let next = inner.meta.alloc_auto_id(table_id)?;
        if !inner.defer_persist {
            persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        }
        Ok(next)
    }

    /// Raise the `AUTO_INCREMENT` counter after an explicit user value.
    pub fn bump_auto_id(&self, table_id: i64, value: i64) -> Result<()> {
        let mut inner = self.inner.lock().expect("domain handle lock");
        inner.meta.bump_auto_id(table_id, value)?;
        if !inner.defer_persist {
            persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        }
        Ok(())
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
        // TiDB drops stats_meta with the table; stale ANALYZE must not linger
        // across DROP + CREATE of the same name (pseudo card until ANALYZE).
        if let Statement::DropTable(drop) = stmt {
            for name in &drop.tables {
                let (schema, table) = name.qualified(current_db);
                if !schema.is_empty() {
                    self.stats.delete(&schema, &table);
                }
            }
        }
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

    /// Allocate the next value for a SEQUENCE (TiDB `NEXTVAL`).
    pub fn next_sequence_value(&self, schema: &str, name: &str) -> Result<i64> {
        let mut inner = self.inner.lock().expect("domain sequence lock");
        let is = inner.infoschema.clone();
        let db = is.schema_by_name(schema).ok_or_else(|| {
            sealdb_errno::Error::new(
                sealdb_errno::ERR_BAD_DB,
                format!("Unknown database '{schema}'"),
            )
        })?;
        let table = is.table_by_name(schema, name).ok_or_else(|| {
            sealdb_errno::Error::new(
                sealdb_errno::ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            )
        })?;
        if !table.is_sequence() {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_WRONG_OBJECT,
                format!("'{schema}.{name}' is not SEQUENCE"),
            ));
        }
        let mut info = table.clone();
        let seq = info.sequence.as_mut().expect("is_sequence");
        let value = seq.next_val().map_err(sealdb_errno::Error::unsupported)?;
        inner.meta.update_table(db.id, &info)?;
        inner.infoschema = InfoSchema::from_meta(&inner.meta)?;
        if !inner.defer_persist {
            persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        }
        Ok(value)
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

impl TableLockTable {
    fn unlock_session(&mut self, session_id: u64) {
        let Some(keys) = self.by_session.remove(&session_id) else {
            return;
        };
        for key in keys {
            if self
                .held
                .get(&key)
                .is_some_and(|(holder, _)| *holder == session_id)
            {
                self.held.remove(&key);
            }
        }
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

#[derive(Debug)]
struct MetaFileLock {
    path: PathBuf,
    /// Serializes persist attempts inside this process, so a pid seen in the
    /// lock file can only be a *previous* process that had the same pid.
    _in_process: std::sync::MutexGuard<'static, ()>,
}

impl Drop for MetaFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static META_PERSIST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn meta_file_lock() -> Result<MetaFileLock> {
    meta_file_lock_at(&std::env::temp_dir().join("sealdb-meta.lock"))
}

/// Cross-process lock around `persist_meta`. The file records the owner's pid
/// so a lock left behind by a killed run (SIGKILL skips `Drop`) does not wedge
/// every later server on this machine forever.
fn meta_file_lock_at(path: &Path) -> Result<MetaFileLock> {
    let in_process = META_PERSIST_MUTEX
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for attempt in 0..200 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
                return Ok(MetaFileLock {
                    path: path.to_path_buf(),
                    _in_process: in_process,
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // Re-check for an abandoned lock about twice per second rather
                // than on every 10ms retry.
                if attempt > 0 && attempt % 50 == 0 && meta_lock_is_stale(path) {
                    let _ = std::fs::remove_file(path);
                    continue;
                }
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

/// A lock file is abandoned when its owner pid is gone. Liveness is checked
/// with `kill -0`; an unparsable file is only abandoned once it is old enough
/// that it cannot be a holder that is still writing its pid.
fn meta_lock_is_stale(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    match raw.trim().parse::<u32>() {
        // We hold `META_PERSIST_MUTEX`, so this file predates our process: pids
        // are unique while alive, and this one was recycled after a kill.
        Ok(pid) if pid == std::process::id() => true,
        Ok(pid) => !process_is_alive(pid) || meta_lock_expired(path),
        Err(_) => meta_lock_expired(path),
    }
}

/// A persist never legitimately holds the lock for minutes; past that the
/// owning pid has been recycled by an unrelated long-lived process.
fn meta_lock_expired(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= Duration::from_secs(120))
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    match std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
    {
        Ok(out) if out.status.success() => true,
        // EPERM means the pid exists but is owned by someone else.
        Ok(out) => String::from_utf8_lossy(&out.stderr).contains("permitted"),
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
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

    /// `SELECT ... AS OF TIMESTAMP`: snapshot at the wall clock's PD TSO.
    fn begin_stale_read(&self, unix_millis: u64) -> Result<Box<dyn sealdb_kv::Transaction>> {
        self.storage.begin_stale_read(unix_millis)
    }
}

#[cfg(test)]
mod meta_lock_tests {
    use super::*;

    fn scratch_lock(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sealdb-meta-lock-{}-{name}", std::process::id()))
    }

    #[test]
    fn abandoned_lock_is_taken_over() {
        let path = scratch_lock("abandoned");
        // pid 4294967295 never exists; mimics a run killed while holding the lock.
        std::fs::write(&path, "4294967295").expect("seed stale lock");
        let lock = meta_file_lock_at(&path).expect("steal abandoned lock");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(lock);
        assert!(!path.exists(), "lock file removed on drop");
    }

    #[test]
    fn recycled_own_pid_is_taken_over() {
        let path = scratch_lock("recycled");
        // A killed run left the lock behind and the OS later reused its pid for
        // this process; the in-process mutex proves we are not the holder.
        std::fs::write(&path, std::process::id().to_string()).expect("seed recycled lock");
        let lock = meta_file_lock_at(&path).expect("steal recycled lock");
        drop(lock);
        assert!(!path.exists(), "lock file removed on drop");
    }

    #[test]
    fn live_owner_is_respected() {
        let path = scratch_lock("live");
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn lock owner");
        std::fs::write(&path, child.id().to_string()).expect("seed live lock");
        let err = meta_file_lock_at(&path).expect_err("must not steal a live lock");
        assert!(
            err.message.contains("timed out locking meta persist"),
            "{}",
            err.message
        );
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_file(&path).ok();
    }
}

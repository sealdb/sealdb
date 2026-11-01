use sealdb_ddl::execute_ddl;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::{MemStorage, Storage};
use sealdb_meta::Mutator;
use sealdb_parser::ast::Statement;
use sealdb_privilege::Manager;
use sealdb_statistics::Handle;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Schema and NextGlobalID / NextHandle live in the `m` prefix of Storage
/// so `--store tikv` can reload tables after process restart.
pub struct Domain {
    inner: Mutex<Inner>,
    storage: Arc<dyn Storage>,
    privilege: Arc<Manager>,
    stats: Arc<Handle>,
}

struct Inner {
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
        Ok(Self {
            inner: Mutex::new(Inner { meta, infoschema }),
            storage,
            privilege: Arc::new(privilege),
            stats: Arc::new(Handle::new()),
        })
    }

    pub fn privilege(&self) -> Arc<Manager> {
        Arc::clone(&self.privilege)
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
        persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        Ok(next)
    }

    pub fn execute_ddl(&self, stmt: &Statement, current_db: &str) -> Result<()> {
        let mut inner = self.inner.lock().expect("domain ddl lock");
        let snapshot = inner.infoschema.clone();
        execute_ddl(
            &mut inner.meta,
            &snapshot,
            stmt,
            current_db,
            self.storage.as_ref(),
        )?;
        inner.infoschema = InfoSchema::from_meta(&inner.meta)?;
        persist_meta(self.storage.as_ref(), &mut inner.meta)?;
        Ok(())
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

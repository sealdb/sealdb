use sealdb_ddl::execute_ddl;
use sealdb_errno::Result;
use sealdb_infoschema::InfoSchema;
use sealdb_kv::{MemStorage, Storage};
use sealdb_meta::Mutator;
use sealdb_parser::ast::Statement;
use sealdb_privilege::Manager;
use sealdb_statistics::Handle;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Completeness wave: persist meta / next_handle to PD+TiKV so a
/// restarted `--store tikv` process can reload tables.
pub struct Domain {
    inner: Mutex<Inner>,
    storage: Arc<dyn Storage>,
    privilege: Arc<Manager>,
    stats: Arc<Handle>,
}

struct Inner {
    meta: Mutator,
    infoschema: InfoSchema,
    next_handle: HashMap<i64, i64>,
}

impl Domain {
    pub fn new() -> Self {
        Self::with_privilege(Manager::skip_with_grant())
    }

    pub fn with_grant_table() -> Self {
        Self::with_privilege(Manager::with_grant_table())
    }

    pub fn with_storage(storage: Arc<dyn Storage>) -> Self {
        Self::with_storage_and_privilege(storage, Manager::skip_with_grant())
    }

    fn with_privilege(privilege: Manager) -> Self {
        Self::with_storage_and_privilege(Arc::new(MemStorage::new()), privilege)
    }

    fn with_storage_and_privilege(storage: Arc<dyn Storage>, privilege: Manager) -> Self {
        Self {
            inner: Mutex::new(Inner {
                meta: Mutator::new(),
                infoschema: InfoSchema::empty(),
                next_handle: HashMap::new(),
            }),
            storage,
            privilege: Arc::new(privilege),
            stats: Arc::new(Handle::new()),
        }
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

    pub fn next_handle(&self, table_id: i64) -> i64 {
        let mut inner = self.inner.lock().expect("domain handle lock");
        let next = inner.next_handle.entry(table_id).or_insert(0);
        *next += 1;
        *next
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
        Ok(())
    }
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

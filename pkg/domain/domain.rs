use sealdb_ddl::execute_ddl;
use sealdb_errno::Result;
use sealdb_infoschema::InfoSchema;
use sealdb_meta::Mutator;
use sealdb_parser::ast::Statement;
use std::sync::Mutex;

pub struct Domain {
    inner: Mutex<Inner>,
}

struct Inner {
    meta: Mutator,
    infoschema: InfoSchema,
}

impl Domain {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                meta: Mutator::new(),
                infoschema: InfoSchema::empty(),
            }),
        }
    }

    pub fn infoschema(&self) -> InfoSchema {
        self.inner
            .lock()
            .expect("domain infoschema lock")
            .infoschema
            .clone()
    }

    pub fn execute_ddl(&self, stmt: &Statement, current_db: &str) -> Result<()> {
        let mut inner = self.inner.lock().expect("domain ddl lock");
        let snapshot = inner.infoschema.clone();
        execute_ddl(&mut inner.meta, &snapshot, stmt, current_db)?;
        inner.infoschema = InfoSchema::from_meta(&inner.meta)?;
        Ok(())
    }
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_parser::ast::Statement;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

use crate::internal::exec::Executor;

/// Runs CREATE/DROP/ALTER. `persist` is false inside an explicit txn so
/// schema stays in memory until COMMIT or ROLLBACK.
pub struct DDLExec {
    domain: Arc<Domain>,
    stmt: Statement,
    current_db: String,
    persist: bool,
    done: bool,
}

impl DDLExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: Statement,
        current_db: impl Into<String>,
        persist: bool,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            persist,
            done: false,
        }
    }
}

impl Executor for DDLExec {
    fn open(&mut self) -> Result<()> {
        self.domain
            .execute_ddl(&self.stmt, &self.current_db, self.persist)
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

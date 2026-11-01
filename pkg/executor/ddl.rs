use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_parser::ast::Statement;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

use crate::internal::exec::Executor;

pub struct DDLExec {
    domain: Arc<Domain>,
    stmt: Statement,
    current_db: String,
    done: bool,
}

impl DDLExec {
    pub fn new(domain: Arc<Domain>, stmt: Statement, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            done: false,
        }
    }
}

impl Executor for DDLExec {
    fn open(&mut self) -> Result<()> {
        self.domain.execute_ddl(&self.stmt, &self.current_db)
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

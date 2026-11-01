/// F20 core: GrantExec / revoke via privilege Manager (TiDB `executor/grant.go`).
/// Completeness wave: mysql.* tables, WITH GRANT OPTION, roles.
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_parser::ast::Statement;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct GrantExec {
    domain: Arc<Domain>,
    stmt: Statement,
    done: bool,
}

impl GrantExec {
    pub fn new(domain: Arc<Domain>, stmt: Statement) -> Self {
        Self {
            domain,
            stmt,
            done: false,
        }
    }
}

impl Executor for GrantExec {
    fn open(&mut self) -> Result<()> {
        match &self.stmt {
            Statement::Grant(grant) => self.domain.privilege().grant(grant)?,
            Statement::Revoke(revoke) => self.domain.privilege().revoke(revoke)?,
            _ => {}
        }
        self.done = true;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

/// F20: GrantExec — GRANT/REVOKE privileges and roles, CREATE/DROP ROLE
/// (TiDB `executor/grant.go` / `role.go`). Completeness wave: mysql.* tables,
/// WITH GRANT OPTION.
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
            Statement::CreateRole(create) => self.domain.privilege().create_role(create)?,
            Statement::DropRole(drop) => self.domain.privilege().drop_role(drop)?,
            Statement::CreateUser(create) => self.domain.privilege().create_user(create)?,
            Statement::DropUser(drop) => self.domain.privilege().drop_user(drop)?,
            Statement::GrantRole(grant) => self.domain.privilege().grant_role(grant)?,
            Statement::RevokeRole(revoke) => self.domain.privilege().revoke_role(revoke)?,
            _ => {}
        }
        // Flush mysql.user-equivalent snapshot after privilege DDL.
        self.domain.persist_privileges()?;
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

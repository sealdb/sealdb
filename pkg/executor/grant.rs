/// F20: GrantExec — GRANT/REVOKE privileges and roles, CREATE/DROP ROLE
/// (TiDB `executor/grant.go` / `role.go`). Completeness wave: mysql.* tables,
/// WITH GRANT OPTION.
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_expression::eval_expr;
use sealdb_parser::ast::{FlushStmtType, Statement, UserIdentity};
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct GrantExec {
    domain: Arc<Domain>,
    stmt: Statement,
    current_user: UserIdentity,
    done: bool,
}

impl GrantExec {
    pub fn new(domain: Arc<Domain>, stmt: Statement) -> Self {
        Self::with_user(domain, stmt, UserIdentity::root())
    }

    pub fn with_user(domain: Arc<Domain>, stmt: Statement, current_user: UserIdentity) -> Self {
        Self {
            domain,
            stmt,
            current_user,
            done: false,
        }
    }
}

impl Executor for GrantExec {
    fn open(&mut self) -> Result<()> {
        let mut persist_priv = false;
        match &self.stmt {
            Statement::Grant(grant) => {
                self.domain.privilege().grant(grant)?;
                persist_priv = true;
            }
            Statement::Revoke(revoke) => {
                self.domain.privilege().revoke(revoke)?;
                persist_priv = true;
            }
            Statement::CreateRole(create) => {
                self.domain.privilege().create_role(create)?;
                persist_priv = true;
            }
            Statement::DropRole(drop) => {
                self.domain.privilege().drop_role(drop)?;
                persist_priv = true;
            }
            Statement::CreateUser(create) => {
                self.domain.privilege().create_user(create)?;
                persist_priv = true;
            }
            Statement::DropUser(drop) => {
                self.domain.privilege().drop_user(drop)?;
                persist_priv = true;
            }
            Statement::GrantRole(grant) => {
                self.domain.privilege().grant_role(grant)?;
                persist_priv = true;
            }
            Statement::RevokeRole(revoke) => {
                self.domain.privilege().revoke_role(revoke)?;
                persist_priv = true;
            }
            Statement::SetPwd(set) => {
                self.domain
                    .privilege()
                    .set_password(set, &self.current_user)?;
                persist_priv = true;
            }
            Statement::Flush(flush) => match flush.tp {
                FlushStmtType::Privileges => self.domain.reload_privileges()?,
                FlushStmtType::Tables | FlushStmtType::Other => {}
            },
            Statement::Do(doit) => {
                for expr in &doit.exprs {
                    let _ = eval_expr(expr)?;
                }
            }
            Statement::Shutdown(_) => {
                self.domain.request_shutdown();
            }
            _ => {}
        }
        // Flush mysql.user-equivalent snapshot after privilege DDL.
        if persist_priv {
            self.domain.persist_privileges()?;
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

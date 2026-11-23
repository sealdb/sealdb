/// GrantExec — GRANT/REVOKE privileges and roles, CREATE/DROP ROLE
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
    /// Session database, used to qualify `GRANT … ON t` table levels.
    current_db: String,
    done: bool,
}

impl GrantExec {
    pub fn new(domain: Arc<Domain>, stmt: Statement) -> Self {
        Self::with_user(domain, stmt, UserIdentity::root())
    }

    pub fn with_current_db(mut self, current_db: String) -> Self {
        self.current_db = current_db;
        self
    }

    /// TiDB resolves an unqualified table in `GRANT`/`REVOKE` against the
    /// session database before recording the grant (`ON t` → `ON db.t`).
    fn qualify_level(&self, level: &mut sealdb_parser::ast::GrantLevel) {
        if level.level == sealdb_parser::ast::GrantLevelType::Table
            && level.db_name.is_empty()
            && !self.current_db.is_empty()
        {
            level.db_name = self.current_db.clone();
        }
    }

    /// `CURRENT_USER()` lowers to an empty identity; resolve it against the
    /// connection executing the statement (`REVOKE ALL … FROM
    /// CURRENT_USER()`). The session publishes its identity as the privilege
    /// thread-local before every statement; `current_user` is only a fallback.
    fn resolve_current_user(&self, users: &mut [UserIdentity]) {
        let caller = sealdb_privilege::thread_user().unwrap_or_else(|| self.current_user.clone());
        for user in users.iter_mut() {
            if user.username.is_empty() {
                *user = caller.clone();
            }
        }
    }

    pub fn with_user(domain: Arc<Domain>, stmt: Statement, current_user: UserIdentity) -> Self {
        Self {
            domain,
            stmt,
            current_user,
            current_db: String::new(),
            done: false,
        }
    }
}

impl Executor for GrantExec {
    fn open(&mut self) -> Result<()> {
        let mut persist_priv = false;
        match &self.stmt {
            Statement::Grant(grant) => {
                let mut grant = grant.clone();
                self.qualify_level(&mut grant.level);
                self.resolve_current_user(&mut grant.users);
                self.domain.privilege().grant(&grant)?;
                persist_priv = true;
            }
            Statement::Revoke(revoke) => {
                let mut revoke = revoke.clone();
                self.qualify_level(&mut revoke.level);
                self.resolve_current_user(&mut revoke.users);
                self.domain.privilege().revoke(&revoke)?;
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
            Statement::AlterUser(alter) => {
                self.domain.privilege().alter_user(alter)?;
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
            Statement::RenameUser(rename) => {
                self.domain.privilege().rename_user(rename)?;
                persist_priv = true;
            }
            Statement::Kill(kill) => {
                let _ = self.domain.kill_connection(kill.connection_id);
            }
            Statement::Binlog(_) | Statement::Change(_) | Statement::AlterInstance(_) => {}
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

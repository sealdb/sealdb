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

    /// TiDB warns 3929 for privilege names it does not know and ignores them
    /// (`REVOKE bogus ON *.* FROM u` succeeds).
    fn warn_unknown_dynamic_privs(dynamic: &[String]) {
        for name in dynamic {
            if !sealdb_parser::sqlparser_bridge::is_dynamic_priv(name) {
                sealdb_expression::note_warning(
                    3929,
                    format!("Dynamic privilege '{name}' is not registered with the server."),
                );
            }
        }
    }

    /// `GRANT` rejects unknown dynamic privilege names with 3929 (only
    /// `REVOKE` degrades them to a warning).
    fn reject_unknown_dynamic_privs(dynamic: &[String]) -> Result<()> {
        for name in dynamic {
            if !sealdb_parser::sqlparser_bridge::is_dynamic_priv(name) {
                return Err(sealdb_errno::Error::new(
                    3929,
                    format!("Dynamic privilege '{name}' is not registered with the server."),
                ));
            }
        }
        Ok(())
    }

    /// `GRANT`-side validation TiDB runs before touching the grant tables:
    /// 3941 unknown dynamic privileges, 1044 for `performance_schema`, 1221
    /// for column grants of non-column privileges, 1144 for privileges that
    /// do not exist below the database level, and 1146 for a missing table
    /// (only `CREATE` may be granted on one).
    fn validate_grant(&self, grant: &sealdb_parser::ast::GrantStmt) -> Result<()> {
        use sealdb_parser::ast::{GrantLevelType, PrivilegeType};
        Self::reject_unknown_dynamic_privs(&grant.dynamic_privs)?;
        let column_grantable = PrivilegeType::SELECT
            .union(PrivilegeType::INSERT)
            .union(PrivilegeType::UPDATE)
            .union(PrivilegeType::REFERENCES);
        // Privileges MySQL/TiDB only records at the database level or above.
        let db_only = PrivilegeType::CREATE_TEMP_TABLES
            .union(PrivilegeType::LOCK_TABLES)
            .union(PrivilegeType::EVENT)
            .union(PrivilegeType::CREATE_ROUTINE)
            .union(PrivilegeType::ALTER_ROUTINE)
            .union(PrivilegeType::CREATE_TABLESPACE)
            .union(PrivilegeType::CREATE_USER)
            .union(PrivilegeType::PROCESS)
            .union(PrivilegeType::SUPER)
            .union(PrivilegeType::RELOAD)
            .union(PrivilegeType::SHUTDOWN)
            .union(PrivilegeType::FILE)
            .union(PrivilegeType::REPLICATION_SLAVE)
            .union(PrivilegeType::REPLICATION_CLIENT)
            .union(PrivilegeType::CONFIG)
            .union(PrivilegeType::SYSTEM_USER)
            .union(PrivilegeType::CREATE_ROLE)
            .union(PrivilegeType::DROP_ROLE);
        for elem in &grant.privs {
            if !elem.cols.is_empty() && !column_grantable.contains(elem.tp) {
                return Err(sealdb_errno::Error::new(
                    sealdb_errno::ERR_INCORRECT_USAGE,
                    "Incorrect usage of COLUMN GRANT and NON-COLUMN PRIVILEGES",
                ));
            }
            if grant.level.level == GrantLevelType::Table && db_only.contains(elem.tp) {
                return Err(sealdb_errno::Error::new(
                    sealdb_errno::ERR_ILLEGAL_GRANT_FOR_TABLE,
                    "Illegal GRANT/REVOKE command; please consult the manual to see which privileges can be used",
                ));
            }
        }
        if grant.level.level == GrantLevelType::DB
            && grant
                .level
                .db_name
                .eq_ignore_ascii_case("performance_schema")
            && !grant.privs.is_empty()
            && grant
                .privs
                .iter()
                .any(|elem| !elem.tp.is_empty() && elem.tp != PrivilegeType::SELECT)
        {
            let caller = sealdb_privilege::thread_user()
                .unwrap_or_else(sealdb_parser::ast::UserIdentity::root);
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_DBACCESS_DENIED_ERROR,
                format!(
                    "Access denied for user '{}'@'{}' to database '{}'",
                    caller.username, caller.hostname, grant.level.db_name
                ),
            ));
        }
        if grant.level.level == GrantLevelType::Table && !grant.privs.is_empty() {
            let db = if grant.level.db_name.is_empty() {
                self.current_db.clone()
            } else {
                grant.level.db_name.clone()
            };
            let table = &grant.level.table_name;
            // `metrics_schema` tables are virtual (`up`, …) and never appear in
            // the catalog, but TiDB still accepts grants on them.
            let virtual_schema = db.eq_ignore_ascii_case("metrics_schema");
            let is = self.domain.infoschema();
            if !virtual_schema && is.table_by_name(&db, table).is_none() {
                // `CREATE` does not need the table to exist yet (TiDB
                // `grant.go`); `ALL` includes it.
                let creates = grant
                    .privs
                    .iter()
                    .any(|elem| elem.tp.is_all() || elem.tp.contains(PrivilegeType::CREATE));
                if !creates {
                    return Err(sealdb_errno::Error::new(
                        sealdb_errno::ERR_NO_SUCH_TABLE,
                        format!("Table '{db}.{table}' doesn't exist"),
                    ));
                }
            }
        }
        Ok(())
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
                self.validate_grant(&grant)?;
                self.domain.privilege().grant(&grant)?;
                persist_priv = true;
            }
            Statement::Revoke(revoke) => {
                let mut revoke = revoke.clone();
                self.qualify_level(&mut revoke.level);
                self.resolve_current_user(&mut revoke.users);
                Self::warn_unknown_dynamic_privs(&revoke.dynamic_privs);
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
                self.domain
                    .privilege()
                    .create_user(create, self.domain.password_policy())?;
                persist_priv = true;
            }
            Statement::AlterUser(alter) => {
                let warnings = self
                    .domain
                    .privilege()
                    .alter_user(alter, self.domain.password_policy())?;
                for (code, message) in warnings {
                    // `ALTER USER IF EXISTS` reports missing accounts as notes.
                    sealdb_expression::note_message(i32::from(code), message);
                }
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
                self.domain.privilege().set_password(
                    set,
                    &self.current_user,
                    self.domain.password_policy(),
                )?;
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

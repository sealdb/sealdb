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
        // `CREATE VIEW` without an explicit definer stores the connection user
        // (`root@%`), like TiDB; `SHOW CREATE VIEW` prints it verbatim.
        let stmt = match stmt {
            Statement::CreateView(mut create) => {
                if create.definer.eq_ignore_ascii_case("CURRENT_USER")
                    && let Some(user) = sealdb_privilege::thread_user()
                {
                    create.definer = format!("`{}`@`{}`", user.username, user.hostname);
                }
                Statement::CreateView(create)
            }
            other => other,
        };
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

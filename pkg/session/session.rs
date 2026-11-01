use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_executor::{ExecutorBuilder, QueryResult};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::Parser;
use sealdb_planner::PlanBuilder;
use std::sync::{Arc, Mutex};

pub struct Session {
    parser: Parser,
    planner: PlanBuilder,
    domain: Arc<Domain>,
    current_db: Mutex<String>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            planner: PlanBuilder::new(),
            domain: Arc::new(Domain::new()),
            current_db: Mutex::new(String::new()),
        }
    }

    pub fn execute_stmt(&self, sql: &str) -> Result<QueryResult> {
        let stmt = self.parser.parse_one_stmt(sql)?;
        if let sealdb_parser::ast::Statement::Use(use_stmt) = &stmt {
            let is = self.domain.infoschema();
            if is.schema_by_name(&use_stmt.db_name).is_none() {
                return Err(sealdb_errno::Error::new(
                    sealdb_errno::ERR_BAD_DB,
                    format!("Unknown database '{}'", use_stmt.db_name),
                ));
            }
            *self.current_db.lock().expect("session current_db lock") = use_stmt.db_name.clone();
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }
        let plan = self.planner.build(&stmt)?;
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        let exec = ExecutorBuilder::build(&plan, &stmt, Arc::clone(&self.domain), &current_db)?;
        ExecutorBuilder::run(exec)
    }

    pub fn infoschema(&self) -> InfoSchema {
        self.domain.infoschema()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_errno::{ERR_DB_CREATE_EXISTS, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS};
    use sealdb_types::Datum;

    #[test]
    fn test_select_one() {
        let session = Session::new();
        let result = session.execute_stmt("SELECT 1").unwrap();
        assert_eq!(result.rows, vec![vec![Datum::I64(1)]]);
    }

    #[test]
    fn test_create_drop_database_and_table() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        let is = session.infoschema();
        assert!(is.schema_by_name("db1").is_some());
        let table = is.table_by_name("db1", "t").unwrap();
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name.o(), "id");
        assert_eq!(table.columns[1].name.o(), "name");
        session.execute_stmt("DROP TABLE db1.t").unwrap();
        session.execute_stmt("DROP DATABASE db1").unwrap();
        assert!(session.infoschema().schema_by_name("db1").is_none());
    }

    #[test]
    fn test_if_exists_and_duplicate_errors() {
        let session = Session::new();
        session
            .execute_stmt("CREATE DATABASE IF NOT EXISTS db1")
            .unwrap();
        session
            .execute_stmt("CREATE DATABASE IF NOT EXISTS db1")
            .unwrap();
        let err = session.execute_stmt("CREATE DATABASE db1").unwrap_err();
        assert_eq!(err.code, ERR_DB_CREATE_EXISTS);
        session
            .execute_stmt("CREATE TABLE db1.t (id BIGINT)")
            .unwrap();
        let err = session
            .execute_stmt("CREATE TABLE db1.t (id BIGINT)")
            .unwrap_err();
        assert_eq!(err.code, ERR_TABLE_EXISTS);
        session
            .execute_stmt("CREATE TABLE IF NOT EXISTS db1.t (id BIGINT)")
            .unwrap();
        session
            .execute_stmt("DROP TABLE IF EXISTS db1.missing")
            .unwrap();
        let err = session.execute_stmt("DROP TABLE db1.missing").unwrap_err();
        assert_eq!(err.code, ERR_NO_SUCH_TABLE);
        session
            .execute_stmt("DROP DATABASE IF EXISTS missing_db")
            .unwrap();
    }

    #[test]
    fn test_show_and_desc() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        let dbs = session.execute_stmt("SHOW DATABASES").unwrap();
        assert!(
            dbs.display_rows()
                .iter()
                .any(|row| row.first().map(String::as_str) == Some("db1"))
        );
        let tables = session.execute_stmt("SHOW TABLES FROM db1").unwrap();
        assert_eq!(tables.columns, vec!["Tables_in_db1"]);
        assert_eq!(tables.display_rows(), vec![vec!["t".to_string()]]);
        session.execute_stmt("USE db1").unwrap();
        let desc = session.execute_stmt("DESC t").unwrap();
        assert_eq!(desc.columns[0], "Field");
        assert_eq!(desc.display_rows()[0][0], "id");
        assert_eq!(desc.display_rows()[1][0], "name");
    }
}

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
        let stmts = self.parser.parse_sql(sql)?;
        if stmts.is_empty() {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_PARSE_ERROR,
                "empty SQL",
            ));
        }
        let mut last = QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
        };
        for stmt in stmts {
            last = self.execute_one(&stmt)?;
        }
        Ok(last)
    }

    fn execute_one(&self, stmt: &sealdb_parser::ast::Statement) -> Result<QueryResult> {
        if let sealdb_parser::ast::Statement::Use(use_stmt) = stmt {
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
        let plan = self.planner.build(stmt)?;
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        let exec = ExecutorBuilder::build(&plan, stmt, Arc::clone(&self.domain), &current_db)?;
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

    #[test]
    fn test_insert_and_select_star() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        let result = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(result.columns, vec!["id", "name"]);
        assert_eq!(
            result.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
    }

    #[test]
    fn test_where_selection() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let eq = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 2")
            .unwrap();
        assert_eq!(
            eq.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let and = session
            .execute_stmt("SELECT * FROM db1.t WHERE id > 1 AND name = 'c'")
            .unwrap();
        assert_eq!(
            and.display_rows(),
            vec![vec!["3".to_string(), "c".to_string()]]
        );
        let none = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 9")
            .unwrap();
        assert!(none.display_rows().is_empty());
        let constant = session.execute_stmt("SELECT 1 WHERE 0").unwrap();
        assert!(constant.display_rows().is_empty());
        let keep = session.execute_stmt("SELECT 1 WHERE 1").unwrap();
        assert_eq!(keep.display_rows(), vec![vec!["1".to_string()]]);
    }

    #[test]
    fn test_order_by_and_limit() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (3, 'c'), (1, 'a'), (2, 'b')",
            )
            .unwrap();
        let ordered = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            ordered.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()],
                vec!["3".to_string(), "c".to_string()]
            ]
        );
        let desc = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id DESC LIMIT 1")
            .unwrap();
        assert_eq!(
            desc.display_rows(),
            vec![vec!["3".to_string(), "c".to_string()]]
        );
        let offset = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id LIMIT 1 OFFSET 1")
            .unwrap();
        assert_eq!(
            offset.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_point_get() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        let hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 2")
            .unwrap();
        assert_eq!(
            hit.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let miss = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 9")
            .unwrap();
        assert!(miss.display_rows().is_empty());
    }

    #[test]
    fn test_update_and_delete() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        session
            .execute_stmt("UPDATE db1.t SET name = 'x' WHERE id = 1")
            .unwrap();
        let updated = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(
            updated.display_rows(),
            vec![vec!["1".to_string(), "x".to_string()]]
        );
        session
            .execute_stmt("UPDATE db1.t SET name = 'y' WHERE name = 'b'")
            .unwrap();
        let by_name = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 2")
            .unwrap();
        assert_eq!(
            by_name.display_rows(),
            vec![vec!["2".to_string(), "y".to_string()]]
        );
        session
            .execute_stmt("DELETE FROM db1.t WHERE id = 3")
            .unwrap();
        let after_delete = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            after_delete.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["2".to_string(), "y".to_string()]
            ]
        );
        session.execute_stmt("DELETE FROM db1.t").unwrap();
        let empty = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert!(empty.display_rows().is_empty());
    }
}

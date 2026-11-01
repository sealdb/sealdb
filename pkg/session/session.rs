use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_executor::{ExecutorBuilder, QueryResult};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::Parser;
use sealdb_parser::ast::Statement;
use sealdb_planner::PlanBuilder;
use sealdb_sessiontxn::TxnManager;
use std::sync::{Arc, Mutex};

pub struct Session {
    parser: Parser,
    planner: PlanBuilder,
    domain: Arc<Domain>,
    current_db: Mutex<String>,
    txn: Mutex<TxnManager>,
}

impl Session {
    pub fn new() -> Self {
        Self::with_domain(Arc::new(Domain::new()))
    }

    pub fn with_domain(domain: Arc<Domain>) -> Self {
        Self {
            parser: Parser::new(),
            planner: PlanBuilder::new(),
            domain,
            current_db: Mutex::new(String::new()),
            txn: Mutex::new(TxnManager::new()),
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

    fn execute_one(&self, stmt: &Statement) -> Result<QueryResult> {
        match stmt {
            Statement::Use(use_stmt) => {
                let is = self.domain.infoschema();
                if is.schema_by_name(&use_stmt.db_name).is_none() {
                    return Err(sealdb_errno::Error::new(
                        sealdb_errno::ERR_BAD_DB,
                        format!("Unknown database '{}'", use_stmt.db_name),
                    ));
                }
                *self.current_db.lock().expect("session current_db lock") =
                    use_stmt.db_name.clone();
                return Ok(empty_result());
            }
            Statement::Begin(begin) => {
                let mut txn = self.txn.lock().expect("session txn lock");
                if begin.pessimistic {
                    txn.enter_begin_pessimistic(self.domain.as_ref())?;
                } else {
                    txn.enter_begin(self.domain.as_ref())?;
                }
                return Ok(empty_result());
            }
            Statement::Commit(_) => {
                self.txn.lock().expect("session txn lock").commit()?;
                return Ok(empty_result());
            }
            Statement::Rollback(_) => {
                self.txn.lock().expect("session txn lock").rollback()?;
                return Ok(empty_result());
            }
            Statement::CreateDatabase(_)
            | Statement::DropDatabase(_)
            | Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::AlterTable(_) => {
                // F12 gap: no transactional DDL. Implicit-commit then apply.
                self.txn.lock().expect("session txn lock").commit()?;
            }
            _ => {}
        }
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        let is = self.domain.infoschema();
        let plan = self
            .planner
            .build_with_schema(stmt, Some(&is), &current_db)?;
        let txn = self
            .txn
            .lock()
            .expect("session txn lock")
            .ensure(self.domain.as_ref())?;
        let exec = ExecutorBuilder::build(&plan, stmt, Arc::clone(&self.domain), &current_db, txn)?;
        let result = ExecutorBuilder::run(exec);
        self.txn
            .lock()
            .expect("session txn lock")
            .finish_autocommit(result.is_ok())?;
        result
    }

    pub fn infoschema(&self) -> InfoSchema {
        self.domain.infoschema()
    }
}

fn empty_result() -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
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

    #[test]
    fn test_agg_and_group_by() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b')",
            )
            .unwrap();
        let count = session.execute_stmt("SELECT COUNT(*) FROM db1.t").unwrap();
        assert_eq!(count.display_rows(), vec![vec!["3".to_string()]]);
        let scalars = session
            .execute_stmt("SELECT SUM(id), MIN(id), MAX(id), AVG(id) FROM db1.t")
            .unwrap();
        assert_eq!(
            scalars.display_rows(),
            vec![vec![
                "6".to_string(),
                "1".to_string(),
                "3".to_string(),
                "2.0".to_string()
            ]]
        );
        let grouped = session
            .execute_stmt("SELECT name, COUNT(*) FROM db1.t GROUP BY name ORDER BY name")
            .unwrap();
        assert_eq!(
            grouped.display_rows(),
            vec![
                vec!["a".to_string(), "2".to_string()],
                vec!["b".to_string(), "1".to_string()]
            ]
        );
        let none = session
            .execute_stmt("SELECT COUNT(*) FROM db1.t WHERE id = 9")
            .unwrap();
        assert_eq!(none.display_rows(), vec![vec!["0".to_string()]]);
        let dual = session.execute_stmt("SELECT COUNT(*)").unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
    }

    #[test]
    fn test_hash_join() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y')",
            )
            .unwrap();
        let inner = session
            .execute_stmt("SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id")
            .unwrap();
        assert_eq!(
            inner.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "x".to_string()]]
        );
        let left = session
            .execute_stmt(
                "SELECT t1.id, t2.tag FROM db1.t1 LEFT JOIN db1.t2 ON t1.id = t2.id ORDER BY t1.id",
            )
            .unwrap();
        assert_eq!(
            left.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["2".to_string(), "NULL".to_string()]
            ]
        );
        let right = session
            .execute_stmt(
                "SELECT t1.name, t2.tag FROM db1.t1 RIGHT JOIN db1.t2 ON t1.id = t2.id ORDER BY t2.tag",
            )
            .unwrap();
        assert_eq!(
            right.display_rows(),
            vec![
                vec!["a".to_string(), "x".to_string()],
                vec!["NULL".to_string(), "y".to_string()]
            ]
        );
    }

    #[test]
    fn test_secondary_index_lookup() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'a')",
            )
            .unwrap();
        let table = session
            .infoschema()
            .table_by_name("db1", "t")
            .unwrap()
            .clone();
        assert_eq!(table.indices.len(), 1);
        assert_eq!(table.indices[0].name.o(), "idx_name");
        let hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            hit.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["3".to_string(), "a".to_string()]
            ]
        );
        session
            .execute_stmt("UPDATE db1.t SET name = 'c' WHERE id = 1")
            .unwrap();
        let after_update = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            after_update.display_rows(),
            vec![vec!["3".to_string(), "a".to_string()]]
        );
        let renamed = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'c'")
            .unwrap();
        assert_eq!(
            renamed.display_rows(),
            vec![vec!["1".to_string(), "c".to_string()]]
        );
        session
            .execute_stmt("DELETE FROM db1.t WHERE name = 'c'")
            .unwrap();
        let after_delete = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'c'")
            .unwrap();
        assert!(after_delete.display_rows().is_empty());
        let miss = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'missing'")
            .unwrap();
        assert!(miss.display_rows().is_empty());
        session
            .execute_stmt("CREATE INDEX idx_id ON db1.t (id)")
            .unwrap();
        assert_eq!(
            session
                .infoschema()
                .table_by_name("db1", "t")
                .unwrap()
                .indices
                .len(),
            2
        );
    }

    #[test]
    fn test_begin_commit_rollback() {
        let session = Session::new();
        session
            .execute_stmt("CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("BEGIN; INSERT INTO db1.t VALUES (1, 'a'); SELECT * FROM db1.t")
            .unwrap();
        session.execute_stmt("ROLLBACK").unwrap();
        let after_rollback = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert!(after_rollback.display_rows().is_empty());
        session
            .execute_stmt("START TRANSACTION; INSERT INTO db1.t VALUES (1, 'a'); COMMIT")
            .unwrap();
        let after_commit = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            after_commit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        session.execute_stmt("COMMIT").unwrap();
        session.execute_stmt("ROLLBACK").unwrap();
        let err = session
            .execute_stmt("START TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap_err();
        assert!(err.message.contains("START TRANSACTION modes"));
    }

    #[test]
    fn test_optimistic_rr_snapshot_and_conflict() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("BEGIN").unwrap();
        let snap = a.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            snap.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        b.execute_stmt("UPDATE db1.t SET name = 'b' WHERE id = 1")
            .unwrap();
        let still = a.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            still.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        a.execute_stmt("UPDATE db1.t SET name = 'c' WHERE id = 1")
            .unwrap();
        let err = a.execute_stmt("COMMIT").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_WRITE_CONFLICT);
        let committed = b.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            committed.display_rows(),
            vec![vec!["1".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_select_for_update_lock() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("BEGIN").unwrap();
        let snap = a.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            snap.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        b.execute_stmt("UPDATE db1.t SET name = 'b' WHERE id = 1")
            .unwrap();
        let locked = a.execute_stmt("SELECT * FROM db1.t FOR UPDATE").unwrap();
        assert_eq!(
            locked.display_rows(),
            vec![vec!["1".to_string(), "b".to_string()]]
        );
        let err = b
            .execute_stmt("SELECT * FROM db1.t FOR UPDATE")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_LOCK_WAIT_TIMEOUT);
        a.execute_stmt("COMMIT").unwrap();
        let after = b.execute_stmt("SELECT * FROM db1.t FOR UPDATE").unwrap();
        assert_eq!(
            after.display_rows(),
            vec![vec!["1".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_pessimistic_update_latest_and_lock() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("BEGIN PESSIMISTIC").unwrap();
        b.execute_stmt("UPDATE db1.t SET name = 'b' WHERE id = 1")
            .unwrap();
        a.execute_stmt("UPDATE db1.t SET name = 'c' WHERE id = 1")
            .unwrap();
        let err = b
            .execute_stmt("BEGIN PESSIMISTIC; UPDATE db1.t SET name = 'd' WHERE id = 1")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_LOCK_WAIT_TIMEOUT);
        b.execute_stmt("ROLLBACK").unwrap();
        a.execute_stmt("COMMIT").unwrap();
        let committed = b.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            committed.display_rows(),
            vec![vec!["1".to_string(), "c".to_string()]]
        );
    }

    #[test]
    fn test_exists_in_and_apply() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y')",
            )
            .unwrap();
        let exists = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            exists.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let not_exists = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE NOT EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            not_exists.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let in_sub = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id IN (SELECT id FROM db1.t2) ORDER BY id")
            .unwrap();
        assert_eq!(
            in_sub.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let in_list = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id IN (2, 9) ORDER BY id")
            .unwrap();
        assert_eq!(
            in_list.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let scalar = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id = (SELECT id FROM db1.t2 WHERE tag = 'x')")
            .unwrap();
        assert_eq!(
            scalar.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let dual = session
            .execute_stmt("SELECT 1 WHERE EXISTS (SELECT * FROM db1.t2 WHERE id = 1)")
            .unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
        let err = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id = (SELECT id FROM db1.t2)")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_SUBQUERY_NO_1_ROW);
        let err = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id = ANY (SELECT id FROM db1.t2)")
            .unwrap_err();
        assert!(err.message.contains("ANY"));
    }

    #[test]
    fn test_union_and_union_all() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, name VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let distinct = session
            .execute_stmt("SELECT 1 UNION SELECT 1 UNION SELECT 2")
            .unwrap();
        assert_eq!(
            distinct.display_rows(),
            vec![vec!["1".to_string()], vec!["2".to_string()]]
        );
        let all = session.execute_stmt("SELECT 1 UNION ALL SELECT 1").unwrap();
        assert_eq!(
            all.display_rows(),
            vec![vec!["1".to_string()], vec!["1".to_string()]]
        );
        let tables = session
            .execute_stmt("SELECT * FROM db1.t1 UNION SELECT * FROM db1.t2 ORDER BY id")
            .unwrap();
        assert_eq!(
            tables.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()],
                vec!["3".to_string(), "c".to_string()]
            ]
        );
        let limited = session
            .execute_stmt("SELECT * FROM db1.t1 UNION ALL SELECT * FROM db1.t2 ORDER BY id LIMIT 2")
            .unwrap();
        assert_eq!(limited.display_rows().len(), 2);
        let err = session
            .execute_stmt("SELECT 1 UNION SELECT 1, 2")
            .unwrap_err();
        assert_eq!(
            err.code,
            sealdb_errno::ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT
        );
        let err = session
            .execute_stmt("SELECT 1 EXCEPT SELECT 2")
            .unwrap_err();
        assert!(err.message.contains("EXCEPT"));
    }

    #[test]
    fn test_window_functions() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'a')",
            )
            .unwrap();
        let numbered = session
            .execute_stmt(
                "SELECT id, name, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) FROM db1.t ORDER BY name, id",
            )
            .unwrap();
        assert_eq!(
            numbered.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string(), "1".to_string()],
                vec!["2".to_string(), "a".to_string(), "2".to_string()],
                vec!["4".to_string(), "a".to_string(), "3".to_string()],
                vec!["3".to_string(), "b".to_string(), "1".to_string()]
            ]
        );
        let ranked = session
            .execute_stmt(
                "SELECT id, RANK() OVER (PARTITION BY name ORDER BY name), DENSE_RANK() OVER (PARTITION BY name ORDER BY name) FROM db1.t ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            ranked.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string(), "1".to_string()],
                vec!["2".to_string(), "1".to_string(), "1".to_string()],
                vec!["3".to_string(), "1".to_string(), "1".to_string()],
                vec!["4".to_string(), "1".to_string(), "1".to_string()]
            ]
        );
        let summed = session
            .execute_stmt(
                "SELECT id, name, SUM(id) OVER (PARTITION BY name) FROM db1.t ORDER BY name, id",
            )
            .unwrap();
        assert_eq!(
            summed.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string(), "7".to_string()],
                vec!["2".to_string(), "a".to_string(), "7".to_string()],
                vec!["4".to_string(), "a".to_string(), "7".to_string()],
                vec!["3".to_string(), "b".to_string(), "3".to_string()]
            ]
        );
        let dual = session.execute_stmt("SELECT ROW_NUMBER() OVER ()").unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
        let err = session
            .execute_stmt("SELECT LAG(id) OVER (ORDER BY id) FROM db1.t")
            .unwrap_err();
        assert!(err.message.contains("window function"));
    }

    #[test]
    fn test_cte() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let dual = session
            .execute_stmt("WITH c AS (SELECT 1 AS id) SELECT * FROM c")
            .unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
        let named = session
            .execute_stmt("WITH c(id) AS (SELECT 2) SELECT id FROM c")
            .unwrap();
        assert_eq!(named.display_rows(), vec![vec!["2".to_string()]]);
        let filtered = session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id < 3) SELECT * FROM c ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            filtered.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let chained = session
            .execute_stmt(
                "WITH a AS (SELECT * FROM db1.t WHERE id = 1), b AS (SELECT * FROM a) SELECT * FROM b",
            )
            .unwrap();
        assert_eq!(
            chained.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let projected = session
            .execute_stmt("WITH c AS (SELECT id FROM db1.t) SELECT * FROM c ORDER BY id")
            .unwrap();
        assert_eq!(
            projected.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
        let counted = session
            .execute_stmt("WITH c AS (SELECT * FROM db1.t) SELECT COUNT(*) FROM c")
            .unwrap();
        assert_eq!(counted.display_rows(), vec![vec!["3".to_string()]]);
        let unioned = session
            .execute_stmt("WITH c AS (SELECT 1 AS id) SELECT * FROM c UNION SELECT 2")
            .unwrap();
        assert_eq!(
            unioned.display_rows(),
            vec![vec!["1".to_string()], vec!["2".to_string()]]
        );
        let err = session
            .execute_stmt("WITH RECURSIVE c AS (SELECT 1) SELECT * FROM c")
            .unwrap_err();
        assert!(err.message.contains("RECURSIVE"));
        let err = session
            .execute_stmt("WITH c AS (SELECT 1 UNION SELECT 2) SELECT * FROM c")
            .unwrap_err();
        assert!(err.message.contains("UNION"));
        let err = session
            .execute_stmt("WITH c(a, b) AS (SELECT 1) SELECT * FROM c")
            .unwrap_err();
        assert_eq!(
            err.code,
            sealdb_errno::ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT
        );
    }

    #[test]
    fn test_online_ddl() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("CREATE INDEX idx_name ON db1.t (name)")
            .unwrap();
        let hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert_eq!(
            hit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        session
            .execute_stmt("ALTER TABLE db1.t ADD COLUMN age INT")
            .unwrap();
        let desc = session.execute_stmt("DESC db1.t").unwrap();
        assert_eq!(desc.display_rows().len(), 3);
        assert_eq!(desc.display_rows()[2][0], "age");
        let existing = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(
            existing.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "NULL".to_string()]]
        );
        session
            .execute_stmt("INSERT INTO db1.t VALUES (3, 'c', 10)")
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t ADD INDEX idx_age (age)")
            .unwrap();
        let aged = session
            .execute_stmt("SELECT * FROM db1.t WHERE age = 10")
            .unwrap();
        assert_eq!(
            aged.display_rows(),
            vec![vec!["3".to_string(), "c".to_string(), "10".to_string()]]
        );
        session
            .execute_stmt("DROP INDEX idx_name ON db1.t")
            .unwrap();
        assert_eq!(
            session
                .infoschema()
                .table_by_name("db1", "t")
                .unwrap()
                .indices
                .len(),
            1
        );
        session
            .execute_stmt("ALTER TABLE db1.t DROP COLUMN age")
            .unwrap_err();
        session
            .execute_stmt("ALTER TABLE db1.t DROP INDEX idx_age")
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t DROP COLUMN age")
            .unwrap();
        let after_drop = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 3")
            .unwrap();
        assert_eq!(
            after_drop.display_rows(),
            vec![vec!["3".to_string(), "c".to_string()]]
        );
        let err = session
            .execute_stmt("CREATE UNIQUE INDEX idx_id ON db1.t (id)")
            .unwrap_err();
        assert!(err.message.contains("UNIQUE"));
        let err = session
            .execute_stmt("ALTER TABLE db1.t CHANGE name title VARCHAR(20)")
            .unwrap_err();
        assert!(err.message.contains("ALTER TABLE"));
    }
}

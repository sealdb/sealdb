use crate::ast::{Node, Statement, StmtNode};
use crate::format::RestoreCtx;
use crate::sqlparser_bridge;
use sealdb_errno::Result;
use serde::Serialize;

pub struct Parser;

impl Parser {
    pub fn new() -> Self {
        Self
    }

    pub fn parse_sql(&self, sql: &str) -> Result<Vec<Statement>> {
        sqlparser_bridge::parse_sql(sql)
    }

    pub fn parse_one_stmt(&self, sql: &str) -> Result<Statement> {
        let mut stmts = self.parse_sql(sql)?;
        stmts
            .pop()
            .ok_or_else(|| sealdb_errno::Error::new(sealdb_errno::ERR_PARSE_ERROR, "empty SQL"))
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize)]
pub struct OracleError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    pub message: String,
}

#[derive(Serialize)]
pub struct OracleStmt {
    #[serde(rename = "type")]
    pub stmt_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Serialize)]
pub struct OracleResult {
    pub sql: String,
    pub statements: Vec<OracleStmt>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OracleError>,
}

pub fn parse_to_oracle_json(sql: &str) -> OracleResult {
    let parser = Parser::new();
    match parser.parse_sql(sql) {
        Ok(stmts) => OracleResult {
            sql: sql.to_string(),
            statements: stmts
                .iter()
                .map(|stmt| {
                    let mut ctx = RestoreCtx::new();
                    let restore = stmt.restore(&mut ctx).ok().map(|_| ctx.into_string());
                    OracleStmt {
                        stmt_type: stmt.stmt_type().to_string(),
                        restore,
                        text: None,
                    }
                })
                .collect(),
            warnings: Vec::new(),
            error: None,
        },
        Err(err) => OracleResult {
            sql: sql.to_string(),
            statements: Vec::new(),
            warnings: Vec::new(),
            error: Some(OracleError {
                code: Some(err.code),
                message: err.message,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_select_one() {
        let out = parse_to_oracle_json("select 1");
        assert!(out.error.is_none());
        assert_eq!(out.statements[0].stmt_type, "SelectStmt");
        assert_eq!(out.statements[0].restore.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn test_parse_create_drop_table() {
        let parser = Parser::new();
        let create = parser
            .parse_one_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        assert_eq!(create.stmt_type(), "CreateTableStmt");
        let drop = parser.parse_one_stmt("DROP TABLE IF EXISTS db1.t").unwrap();
        assert_eq!(drop.stmt_type(), "DropTableStmt");
        let db = parser
            .parse_one_stmt("CREATE DATABASE IF NOT EXISTS db1")
            .unwrap();
        assert_eq!(db.stmt_type(), "CreateDatabaseStmt");
    }

    #[test]
    fn test_parse_show_and_use() {
        let parser = Parser::new();
        assert_eq!(
            parser.parse_one_stmt("SHOW DATABASES").unwrap().stmt_type(),
            "ShowStmt"
        );
        assert_eq!(
            parser
                .parse_one_stmt("SHOW TABLES FROM db1")
                .unwrap()
                .stmt_type(),
            "ShowStmt"
        );
        assert_eq!(
            parser.parse_one_stmt("DESC db1.t").unwrap().stmt_type(),
            "ShowStmt"
        );
        assert_eq!(
            parser.parse_one_stmt("USE db1").unwrap().stmt_type(),
            "UseStmt"
        );
    }

    #[test]
    fn test_parse_insert_and_select_star() {
        let parser = Parser::new();
        assert_eq!(
            parser
                .parse_one_stmt("INSERT INTO db1.t VALUES (1, 'a')")
                .unwrap()
                .stmt_type(),
            "InsertStmt"
        );
        assert_eq!(
            parser
                .parse_one_stmt("SELECT * FROM db1.t")
                .unwrap()
                .stmt_type(),
            "SelectStmt"
        );
        let where_stmt = parser
            .parse_one_stmt("SELECT * FROM db1.t WHERE id = 1")
            .unwrap();
        assert!(where_stmt.as_select().unwrap().selection.is_some());
    }

    #[test]
    fn test_parse_update_and_delete() {
        let parser = Parser::new();
        assert_eq!(
            parser
                .parse_one_stmt("UPDATE db1.t SET name = 'c' WHERE id = 1")
                .unwrap()
                .stmt_type(),
            "UpdateStmt"
        );
        assert_eq!(
            parser
                .parse_one_stmt("DELETE FROM db1.t WHERE id = 2")
                .unwrap()
                .stmt_type(),
            "DeleteStmt"
        );
    }

    #[test]
    fn test_parse_group_by_and_count() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("SELECT name, COUNT(*) FROM db1.t GROUP BY name")
            .unwrap();
        let select = stmt.as_select().unwrap();
        assert_eq!(select.group_by.len(), 1);
        match &select.fields[1].expr {
            crate::ast::expressions::ExprNode::Func(func) => {
                assert!(func.name.eq_ignore_ascii_case("COUNT"));
                assert!(func.args.is_empty());
            }
            other => panic!("expected COUNT(*), got {other:?}"),
        }
    }

    #[test]
    fn test_parse_join() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("SELECT * FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id")
            .unwrap();
        let join = stmt.as_select().unwrap().join.as_ref().unwrap();
        assert_eq!(join.left.name, "t1");
        assert_eq!(join.right.name, "t2");
        assert_eq!(join.tp, crate::ast::JoinType::Inner);
    }

    #[test]
    fn test_parse_begin_commit_rollback() {
        let parser = Parser::new();
        assert_eq!(
            parser.parse_one_stmt("BEGIN").unwrap().stmt_type(),
            "BeginStmt"
        );
        assert_eq!(
            parser
                .parse_one_stmt("START TRANSACTION")
                .unwrap()
                .stmt_type(),
            "BeginStmt"
        );
        assert_eq!(
            parser.parse_one_stmt("COMMIT").unwrap().stmt_type(),
            "CommitStmt"
        );
        assert_eq!(
            parser.parse_one_stmt("ROLLBACK").unwrap().stmt_type(),
            "RollbackStmt"
        );
        let err = parser.parse_one_stmt("ROLLBACK TO sp").unwrap_err();
        assert!(err.message.contains("ROLLBACK TO SAVEPOINT"));
    }

    #[test]
    fn test_parse_for_update_and_begin_pessimistic() {
        let parser = Parser::new();
        let select = parser.parse_one_stmt("SELECT * FROM t FOR UPDATE").unwrap();
        assert_eq!(
            select.as_select().unwrap().lock,
            crate::ast::SelectLock::ForUpdate
        );
        match parser.parse_one_stmt("BEGIN PESSIMISTIC").unwrap() {
            crate::ast::Statement::Begin(begin) => assert!(begin.pessimistic),
            other => panic!("expected BeginStmt, got {other:?}"),
        }
        let mixed = parser.parse_sql("BEGIN; BEGIN PESSIMISTIC").unwrap();
        match (&mixed[0], &mixed[1]) {
            (crate::ast::Statement::Begin(first), crate::ast::Statement::Begin(second)) => {
                assert!(!first.pessimistic);
                assert!(second.pessimistic);
            }
            other => panic!("expected two BeginStmts, got {other:?}"),
        }
        match parser
            .parse_one_stmt("START TRANSACTION PESSIMISTIC")
            .unwrap()
        {
            crate::ast::Statement::Begin(begin) => assert!(begin.pessimistic),
            other => panic!("expected BeginStmt, got {other:?}"),
        }
        let err = parser
            .parse_one_stmt("SELECT * FROM t FOR SHARE")
            .unwrap_err();
        assert!(err.message.contains("FOR SHARE"));
        let err = parser
            .parse_one_stmt("SELECT * FROM t FOR UPDATE NOWAIT")
            .unwrap_err();
        assert!(err.message.contains("FOR UPDATE NOWAIT"));
        let err = parser
            .parse_one_stmt("SELECT * FROM t FOR UPDATE SKIP LOCKED")
            .unwrap_err();
        assert!(err.message.contains("FOR UPDATE SKIP LOCKED"));
    }

    #[test]
    fn test_parse_create_table_index() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name))")
            .unwrap();
        match stmt {
            crate::ast::Statement::CreateTable(create) => {
                assert_eq!(create.indices.len(), 1);
                assert_eq!(create.indices[0].name, "idx_name");
                assert_eq!(create.indices[0].columns, vec!["name".to_string()]);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        let err = parser
            .parse_one_stmt("CREATE INDEX idx_name ON db1.t (name)")
            .unwrap_err();
        assert!(err.message.contains("CREATE INDEX"));
    }

    #[test]
    fn test_parse_exists_in_subquery() {
        let parser = Parser::new();
        let exists = parser
            .parse_one_stmt("SELECT * FROM t1 WHERE EXISTS (SELECT 1 FROM t2)")
            .unwrap();
        assert!(matches!(
            exists.as_select().unwrap().selection.as_ref(),
            Some(crate::ast::ExprNode::Exists(_))
        ));
        let in_sub = parser
            .parse_one_stmt("SELECT * FROM t1 WHERE id IN (SELECT id FROM t2)")
            .unwrap();
        assert!(matches!(
            in_sub.as_select().unwrap().selection.as_ref(),
            Some(crate::ast::ExprNode::InSubquery(_))
        ));
        let in_list = parser
            .parse_one_stmt("SELECT * FROM t1 WHERE id IN (1, 2)")
            .unwrap();
        assert!(matches!(
            in_list.as_select().unwrap().selection.as_ref(),
            Some(crate::ast::ExprNode::InList(_))
        ));
        let err = parser
            .parse_one_stmt("SELECT * FROM t1 WHERE id = ANY (SELECT id FROM t2)")
            .unwrap_err();
        assert!(err.message.contains("ANY"));
    }

    #[test]
    fn test_parse_union() {
        let parser = Parser::new();
        match parser.parse_one_stmt("SELECT 1 UNION SELECT 2").unwrap() {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest.len(), 1);
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::Union);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SELECT 1 UNION ALL SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::UnionAll);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
        let err = parser
            .parse_one_stmt("SELECT 1 EXCEPT SELECT 2")
            .unwrap_err();
        assert!(err.message.contains("EXCEPT"));
    }

    #[test]
    fn test_parse_window() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) FROM t")
            .unwrap();
        assert!(matches!(
            stmt.as_select().unwrap().fields[1].expr,
            crate::ast::ExprNode::Window(_)
        ));
        let err = parser
            .parse_one_stmt("SELECT LAG(id) OVER (ORDER BY id) FROM t")
            .unwrap_err();
        assert!(err.message.contains("window function"));
        let err = parser
            .parse_one_stmt(
                "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
            )
            .unwrap_err();
        assert!(err.message.contains("frame"));
    }

    #[test]
    fn test_parse_cte() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("WITH c AS (SELECT 1 AS id) SELECT * FROM c")
            .unwrap();
        let select = stmt.as_select().unwrap();
        let with = select.with.as_ref().expect("with clause");
        assert_eq!(with.ctes.len(), 1);
        assert_eq!(with.ctes[0].name, "c");
        assert!(with.ctes[0].col_name_list.is_empty());
        let named = parser
            .parse_one_stmt("WITH c(id) AS (SELECT 1) SELECT id FROM c")
            .unwrap();
        assert_eq!(
            named.as_select().unwrap().with.as_ref().unwrap().ctes[0].col_name_list,
            vec!["id".to_string()]
        );
        let err = parser
            .parse_one_stmt(
                "WITH RECURSIVE c AS (SELECT 1 UNION SELECT id + 1 FROM c) SELECT * FROM c",
            )
            .unwrap_err();
        assert!(err.message.contains("RECURSIVE"));
        let err = parser
            .parse_one_stmt("WITH c AS (SELECT 1 UNION SELECT 2) SELECT * FROM c")
            .unwrap_err();
        assert!(err.message.contains("UNION"));
    }
}

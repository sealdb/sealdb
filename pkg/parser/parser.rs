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
}

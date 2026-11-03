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
    fn test_parse_truncate_table() {
        let parser = Parser::new();
        match parser.parse_one_stmt("TRUNCATE TABLE db1.t").unwrap() {
            crate::ast::Statement::TruncateTable(stmt) => {
                assert_eq!(stmt.table.schema.as_deref(), Some("db1"));
                assert_eq!(stmt.table.name, "t");
                let mut ctx = crate::format::RestoreCtx::new();
                stmt.restore(&mut ctx).unwrap();
                assert_eq!(ctx.into_string(), "TRUNCATE TABLE `db1`.`t`");
            }
            other => panic!("expected TruncateTableStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("TRUNCATE db1.t").unwrap() {
            crate::ast::Statement::TruncateTable(stmt) => {
                assert_eq!(stmt.table.name, "t");
            }
            other => panic!("expected TruncateTableStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_c6_admin_ops() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("BACKUP DATABASE * TO 's3://bucket/path'")
            .unwrap()
        {
            crate::ast::Statement::BRIE(_) => {}
            other => panic!("expected BRIEStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("IMPORT INTO t FROM '/tmp/a.csv'")
            .unwrap()
        {
            crate::ast::Statement::ImportInto(_) => {}
            other => panic!("expected ImportIntoStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SPLIT TABLE t BETWEEN (1) AND (100) REGIONS 4")
            .unwrap()
        {
            crate::ast::Statement::SplitRegion(_) => {}
            other => panic!("expected SplitRegionStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("DISTRIBUTE TABLE t RULE = 'key'")
            .unwrap()
        {
            crate::ast::Statement::DistributeTable(_) => {}
            other => panic!("expected DistributeTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE t COMPACT TIFLASH REPLICA")
            .unwrap()
        {
            crate::ast::Statement::CompactTable(_) => {}
            other => panic!("expected CompactTableStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("RECOMMEND INDEX RUN").unwrap() {
            crate::ast::Statement::RecommendIndex(_) => {}
            other => panic!("expected RecommendIndexStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CALIBRATE RESOURCE WORKLOAD TPCC")
            .unwrap()
        {
            crate::ast::Statement::CalibrateResource(_) => {}
            other => panic!("expected CalibrateResourceStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_trace_and_plan_replayer() {
        let parser = Parser::new();
        match parser.parse_one_stmt("TRACE SELECT 1").unwrap() {
            crate::ast::Statement::Trace(stmt) => {
                assert!(!stmt.trace_plan);
                assert!(stmt.stmt_sql.to_ascii_uppercase().contains("SELECT"));
            }
            other => panic!("expected TraceStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("TRACE PLAN FORMAT = 'json' SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::Trace(stmt) => {
                assert!(stmt.trace_plan);
                assert_eq!(stmt.format.to_ascii_lowercase(), "json");
            }
            other => panic!("expected TraceStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("PLAN REPLAYER LOAD '/tmp/plan.zip'")
            .unwrap()
        {
            crate::ast::Statement::PlanReplayer(stmt) => {
                assert!(stmt.load);
                assert_eq!(stmt.file, "/tmp/plan.zip");
            }
            other => panic!("expected PlanReplayerStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("PLAN REPLAYER CAPTURE 'sql' 'plan'")
            .unwrap()
        {
            crate::ast::Statement::PlanReplayer(stmt) => {
                assert!(stmt.capture);
                assert_eq!(stmt.sql_digest, "sql");
                assert_eq!(stmt.plan_digest, "plan");
            }
            other => panic!("expected PlanReplayerStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_stats_lock_load() {
        let parser = Parser::new();
        match parser.parse_one_stmt("LOAD STATS '/tmp/s.json'").unwrap() {
            crate::ast::Statement::LoadStats(stmt) => assert_eq!(stmt.path, "/tmp/s.json"),
            other => panic!("expected LoadStatsStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("LOCK STATS db1.t, db1.u").unwrap() {
            crate::ast::Statement::LockStats(stmt) => assert_eq!(stmt.tables.len(), 2),
            other => panic!("expected LockStatsStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("UNLOCK STATS db1.t").unwrap() {
            crate::ast::Statement::UnlockStats(stmt) => assert_eq!(stmt.tables.len(), 1),
            other => panic!("expected UnlockStatsStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_load_data() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt(
                "LOAD DATA LOCAL INFILE '/tmp/x.csv' INTO TABLE db1.t FIELDS TERMINATED BY ',' LINES TERMINATED BY '\\n' IGNORE 1 LINES (id, name)",
            )
            .unwrap()
        {
            crate::ast::Statement::LoadData(stmt) => {
                assert!(stmt.local);
                assert_eq!(stmt.path, "/tmp/x.csv");
                assert_eq!(stmt.table.name, "t");
                assert_eq!(stmt.fields_terminated_by, ",");
                assert_eq!(stmt.lines_terminated_by, "\n");
                assert_eq!(stmt.ignore_lines, 1);
                assert_eq!(stmt.columns, vec!["id".to_string(), "name".to_string()]);
            }
            other => panic!("expected LoadDataStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_create_drop_alter_sequence() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt(
                "CREATE SEQUENCE IF NOT EXISTS db1.s INCREMENT BY 1 START WITH 10 CACHE 20 NOCYCLE",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateSequence(stmt) => {
                assert!(stmt.if_not_exists);
                assert_eq!(stmt.name.schema.as_deref(), Some("db1"));
                assert_eq!(stmt.name.name, "s");
                assert_eq!(stmt.options.increment, Some(1));
                assert_eq!(stmt.options.start, Some(10));
                assert_eq!(stmt.options.cache, Some(20));
                assert_eq!(stmt.options.cycle, Some(false));
            }
            other => panic!("expected CreateSequenceStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("DROP SEQUENCE IF EXISTS db1.s, db1.t")
            .unwrap()
        {
            crate::ast::Statement::DropSequence(stmt) => {
                assert!(stmt.if_exists);
                assert_eq!(stmt.sequences.len(), 2);
            }
            other => panic!("expected DropSequenceStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER SEQUENCE db1.s INCREMENT BY 2 RESTART WITH 5")
            .unwrap()
        {
            crate::ast::Statement::AlterSequence(stmt) => {
                assert_eq!(stmt.options.increment, Some(2));
                assert_eq!(stmt.options.start, Some(5));
            }
            other => panic!("expected AlterSequenceStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_create_drop_view() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("CREATE VIEW db1.v AS SELECT id FROM db1.t")
            .unwrap()
        {
            crate::ast::Statement::CreateView(stmt) => {
                assert!(!stmt.or_replace);
                assert_eq!(stmt.view_name.schema.as_deref(), Some("db1"));
                assert_eq!(stmt.view_name.name, "v");
                assert_eq!(stmt.algorithm, "UNDEFINED");
                assert!(!stmt.select.text.is_empty());
            }
            other => panic!("expected CreateViewStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE OR REPLACE VIEW v (a) AS SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::CreateView(stmt) => {
                assert!(stmt.or_replace);
                assert_eq!(stmt.cols, vec!["a".to_string()]);
            }
            other => panic!("expected CreateViewStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("DROP VIEW IF EXISTS db1.v").unwrap() {
            crate::ast::Statement::DropTable(stmt) => {
                assert!(stmt.is_view);
                assert!(stmt.if_exists);
                assert_eq!(stmt.tables[0].name, "v");
            }
            other => panic!("expected DropTableStmt(is_view), got {other:?}"),
        }
    }

    #[test]
    fn test_parse_do_flush_setpwd_shutdown_alter_db() {
        let parser = Parser::new();
        match parser.parse_one_stmt("DO 1, 2 + 3").unwrap() {
            crate::ast::Statement::Do(stmt) => {
                assert_eq!(stmt.exprs.len(), 2);
            }
            other => panic!("expected DoStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("FLUSH PRIVILEGES").unwrap() {
            crate::ast::Statement::Flush(stmt) => {
                assert_eq!(stmt.tp, crate::ast::FlushStmtType::Privileges);
            }
            other => panic!("expected FlushStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("FLUSH TABLES").unwrap() {
            crate::ast::Statement::Flush(stmt) => {
                assert_eq!(stmt.tp, crate::ast::FlushStmtType::Tables);
            }
            other => panic!("expected FlushStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SET PASSWORD FOR 'u'@'%' = 'secret'")
            .unwrap()
        {
            crate::ast::Statement::SetPwd(stmt) => {
                let user = stmt.user.expect("FOR user");
                assert_eq!(user.username, "u");
                assert_eq!(stmt.password, "secret");
            }
            other => panic!("expected SetPwdStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SET PASSWORD = 'x'").unwrap() {
            crate::ast::Statement::SetPwd(stmt) => {
                assert!(stmt.user.is_none());
                assert_eq!(stmt.password, "x");
            }
            other => panic!("expected SetPwdStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SHUTDOWN").unwrap() {
            crate::ast::Statement::Shutdown(_) => {}
            other => panic!("expected ShutdownStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER DATABASE db1 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin")
            .unwrap()
        {
            crate::ast::Statement::AlterDatabase(stmt) => {
                assert_eq!(stmt.name, "db1");
                assert_eq!(stmt.charset.as_deref(), Some("utf8mb4"));
                assert_eq!(stmt.collate.as_deref(), Some("utf8mb4_bin"));
            }
            other => panic!("expected AlterDatabaseStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_allowlist_misc_kill_lock_optimize_repair_rename_binlog_change_alter_instance() {
        let parser = Parser::new();
        match parser.parse_one_stmt("KILL QUERY 42").unwrap() {
            crate::ast::Statement::Kill(stmt) => {
                assert!(stmt.query);
                assert_eq!(stmt.connection_id, 42);
            }
            other => panic!("expected KillStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("LOCK TABLES db1.t READ, t2 WRITE")
            .unwrap()
        {
            crate::ast::Statement::LockTables(stmt) => {
                assert_eq!(stmt.table_locks.len(), 2);
                assert_eq!(stmt.table_locks[0].table.schema.as_deref(), Some("db1"));
                assert!(matches!(
                    stmt.table_locks[0].lock_type,
                    crate::ast::TableLockType::Read
                ));
                assert!(matches!(
                    stmt.table_locks[1].lock_type,
                    crate::ast::TableLockType::Write
                ));
            }
            other => panic!("expected LockTablesStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("UNLOCK TABLES").unwrap() {
            crate::ast::Statement::CleanupTableLock(stmt) => {
                assert!(stmt.tables.is_empty());
            }
            other => panic!("expected CleanupTableLockStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ADMIN CLEANUP TABLE LOCK db1.t")
            .unwrap()
        {
            crate::ast::Statement::CleanupTableLock(stmt) => {
                assert_eq!(stmt.tables.len(), 1);
                assert_eq!(stmt.tables[0].name, "t");
            }
            other => panic!("expected CleanupTableLockStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("OPTIMIZE TABLE db1.t").unwrap() {
            crate::ast::Statement::OptimizeTable(stmt) => {
                assert_eq!(stmt.tables.len(), 1);
                assert_eq!(stmt.tables[0].name, "t");
            }
            other => panic!("expected OptimizeTableStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("REPAIR TABLE db1.t").unwrap() {
            crate::ast::Statement::RepairTable(stmt) => {
                assert_eq!(stmt.tables[0].name, "t");
            }
            other => panic!("expected RepairTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("RENAME USER 'u'@'%' TO 'v'@'%'")
            .unwrap()
        {
            crate::ast::Statement::RenameUser(stmt) => {
                assert_eq!(stmt.user_to_users.len(), 1);
                assert_eq!(stmt.user_to_users[0].old_user.username, "u");
                assert_eq!(stmt.user_to_users[0].new_user.username, "v");
            }
            other => panic!("expected RenameUserStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("BINLOG 'abc'").unwrap() {
            crate::ast::Statement::Binlog(stmt) => assert_eq!(stmt.payload, "abc"),
            other => panic!("expected BinlogStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CHANGE PUMP TO NODE_STATE = 'offline' FOR NODE_ID 'n1'")
            .unwrap()
        {
            crate::ast::Statement::Change(stmt) => {
                assert_eq!(stmt.node_type.to_ascii_uppercase(), "PUMP");
                assert_eq!(stmt.state, "offline");
                assert_eq!(stmt.node_id, "n1");
            }
            other => panic!("expected ChangeStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER INSTANCE RELOAD TLS NO ROLLBACK ON ERROR")
            .unwrap()
        {
            crate::ast::Statement::AlterInstance(stmt) => {
                assert!(stmt.reload_tls);
                assert!(stmt.no_rollback_on_error);
            }
            other => panic!("expected AlterInstanceStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_create_table_like_as_select() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("CREATE TABLE db1.t2 LIKE db1.t1")
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                assert_eq!(create.table.name, "t2");
                let refer = create.refer_table.as_ref().expect("LIKE source");
                assert_eq!(refer.name, "t1");
                assert!(create.cols.is_empty());
                assert!(create.select.is_none());
                let mut ctx = crate::format::RestoreCtx::new();
                create.restore(&mut ctx).unwrap();
                assert_eq!(ctx.into_string(), "CREATE TABLE `db1`.`t2` LIKE `db1`.`t1`");
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE TABLE db1.t2 AS SELECT id, name FROM db1.t1")
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                assert!(create.refer_table.is_none());
                let select = create.select.expect("AS SELECT");
                assert_eq!(select.fields.len(), 2);
                assert_eq!(select.from.as_ref().unwrap().name, "t1");
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        let err = parser
            .parse_one_stmt("CREATE TEMPORARY TABLE db1.t (id INT)")
            .unwrap_err();
        assert!(err.message.contains("TEMPORARY"));
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
        match parser
            .parse_one_stmt("SHOW FULL COLUMNS FROM db1.t")
            .unwrap()
        {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::Columns);
                assert!(show.full);
                assert_eq!(show.table.unwrap().name, "t");
            }
            other => panic!("expected SHOW FULL COLUMNS, got {other:?}"),
        }
        match parser.parse_one_stmt("SHOW CREATE TABLE db1.t").unwrap() {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::CreateTable);
                assert_eq!(show.table.unwrap().name, "t");
            }
            other => panic!("expected SHOW CREATE TABLE, got {other:?}"),
        }
        match parser.parse_one_stmt("SHOW CREATE DATABASE db1").unwrap() {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::CreateDatabase);
                assert_eq!(show.db_name.as_deref(), Some("db1"));
            }
            other => panic!("expected SHOW CREATE DATABASE, got {other:?}"),
        }
        match parser.parse_one_stmt("SHOW INDEX FROM db1.t").unwrap() {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::Index);
                let table = show.table.unwrap();
                assert_eq!(table.schema.as_deref(), Some("db1"));
                assert_eq!(table.name, "t");
            }
            other => panic!("expected SHOW INDEX, got {other:?}"),
        }
        assert_eq!(
            parser.parse_one_stmt("SHOW CHARSET").unwrap().stmt_type(),
            "ShowStmt"
        );
        assert_eq!(
            parser.parse_one_stmt("SHOW COLLATION").unwrap().stmt_type(),
            "ShowStmt"
        );
        match parser.parse_one_stmt("ADMIN SHOW DDL JOBS").unwrap() {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::DdlJobs);
            }
            other => panic!("expected ADMIN SHOW DDL JOBS, got {other:?}"),
        }
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
        match parser
            .parse_one_stmt("INSERT INTO db1.t SELECT * FROM db1.src")
            .unwrap()
        {
            crate::ast::Statement::Insert(insert) => {
                assert!(insert.select.is_some());
                assert!(insert.rows.is_empty());
            }
            other => panic!("expected InsertStmt, got {other:?}"),
        }
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
        match parser
            .parse_one_stmt("UPDATE db1.t SET name = 'z' ORDER BY id DESC LIMIT 1")
            .unwrap()
        {
            crate::ast::Statement::Update(update) => {
                assert_eq!(update.order_by.len(), 1);
                assert!(update.order_by[0].desc);
                assert_eq!(update.limit, Some(1));
            }
            other => panic!("expected UpdateStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("DELETE FROM db1.t ORDER BY id LIMIT 1")
            .unwrap()
        {
            crate::ast::Statement::Delete(delete) => {
                assert_eq!(delete.order_by.len(), 1);
                assert!(!delete.order_by[0].desc);
                assert_eq!(delete.limit, Some(1));
            }
            other => panic!("expected DeleteStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.name = t2.name")
            .unwrap()
        {
            crate::ast::Statement::Update(update) => {
                assert!(update.join.is_some());
                assert_eq!(update.list[0].table.as_deref(), Some("t1"));
                assert_eq!(update.list[0].column, "name");
            }
            other => panic!("expected UpdateStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id")
            .unwrap()
        {
            crate::ast::Statement::Delete(delete) => {
                assert_eq!(delete.tables.len(), 1);
                assert!(delete.join.is_some());
            }
            other => panic!("expected DeleteStmt, got {other:?}"),
        }
        assert!(
            parser
                .parse_one_stmt("UPDATE t SET name = 'x' RETURNING name")
                .is_err()
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
        let having = parser
            .parse_one_stmt("SELECT name, COUNT(*) FROM db1.t GROUP BY name HAVING COUNT(*) > 1")
            .unwrap();
        assert!(having.as_select().unwrap().having.is_some());
        match parser
            .parse_one_stmt("SELECT COUNT(DISTINCT name) FROM t")
            .unwrap()
        {
            crate::ast::Statement::Select(select) => match &select.fields[0].expr {
                crate::ast::expressions::ExprNode::Func(func) => {
                    assert!(func.name.eq_ignore_ascii_case("COUNT"));
                    assert!(func.distinct);
                }
                other => panic!("expected COUNT DISTINCT, got {other:?}"),
            },
            other => panic!("expected SelectStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_join() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("SELECT * FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id")
            .unwrap();
        let join = &stmt.as_select().unwrap().joins[0];
        assert_eq!(join.left.name, "t1");
        assert_eq!(join.right.name, "t2");
        assert_eq!(join.tp, crate::ast::JoinType::Inner);
        let cross = parser
            .parse_one_stmt("SELECT * FROM db1.t1 CROSS JOIN db1.t2")
            .unwrap();
        assert_eq!(
            cross.as_select().unwrap().joins[0].tp,
            crate::ast::JoinType::Cross
        );
        let comma = parser
            .parse_one_stmt("SELECT * FROM db1.t1, db1.t2")
            .unwrap();
        assert_eq!(
            comma.as_select().unwrap().joins[0].tp,
            crate::ast::JoinType::Cross
        );
        let three = parser
            .parse_one_stmt(
                "SELECT * FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id JOIN db1.t3 ON t2.id = t3.id",
            )
            .unwrap();
        assert_eq!(three.as_select().unwrap().joins.len(), 2);
        let semi =
            parser.parse_one_stmt("SELECT * FROM db1.t1 LEFT SEMI JOIN db1.t2 ON t1.id = t2.id");
        match semi {
            Ok(stmt) => {
                assert_eq!(
                    stmt.as_select().unwrap().joins[0].tp,
                    crate::ast::JoinType::Semi
                );
            }
            Err(err) => {
                // MySQL dialect may reject the non-standard SEMI keyword;
                // EXISTS/IN rewrite is the user-visible path in that case.
                assert!(
                    err.message.contains("SEMI")
                        || err.message.contains("syntax")
                        || err.message.contains("Expected"),
                    "{err:?}"
                );
            }
        }
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
        let to_sp = parser.parse_one_stmt("ROLLBACK TO sp").unwrap();
        match to_sp {
            crate::ast::Statement::Rollback(rb) => {
                assert_eq!(rb.savepoint.as_deref(), Some("sp"));
                assert!(!rb.chain);
            }
            other => panic!("expected RollbackStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("COMMIT AND CHAIN").unwrap() {
            crate::ast::Statement::Commit(c) => assert!(c.chain),
            other => panic!("expected CommitStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("ROLLBACK AND CHAIN").unwrap() {
            crate::ast::Statement::Rollback(rb) => {
                assert!(rb.chain);
                assert!(rb.savepoint.is_none());
            }
            other => panic!("expected RollbackStmt, got {other:?}"),
        }
        assert_eq!(
            parser.parse_one_stmt("SAVEPOINT sp").unwrap().stmt_type(),
            "SavepointStmt"
        );
        assert_eq!(
            parser
                .parse_one_stmt("RELEASE SAVEPOINT sp")
                .unwrap()
                .stmt_type(),
            "ReleaseSavepointStmt"
        );
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
        let share = parser.parse_one_stmt("SELECT * FROM t FOR SHARE").unwrap();
        assert_eq!(
            share.as_select().unwrap().lock,
            crate::ast::SelectLock::ForShare
        );
        let share_mode = parser
            .parse_one_stmt("SELECT * FROM t LOCK IN SHARE MODE")
            .unwrap();
        assert_eq!(
            share_mode.as_select().unwrap().lock,
            crate::ast::SelectLock::ForShare
        );
        let nowait = parser
            .parse_one_stmt("SELECT * FROM t FOR UPDATE NOWAIT")
            .unwrap();
        assert_eq!(
            nowait.as_select().unwrap().lock,
            crate::ast::SelectLock::ForUpdate
        );
        assert_eq!(
            nowait.as_select().unwrap().lock_wait,
            crate::ast::SelectLockWait::Nowait
        );
        let skip = parser
            .parse_one_stmt("SELECT * FROM t FOR UPDATE SKIP LOCKED")
            .unwrap();
        assert_eq!(
            skip.as_select().unwrap().lock_wait,
            crate::ast::SelectLockWait::SkipLocked
        );
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
                assert!(!create.indices[0].unique);
                assert!(!create.indices[0].primary);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE INDEX idx_name ON db1.t (name)")
            .unwrap()
        {
            crate::ast::Statement::CreateIndex(create) => {
                assert_eq!(create.name, "idx_name");
                assert_eq!(create.columns, vec!["name".to_string()]);
                assert!(!create.unique);
            }
            other => panic!("expected CreateIndexStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("DROP INDEX idx_name ON db1.t")
            .unwrap()
        {
            crate::ast::Statement::DropIndex(drop) => {
                assert_eq!(drop.name, "idx_name");
            }
            other => panic!("expected DropIndexStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t ADD COLUMN age INT, DROP INDEX idx_name")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => {
                assert_eq!(alter.specs.len(), 2);
            }
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE UNIQUE INDEX idx_name ON db1.t (name)")
            .unwrap()
        {
            crate::ast::Statement::CreateIndex(create) => assert!(create.unique),
            other => panic!("expected unique CreateIndexStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t ADD UNIQUE INDEX idx_u (name)")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::AddIndex(idx) => assert!(idx.unique),
                other => panic!("expected ADD UNIQUE INDEX, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE db1.t (id INT, name VARCHAR(20), PRIMARY KEY (id), UNIQUE INDEX idx_name (name))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                assert!(create.indices.iter().any(|idx| idx.primary));
                assert!(create.indices.iter().any(|idx| idx.unique && !idx.primary));
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t CHANGE name title VARCHAR(20)")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::ChangeColumn {
                    old_name, new_name, ..
                } => {
                    assert_eq!(old_name, "name");
                    assert_eq!(new_name, "title");
                }
                other => panic!("expected CHANGE COLUMN, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t MODIFY title VARCHAR(40)")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::ModifyColumn { name, .. } => {
                    assert_eq!(name, "title");
                }
                other => panic!("expected MODIFY COLUMN, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t RENAME COLUMN title TO name")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::RenameColumn { old_name, new_name } => {
                    assert_eq!(old_name, "title");
                    assert_eq!(new_name, "name");
                }
                other => panic!("expected RENAME COLUMN, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t RENAME INDEX idx_name TO idx_title")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::RenameIndex { old_name, new_name } => {
                    assert_eq!(old_name, "idx_name");
                    assert_eq!(new_name, "idx_title");
                }
                other => panic!("expected RENAME INDEX, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t RENAME TO t2")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::RenameTable { new_name } => {
                    assert_eq!(new_name.name, "t2");
                }
                other => panic!("expected RENAME TABLE, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("RENAME TABLE db1.t TO db1.t2")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::RenameTable { new_name } => {
                    assert_eq!(new_name.name, "t2");
                }
                other => panic!("expected RENAME TABLE, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t ADD COLUMN age INT AFTER name")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::AddColumn { position, .. } => {
                    assert!(matches!(
                        position,
                        crate::ast::ColumnPosition::After(name) if name == "name"
                    ));
                }
                other => panic!("expected ADD COLUMN AFTER, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t ADD COLUMN z INT FIRST")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[0] {
                crate::ast::AlterTableSpec::AddColumn { position, .. } => {
                    assert_eq!(*position, crate::ast::ColumnPosition::First);
                }
                other => panic!("expected ADD COLUMN FIRST, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE db1.t ADD COLUMN age INT, ALGORITHM=INPLACE, LOCK=NONE")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => {
                assert!(matches!(
                    &alter.specs[1],
                    crate::ast::AlterTableSpec::Algorithm(name) if name == "INPLACE"
                ));
                assert!(matches!(
                    &alter.specs[2],
                    crate::ast::AlterTableSpec::Lock(name) if name == "NONE"
                ));
            }
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE INDEX idx_age ON db1.t (age) ALGORITHM=INPLACE LOCK=NONE")
            .unwrap()
        {
            crate::ast::Statement::CreateIndex(create) => assert_eq!(create.name, "idx_age"),
            other => panic!("expected CreateIndexStmt, got {other:?}"),
        }
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
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t WHERE id BETWEEN 1 AND 2")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::Between(_))
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t WHERE name LIKE 'a%'")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::Like(_))
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t WHERE name IS NULL")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::IsNull(_))
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t1 WHERE id = ANY (SELECT id FROM t2)")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::InSubquery(_))
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t1 WHERE id = SOME (SELECT id FROM t2)")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::InSubquery(_))
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t1 WHERE id <> ALL (SELECT id FROM t2)")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::InSubquery(s)) if s.not
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t1 WHERE id > ANY (SELECT id FROM t2)")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::QuantifiedCmp(q)) if !q.all
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT * FROM t1 WHERE id > ALL (SELECT id FROM t2)")
                .unwrap()
                .as_select()
                .unwrap()
                .selection
                .as_ref(),
            Some(crate::ast::ExprNode::QuantifiedCmp(q)) if q.all
        ));
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT id, (SELECT tag FROM t2 WHERE t2.id = t1.id) FROM t1")
                .unwrap()
                .as_select()
                .unwrap()
                .fields[1]
                .expr,
            crate::ast::ExprNode::ScalarSubquery(_)
        ));
        match &parser
            .parse_one_stmt("SELECT (SELECT 1 UNION SELECT 1)")
            .unwrap()
            .as_select()
            .unwrap()
            .fields[0]
            .expr
        {
            crate::ast::ExprNode::ScalarSubquery(s) => {
                assert!(matches!(
                    s.query.as_ref(),
                    crate::ast::ResultSetNode::SetOpr(_)
                ));
            }
            other => panic!("expected ScalarSubquery, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SELECT * FROM t1 WHERE EXISTS (SELECT 1 UNION SELECT 2)")
            .unwrap()
            .as_select()
            .unwrap()
            .selection
            .as_ref()
        {
            Some(crate::ast::ExprNode::Exists(e)) => {
                assert!(matches!(
                    e.query.as_ref(),
                    crate::ast::ResultSetNode::SetOpr(_)
                ));
            }
            other => panic!("expected Exists, got {other:?}"),
        }
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
        match parser.parse_one_stmt("SELECT 1 EXCEPT SELECT 2").unwrap() {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::Except);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SELECT 1 INTERSECT SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::Intersect);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SELECT 1 UNION (SELECT 2 UNION SELECT 3)")
            .unwrap()
        {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest.len(), 2);
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::Union);
                assert_eq!(set.rest[1].0, crate::ast::SetOprType::Union);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("(SELECT 1) UNION (SELECT 2)")
            .unwrap()
        {
            crate::ast::Statement::SetOpr(set) => {
                assert_eq!(set.rest.len(), 1);
                assert_eq!(set.rest[0].0, crate::ast::SetOprType::Union);
            }
            other => panic!("expected SetOprStmt, got {other:?}"),
        }
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
        assert!(matches!(
            parser
                .parse_one_stmt("SELECT LAG(id) OVER (ORDER BY id) FROM t")
                .unwrap()
                .as_select()
                .unwrap()
                .fields[0]
                .expr,
            crate::ast::ExprNode::Window(_)
        ));
        match &parser
            .parse_one_stmt(
                "SELECT SUM(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
            )
            .unwrap()
            .as_select()
            .unwrap()
            .fields[0]
            .expr
        {
            crate::ast::ExprNode::Window(w) => {
                let frame = w.spec.frame.as_ref().expect("frame");
                assert_eq!(frame.units, crate::ast::WindowFrameUnits::Rows);
                assert_eq!(
                    frame.start,
                    crate::ast::WindowFrameBound::UnboundedPreceding
                );
                assert_eq!(frame.end, crate::ast::WindowFrameBound::CurrentRow);
            }
            other => panic!("expected Window, got {other:?}"),
        }
        match &parser
            .parse_one_stmt("SELECT ROW_NUMBER() OVER w FROM t WINDOW w AS (ORDER BY id)")
            .unwrap()
            .as_select()
            .unwrap()
            .fields[0]
            .expr
        {
            crate::ast::ExprNode::Window(w) => {
                assert_eq!(w.spec.order_by.len(), 1);
                assert!(w.window_ref.is_none());
            }
            other => panic!("expected Window, got {other:?}"),
        }
        match &parser
            .parse_one_stmt("SELECT FIRST_VALUE(id) IGNORE NULLS OVER (ORDER BY id) FROM t")
            .unwrap()
            .as_select()
            .unwrap()
            .fields[0]
            .expr
        {
            crate::ast::ExprNode::Window(w) => assert!(w.ignore_nulls),
            other => panic!("expected Window, got {other:?}"),
        }
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
        let rec = parser
            .parse_one_stmt(
                "WITH RECURSIVE c AS (SELECT 1 UNION SELECT id + 1 FROM c) SELECT * FROM c",
            )
            .unwrap();
        let cte = &rec.as_select().unwrap().with.as_ref().unwrap().ctes[0];
        assert!(cte.recursive.is_some());
        assert!(rec.as_select().unwrap().with.as_ref().unwrap().is_recursive);
        let mat = parser
            .parse_one_stmt("WITH c AS MATERIALIZED (SELECT 1) SELECT * FROM c")
            .unwrap();
        assert_eq!(
            mat.as_select().unwrap().with.as_ref().unwrap().ctes[0].materialize,
            crate::ast::CteMaterialize::Materialized
        );
        let inline = parser
            .parse_one_stmt("WITH c AS NOT MATERIALIZED (SELECT 1) SELECT * FROM c")
            .unwrap();
        assert_eq!(
            inline.as_select().unwrap().with.as_ref().unwrap().ctes[0].materialize,
            crate::ast::CteMaterialize::NotMaterialized
        );
        let unioned = parser
            .parse_one_stmt("WITH c AS (SELECT 1 UNION SELECT 2) SELECT * FROM c")
            .unwrap();
        assert_eq!(
            unioned.as_select().unwrap().with.as_ref().unwrap().ctes[0]
                .set_rest
                .len(),
            1
        );
        let joined = parser
            .parse_one_stmt("WITH c AS (SELECT 1 AS id) SELECT * FROM t JOIN c ON t.id = c.id")
            .unwrap();
        assert_eq!(joined.as_select().unwrap().joins.len(), 1);
        let insert = parser
            .parse_one_stmt("WITH c AS (SELECT 1) INSERT INTO t SELECT * FROM c")
            .unwrap();
        assert!(matches!(insert, crate::ast::Statement::Insert(_)));
    }

    #[test]
    fn test_parse_derived_table() {
        let parser = Parser::new();
        let stmt = parser
            .parse_one_stmt("SELECT * FROM (SELECT 1 AS id) d")
            .unwrap();
        let select = stmt.as_select().unwrap();
        assert_eq!(select.from.as_ref().unwrap().name, "d");
        let with = select.with.as_ref().expect("derived table CTE");
        assert_eq!(with.ctes.len(), 1);
        assert_eq!(with.ctes[0].name, "d");
        let nested = parser
            .parse_one_stmt("SELECT * FROM (SELECT * FROM (SELECT 1 AS id) a) b")
            .unwrap();
        let names: Vec<_> = nested
            .as_select()
            .unwrap()
            .with
            .as_ref()
            .unwrap()
            .ctes
            .iter()
            .map(|cte| cte.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        let aliased = parser
            .parse_one_stmt("SELECT x FROM (SELECT 1) AS d(x)")
            .unwrap();
        assert_eq!(
            aliased.as_select().unwrap().with.as_ref().unwrap().ctes[0].col_name_list,
            vec!["x".to_string()]
        );
        let err = parser
            .parse_one_stmt("SELECT * FROM t1 JOIN (SELECT 1 AS id) d ON t1.id = d.id")
            .unwrap_err();
        assert!(err.message.contains("subquery JOIN"));
    }

    #[test]
    fn test_parse_partition() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (id INT, name VARCHAR(20)) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (MAXVALUE))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("range partition");
                assert_eq!(part.tp, crate::ast::PartitionType::Range);
                assert_eq!(part.column, "id");
                assert_eq!(part.defs.len(), 2);
                assert_eq!(part.defs[0].less_than, Some(10));
                assert_eq!(part.defs[1].less_than, None);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (id INT, name VARCHAR(20)) PARTITION BY HASH (id) PARTITIONS 4",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("hash partition");
                assert_eq!(part.tp, crate::ast::PartitionType::Hash);
                assert_eq!(part.num, 4);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (id INT) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (10))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("list partition");
                assert_eq!(part.tp, crate::ast::PartitionType::List);
                assert_eq!(part.defs[0].in_values, vec![1, 2]);
                assert_eq!(part.defs[1].in_values, vec![10]);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE TABLE t (id INT) PARTITION BY KEY (id) PARTITIONS 4")
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("key partition");
                assert_eq!(part.tp, crate::ast::PartitionType::Key);
                assert_eq!(part.num, 4);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (a INT, b INT) PARTITION BY RANGE COLUMNS (a, b) (PARTITION p0 VALUES LESS THAN (10, 20), PARTITION p1 VALUES LESS THAN (MAXVALUE, MAXVALUE))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("range columns");
                assert_eq!(part.columns, vec!["a".to_string(), "b".to_string()]);
                assert_eq!(part.defs[0].less_than, Some(10));
                assert_eq!(part.defs[0].extra_bounds, vec![Some(20)]);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("CREATE TABLE t (id INT) PARTITION BY HASH (id % 4) PARTITIONS 4")
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("expr hash");
                assert_eq!(part.expr.as_deref(), Some("id % 4"));
                assert_eq!(part.columns, vec!["id".to_string()]);
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (id INT) PARTITION BY RANGE (ABS(id)) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (MAXVALUE))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("expr range");
                assert_eq!(part.expr.as_deref(), Some("ABS(id)"));
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (a INT) PARTITION BY RANGE (a) SUBPARTITION BY HASH (a) SUBPARTITIONS 2 (PARTITION p0 VALUES LESS THAN (100), PARTITION p1 VALUES LESS THAN (MAXVALUE))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("subpartition stripped");
                assert_eq!(part.tp, crate::ast::PartitionType::Range);
                assert!(part.subpartition_ignored);
                assert_eq!(part.defs.len(), 2);
                assert_eq!(part.defs[0].less_than, Some(100));
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt(
                "CREATE TABLE t (a INT, b INT) PARTITION BY RANGE (a) SUBPARTITION BY HASH (b) SUBPARTITIONS 2 (PARTITION x VALUES LESS THAN (10) (SUBPARTITION y, SUBPARTITION z), PARTITION a VALUES LESS THAN (20) (SUBPARTITION b, SUBPARTITION c))",
            )
            .unwrap()
        {
            crate::ast::Statement::CreateTable(create) => {
                let part = create.partition.expect("named subpartitions stripped");
                assert!(part.subpartition_ignored);
                assert_eq!(part.defs.len(), 2);
                assert_eq!(part.defs[0].name, "x");
                assert_eq!(part.defs[1].less_than, Some(20));
            }
            other => panic!("expected CreateTableStmt, got {other:?}"),
        }
        let err = parser
            .parse_one_stmt(
                "CREATE TABLE t (a INT) PARTITION BY HASH (a) PARTITIONS 2 SUBPARTITION BY KEY (a) SUBPARTITIONS 2",
            )
            .unwrap_err();
        assert_eq!(err.code, 1500);
        match parser
            .parse_one_stmt("ALTER TABLE t ADD PARTITION (PARTITION p2 VALUES LESS THAN (30))")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[..] {
                [
                    crate::ast::AlterTableSpec::Algorithm(_),
                    crate::ast::AlterTableSpec::AddPartition { defs },
                ] => {
                    assert_eq!(defs[0].name, "p2");
                    assert_eq!(defs[0].less_than, Some(30));
                }
                other => panic!("expected ADD PARTITION, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ALTER TABLE t DROP PARTITION p0, p1")
            .unwrap()
        {
            crate::ast::Statement::AlterTable(alter) => match &alter.specs[..] {
                [
                    crate::ast::AlterTableSpec::Algorithm(_),
                    crate::ast::AlterTableSpec::DropPartition { names, .. },
                ] => {
                    assert_eq!(names, &["p0".to_string(), "p1".to_string()]);
                }
                other => panic!("expected DROP PARTITION, got {other:?}"),
            },
            other => panic!("expected AlterTableStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_grant_revoke() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("GRANT SELECT, INSERT ON db1.t TO 'u'@'%'")
            .unwrap()
        {
            crate::ast::Statement::Grant(grant) => {
                assert_eq!(grant.users[0].username, "u");
                assert_eq!(grant.level.table_name, "t");
            }
            other => panic!("expected GrantStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("REVOKE SELECT ON *.* FROM 'u'@'%'")
            .unwrap()
        {
            crate::ast::Statement::Revoke(revoke) => {
                assert_eq!(revoke.level.level, crate::ast::GrantLevelType::Global);
            }
            other => panic!("expected RevokeStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap() {
            crate::ast::Statement::Show(show) => {
                assert_eq!(show.kind, crate::ast::ShowStmtKind::Grants);
                assert_eq!(show.user.unwrap().username, "u");
            }
            other => panic!("expected ShowStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("GRANT SELECT ON db1.t TO 'u'@'%' WITH GRANT OPTION")
            .unwrap()
        {
            crate::ast::Statement::Grant(grant) => assert!(grant.with_grant),
            other => panic!("expected GrantStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("REVOKE GRANT OPTION FOR SELECT ON db1.t FROM 'u'@'%'")
            .unwrap()
        {
            crate::ast::Statement::Revoke(revoke) => assert!(revoke.grant_option),
            other => panic!("expected RevokeStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("GRANT SELECT (id, name) ON db1.t TO 'u'@'%'")
            .unwrap()
        {
            crate::ast::Statement::Grant(grant) => {
                assert_eq!(
                    grant.privs[0].cols,
                    vec!["id".to_string(), "name".to_string()]
                );
            }
            other => panic!("expected GrantStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_analyze() {
        let parser = Parser::new();
        match parser.parse_one_stmt("ANALYZE TABLE t").unwrap() {
            crate::ast::Statement::AnalyzeTable(analyze) => {
                assert_eq!(analyze.table.name, "t");
                assert!(analyze.indexes.is_empty());
            }
            other => panic!("expected AnalyzeTableStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("ANALYZE TABLE db1.t INDEX idx_name, idx_id")
            .unwrap()
        {
            crate::ast::Statement::AnalyzeTable(analyze) => {
                assert_eq!(analyze.table.name, "t");
                assert_eq!(analyze.indexes, vec!["idx_name", "idx_id"]);
            }
            other => panic!("expected AnalyzeTableStmt, got {other:?}"),
        }
        let err = parser.parse_one_stmt("ANALYZE TABLE t (id)").unwrap_err();
        assert!(err.message.contains("column") || err.message.contains("ANALYZE"));
    }

    #[test]
    fn test_parse_explain() {
        let parser = Parser::new();
        match parser.parse_one_stmt("EXPLAIN SELECT 1").unwrap() {
            crate::ast::Statement::Explain(explain) => {
                assert!(!explain.analyze);
                assert_eq!(explain.format, crate::ast::ExplainFormat::Row);
                assert!(matches!(*explain.stmt, crate::ast::Statement::Select(_)));
            }
            other => panic!("expected ExplainStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("EXPLAIN FORMAT = 'dot' SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::Explain(explain) => {
                assert_eq!(explain.format, crate::ast::ExplainFormat::Dot);
            }
            other => panic!("expected ExplainStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("EXPLAIN FORMAT='json' SELECT 1")
            .unwrap()
        {
            crate::ast::Statement::Explain(explain) => {
                assert_eq!(explain.format, crate::ast::ExplainFormat::Json);
            }
            other => panic!("expected ExplainStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("EXPLAIN ANALYZE SELECT 1").unwrap() {
            crate::ast::Statement::Explain(explain) => {
                assert!(explain.analyze);
                assert_eq!(explain.format, crate::ast::ExplainFormat::Row);
                assert!(matches!(*explain.stmt, crate::ast::Statement::Select(_)));
            }
            other => panic!("expected ExplainStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_set_transaction() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("START TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .unwrap()
        {
            crate::ast::Statement::Begin(begin) => {
                assert_eq!(
                    begin.isolation,
                    Some(crate::ast::IsolationLevel::ReadCommitted)
                );
                assert_eq!(begin.read_only, None);
            }
            other => panic!("expected BeginStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .unwrap()
        {
            crate::ast::Statement::SetTxn(set) => {
                assert_eq!(
                    set.isolation,
                    Some(crate::ast::IsolationLevel::RepeatableRead)
                );
                assert_eq!(set.read_only, None);
            }
            other => panic!("expected SetTxnStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("START TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap()
        {
            crate::ast::Statement::Begin(begin) => {
                assert_eq!(
                    begin.isolation,
                    Some(crate::ast::IsolationLevel::Serializable)
                );
            }
            other => panic!("expected BeginStmt, got {other:?}"),
        }
        match parser
            .parse_one_stmt("START TRANSACTION READ ONLY")
            .unwrap()
        {
            crate::ast::Statement::Begin(begin) => {
                assert_eq!(begin.read_only, Some(true));
                assert!(begin.isolation.is_none());
            }
            other => panic!("expected BeginStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SET TRANSACTION READ WRITE").unwrap() {
            crate::ast::Statement::SetTxn(set) => {
                assert_eq!(set.read_only, Some(false));
                assert!(set.isolation.is_none());
            }
            other => panic!("expected SetTxnStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_prepare_execute() {
        let parser = Parser::new();
        match parser
            .parse_one_stmt("PREPARE stmt FROM 'SELECT 1'")
            .unwrap()
        {
            crate::ast::Statement::Prepare(prepare) => {
                assert_eq!(prepare.name, "stmt");
                assert!(prepare.sql.to_ascii_uppercase().contains("SELECT"));
            }
            other => panic!("expected PrepareStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("EXECUTE stmt USING 1, 'a'").unwrap() {
            crate::ast::Statement::Execute(exec) => {
                assert_eq!(exec.name, "stmt");
                assert_eq!(exec.using.len(), 2);
            }
            other => panic!("expected ExecuteStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SET NAMES utf8mb4").unwrap() {
            crate::ast::Statement::Set(set) => {
                assert_eq!(set.assignments[0].name, "names");
            }
            other => panic!("expected SetStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("DEALLOCATE PREPARE stmt").unwrap() {
            crate::ast::Statement::Deallocate(dealloc) => {
                assert_eq!(dealloc.name, "stmt");
            }
            other => panic!("expected DeallocateStmt, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_user_vars() {
        let parser = Parser::new();
        match parser.parse_one_stmt("SET @a = 1").unwrap() {
            crate::ast::Statement::Set(set) => {
                assert_eq!(set.assignments[0].name, "@a");
                assert_eq!(set.assignments[0].value, "1");
            }
            other => panic!("expected SetStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("SELECT @a").unwrap() {
            crate::ast::Statement::Select(select) => match &select.fields[0].expr {
                crate::ast::expressions::ExprNode::Column(col) => assert_eq!(col.name, "@a"),
                other => panic!("expected Column @a, got {other:?}"),
            },
            other => panic!("expected SelectStmt, got {other:?}"),
        }
        match parser.parse_one_stmt("EXECUTE s USING @a").unwrap() {
            crate::ast::Statement::Execute(exec) => match &exec.using[0] {
                crate::ast::expressions::ExprNode::Column(col) => assert_eq!(col.name, "@a"),
                other => panic!("expected Column @a, got {other:?}"),
            },
            other => panic!("expected ExecuteStmt, got {other:?}"),
        }
    }
}

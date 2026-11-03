use super::dialect::dialect;
use crate::ast::ddl::{
    AlterDatabaseStmt, AlterSequenceStmt, AlterTableSpec, AlterTableStmt, ColumnDef,
    ColumnPosition, CreateDatabaseStmt, CreateIndexStmt, CreateSequenceStmt, CreateTableStmt,
    CreateViewStmt, DropDatabaseStmt, DropIndexStmt, DropSequenceStmt, DropTableStmt, IndexDef,
    PartitionDef, PartitionOption, PartitionType, SequenceOptionSet, TableName, TruncateTableStmt,
};
use crate::ast::dml::{
    Assignment, ByItem, CommonTableExpression, CteMaterialize, DeleteStmt, InsertStmt, Join,
    JoinType, LoadDataStmt, ResultSetNode, SelectField, SelectLock, SelectLockWait, SelectStmt,
    SetOprStmt, SetOprType, UpdateStmt, WithClause,
};
use crate::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ExistsExpr, ExprNode, FuncCallExpr, FuncCastExpr, InListExpr,
    InSubqueryExpr, IsNullExpr, PatternLikeExpr, QuantifiedCmpExpr, ScalarSubqueryExpr,
    UnaryOpExpr, ValueExpr, WindowFrameBound, WindowFrameSpec, WindowFrameUnits, WindowFuncExpr,
    WindowSpec,
};
use crate::ast::misc::{
    AlterInstanceStmt, AnalyzeTableStmt, BRIEStmt, BeginStmt, BinlogStmt, CalibrateResourceStmt,
    ChangeStmt, CleanupTableLockStmt, CommitStmt, CompactTableStmt, CreateBindingStmt,
    CreateRoleStmt, CreateUserStmt, DeallocateStmt, DistributeTableStmt, DoStmt, DropBindingStmt,
    DropRoleStmt, DropUserStmt, ExecuteStmt, FlushStmt, FlushStmtType, GrantLevel, GrantLevelType,
    GrantRoleStmt, GrantStmt, ImportIntoStmt, IsolationLevel, KillStmt, LoadStatsStmt,
    LockStatsStmt, LockTablesStmt, OptimizeTableStmt, PlanReplayerStmt, PrepareStmt, PrivElem,
    PrivilegeType, RecommendIndexStmt, ReleaseSavepointStmt, RenameUserStmt, RepairTableStmt,
    RevokeRoleStmt, RevokeStmt, RollbackStmt, SavepointStmt, SetAssignment, SetPwdStmt, SetRoleKind,
    SetRoleStmt, SetStmt, SetTxnStmt, SetVarScope, ShowStmt, ShowStmtKind, ShutdownStmt,
    SplitRegionStmt, TableLockItem, TableLockType, TraceStmt, UnlockStatsStmt, UseStmt,
    UserIdentity, UserSpec, UserToUser,
};
use crate::ast::{ExplainFormat, ExplainStmt, Node, Statement, StmtNode};
use crate::opcode::Op;
use sealdb_errno::{ERR_PARSE_ERROR, ERR_UNSUPPORTED, Error, Result};
use sealdb_types::Datum;
use sqlparser::ast::{
    Action, AlterTableOperation, AnalyzeFormat, AnalyzeFormatKind, AssignmentTarget,
    BinaryOperator, ContextModifier, CreateTable, CreateTableLikeKind, CreateView,
    CreateViewAlgorithm, CreateViewSecurity, Cte, CteAsMaterialized, DataType, Delete,
    DescribeAlias, DuplicateTreatment, Expr, FlushType, FromTable, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentClause, FunctionArguments, Grant, GrantObjects, GranteeName,
    GroupByExpr, JoinConstraint, JoinOperator, KillType, LimitClause, LockClause,
    LockTableType as SqlLockTableType, LockType, MySQLColumnPosition, NamedWindowDefinition,
    NamedWindowExpr, NonBlock, NullTreatment, ObjectName, ObjectType, OrderByExpr, OrderByKind,
    OrderBySort, Privileges, Query, RenameTable as SqlRenameTable, RenameTableNameKind, Revoke,
    SchemaName, SelectItem, Set, SetExpr, SetOperator, SetQuantifier, ShowCreateObject,
    Statement as SqlStatement, TableConstraint, TableFactor, TableObject, TableWithJoins,
    TransactionAccessMode, TransactionIsolationLevel, TransactionMode, UnaryOperator, Update, Use,
    Value, Values, WindowFrameBound as SqlWindowFrameBound,
    WindowFrameUnits as SqlWindowFrameUnits, WindowType, With,
};
use sqlparser::parser::Parser;
use std::collections::HashMap;

pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let (rewritten, pessimistic_flags) = rewrite_begin_pessimistic(sql);
    let rewritten = rewrite_show_grants(&rewritten)?;
    let rewritten = rewrite_show_create_database(&rewritten);
    let rewritten = rewrite_show_index(&rewritten);
    let rewritten = rewrite_admin_show_ddl(&rewritten);
    let rewritten = rewrite_show_bindings(&rewritten);
    let rewritten = rewrite_rename_index(&rewritten);
    let rewritten = rewrite_explain_format(&rewritten);
    let rewritten = rewrite_lock_in_share_mode(&rewritten);
    let (rewritten, materialize_hints) = rewrite_cte_materialized(&rewritten);
    let (rewritten, prepares) = extract_prepare_from(&rewritten);
    let (rewritten, partitions) = extract_create_table_partitions(&rewritten)?;
    let (rewritten, alter_parts) = extract_alter_partitions(&rewritten)?;
    // sqlparser rejects MySQL `GRANT/REVOKE role TO/FROM user` (no ON).
    let (rewritten, role_ops) = extract_role_grants(&rewritten)?;
    // sqlparser rejects MySQL `CREATE/DROP USER 'u'@'%'`.
    let (rewritten, user_ops) = extract_user_ddl(&rewritten)?;
    // sqlparser rejects TiDB `ANALYZE TABLE t INDEX idx`.
    let (rewritten, analyze_indexes) = extract_analyze_index(&rewritten)?;
    // sqlparser rejects TiDB `CREATE/DROP BINDING`.
    let (rewritten, binding_ops) = extract_binding_ddl(&rewritten)?;
    // sqlparser rejects MySQL `DO` / `SET PASSWORD` / `SHUTDOWN` / `ALTER DATABASE … CHARSET`.
    let (rewritten, misc_ops) = extract_misc_allowlist(&rewritten)?;
    // sqlparser MySQL dialect only partially supports SEQUENCE DDL.
    let (rewritten, seq_ops) = extract_sequence_ddl(&rewritten)?;
    // sqlparser has Hive LOAD DATA INPATH, not MySQL LOAD DATA INFILE.
    let (rewritten, load_ops) = extract_load_data(&rewritten)?;
    let stmts = Parser::parse_sql(&dialect(), &rewritten).map_err(|e| {
        Error::new(
            ERR_PARSE_ERROR,
            format!("You have an error in your SQL syntax: {e}"),
        )
    })?;
    let mut flags = pessimistic_flags.into_iter();
    let mut partitions = partitions.into_iter();
    let mut alter_parts = alter_parts.into_iter();
    let mut prepares = prepares.into_iter();
    let mut role_ops = role_ops.into_iter();
    let mut user_ops = user_ops.into_iter();
    let mut analyze_indexes = analyze_indexes.into_iter();
    let mut binding_ops = binding_ops.into_iter();
    let mut misc_ops = misc_ops.into_iter();
    let mut seq_ops = seq_ops.into_iter();
    let mut load_ops = load_ops.into_iter();
    let mut materialize_hints = materialize_hints;
    let mut out = Vec::new();
    for stmt in &stmts {
        let mut lowered = lower_statement(stmt)?;
        if let Statement::Begin(begin) = &mut lowered
            && flags.next() == Some(true)
        {
            begin.pessimistic = true;
        }
        if let Statement::Show(show) = &mut lowered {
            decode_show_grants(show);
            decode_show_index(show);
            decode_show_ddl_jobs(show);
            decode_show_bindings(show);
        }
        if let Statement::CreateTable(create) = &mut lowered {
            create.partition = partitions.next().flatten();
        }
        if let Statement::AlterTable(alter) = &mut lowered
            && let Some(op) = alter_parts.next().flatten()
        {
            match op {
                AlterPartOp::Add(defs) => {
                    alter.specs.push(AlterTableSpec::AddPartition { defs });
                }
                AlterPartOp::Drop { if_exists, names } => {
                    alter
                        .specs
                        .push(AlterTableSpec::DropPartition { if_exists, names });
                }
            }
        }
        if let Statement::Prepare(prepare) = &mut lowered
            && let Some(sql) = prepares.next().flatten()
        {
            prepare.sql = sql;
        }
        if let Statement::AnalyzeTable(analyze) = &mut lowered {
            analyze.indexes = analyze_indexes.next().flatten().unwrap_or_default();
        }
        // Placeholders from extract_role_grants / extract_user_ddl become Set NAMES.
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_role__"))
            && let Some(op) = role_ops.next().flatten()
        {
            lowered = op;
        }
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_user__"))
            && let Some(op) = user_ops.next().flatten()
        {
            lowered = op;
        }
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_binding__"))
            && let Some(op) = binding_ops.next().flatten()
        {
            lowered = op;
        }
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_misc__"))
            && let Some(op) = misc_ops.next().flatten()
        {
            lowered = op;
        }
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_seq__"))
            && let Some(op) = seq_ops.next().flatten()
        {
            lowered = op;
        }
        if matches!(&lowered, Statement::Set(s) if is_placeholder(s, "__sealdb_load__"))
            && let Some(op) = load_ops.next().flatten()
        {
            lowered = op;
        }
        apply_cte_materialize(&mut lowered, &mut materialize_hints);
        out.push(lowered);
    }
    Ok(out)
}

fn is_placeholder(stmt: &SetStmt, marker: &str) -> bool {
    matches!(
        stmt.assignments.as_slice(),
        [SetAssignment { name, value, .. }]
            if name.eq_ignore_ascii_case("names") && value.eq_ignore_ascii_case(marker)
    )
}

fn lower_statement(stmt: &SqlStatement) -> Result<Statement> {
    match stmt {
        SqlStatement::Query(query) => lower_query_stmt(query),
        SqlStatement::CreateDatabase {
            db_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: object_name_last(db_name)?,
        })),
        SqlStatement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: schema_name_last(schema_name)?,
        })),
        SqlStatement::CreateTable(create) => lower_create_table(create),
        SqlStatement::CreateView(create) => lower_create_view(create),
        SqlStatement::CreateIndex(create) => lower_create_index(create),
        SqlStatement::Truncate(truncate) => lower_truncate(truncate),
        SqlStatement::Drop {
            object_type: ObjectType::Index,
            if_exists,
            names,
            table,
            ..
        } => {
            let table = table
                .as_ref()
                .ok_or_else(|| Error::unsupported("DROP INDEX without ON"))?;
            let name = names
                .first()
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing index name"))?;
            if names.len() != 1 {
                return Err(Error::unsupported("DROP INDEX multiple names"));
            }
            Ok(Statement::DropIndex(DropIndexStmt {
                if_exists: *if_exists,
                table: object_table_name(table)?,
                name: object_name_last(name)?,
            }))
        }
        SqlStatement::AlterTable(alter) => lower_alter_table(alter),
        SqlStatement::RenameTable(renames) => lower_rename_tables(renames),
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            temporary,
            ..
        } => {
            if *temporary {
                return Err(Error::unsupported("DROP TEMPORARY"));
            }
            match object_type {
                ObjectType::Database | ObjectType::Schema => {
                    let name = names
                        .first()
                        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing database name"))?;
                    Ok(Statement::DropDatabase(DropDatabaseStmt {
                        if_exists: *if_exists,
                        name: object_name_last(name)?,
                    }))
                }
                ObjectType::Table => Ok(Statement::DropTable(DropTableStmt {
                    if_exists: *if_exists,
                    tables: names
                        .iter()
                        .map(object_table_name)
                        .collect::<Result<Vec<_>>>()?,
                    is_view: false,
                })),
                ObjectType::View => Ok(Statement::DropTable(DropTableStmt {
                    if_exists: *if_exists,
                    tables: names
                        .iter()
                        .map(object_table_name)
                        .collect::<Result<Vec<_>>>()?,
                    is_view: true,
                })),
                ObjectType::Role => Ok(Statement::DropRole(DropRoleStmt {
                    if_exists: *if_exists,
                    roles: names
                        .iter()
                        .map(role_from_object_name)
                        .collect::<Result<Vec<_>>>()?,
                })),
                other => Err(Error::unsupported(format!("DROP {other}"))),
            }
        }
        SqlStatement::CreateRole(create) => Ok(Statement::CreateRole(CreateRoleStmt {
            if_not_exists: create.if_not_exists,
            roles: create
                .names
                .iter()
                .map(role_from_object_name)
                .collect::<Result<Vec<_>>>()?,
        })),
        SqlStatement::ShowCreate {
            obj_type: ShowCreateObject::Table,
            obj_name,
        } => {
            let table = object_table_name(obj_name)?;
            if table
                .schema
                .as_deref()
                .is_some_and(|schema| schema.eq_ignore_ascii_case(SHOW_CREATE_DB_SCHEMA))
            {
                Ok(Statement::Show(ShowStmt {
                    kind: ShowStmtKind::CreateDatabase,
                    db_name: Some(table.name),
                    table: None,
                    user: None,
                    full: false,
                }))
            } else {
                Ok(Statement::Show(ShowStmt {
                    kind: ShowStmtKind::CreateTable,
                    db_name: None,
                    table: Some(table),
                    user: None,
                    full: false,
                }))
            }
        }
        SqlStatement::ShowCreate {
            obj_type: ShowCreateObject::View,
            obj_name,
        } => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::CreateView,
            db_name: None,
            table: Some(object_table_name(obj_name)?),
            user: None,
            full: false,
        })),
        SqlStatement::ShowDatabases { .. } | SqlStatement::ShowSchemas { .. } => {
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Databases,
                db_name: None,
                table: None,
                user: None,
                full: false,
            }))
        }
        SqlStatement::ShowTables { show_options, .. } => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::Tables,
            db_name: show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .map(object_name_last)
                .transpose()?,
            table: None,
            user: None,
            full: false,
        })),
        SqlStatement::ShowCharset(_) => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::Charset,
            db_name: None,
            table: None,
            user: None,
            full: false,
        })),
        SqlStatement::ShowCollation { .. } => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::Collation,
            db_name: None,
            table: None,
            user: None,
            full: false,
        })),
        SqlStatement::ShowColumns {
            full, show_options, ..
        } => {
            let table = show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing table name"))?;
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table)?),
                user: None,
                full: *full,
            }))
        }
        SqlStatement::ExplainTable {
            describe_alias,
            table_name,
            ..
        } => {
            if !matches!(
                describe_alias,
                DescribeAlias::Desc | DescribeAlias::Describe
            ) {
                return Err(Error::unsupported(format!("EXPLAIN {describe_alias}")));
            }
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table_name)?),
                user: None,
                full: false,
            }))
        }
        SqlStatement::Insert(insert) => {
            let table = match &insert.table {
                TableObject::TableName(name) => object_table_name(name)?,
                other => return Err(Error::unsupported(format!("INSERT target {other}"))),
            };
            let columns = insert
                .columns
                .iter()
                .map(object_name_last)
                .collect::<Result<Vec<_>>>()?;
            let (rows, select) = match insert.source.as_deref() {
                Some(query) => match query.body.as_ref() {
                    SetExpr::Values(values) => (lower_values(values)?, None),
                    _ => (Vec::new(), Some(Box::new(lower_query(query)?))),
                },
                None => return Err(Error::unsupported("INSERT without VALUES")),
            };
            Ok(Statement::Insert(InsertStmt {
                table,
                columns,
                rows,
                select,
                with: None,
            }))
        }
        SqlStatement::Update(update) => lower_update(update),
        SqlStatement::Delete(delete) => lower_delete(delete),
        SqlStatement::Use(use_stmt) => {
            let name = match use_stmt {
                Use::Catalog(name)
                | Use::Schema(name)
                | Use::Database(name)
                | Use::Object(name) => object_name_last(name)?,
                other => return Err(Error::unsupported(format!("USE {other}"))),
            };
            Ok(Statement::Use(UseStmt { db_name: name }))
        }
        SqlStatement::StartTransaction {
            modes,
            statements,
            exception,
            ..
        } => {
            // Completeness wave: causal consistency / BEGIN ... END / SERIALIZABLE.
            if !statements.is_empty() || exception.is_some() {
                return Err(Error::unsupported("BEGIN block"));
            }
            let chars = txn_chars(modes)?;
            Ok(Statement::Begin(BeginStmt {
                pessimistic: false,
                isolation: chars.isolation,
                read_only: chars.read_only,
            }))
        }
        SqlStatement::Set(set) => lower_set(set),
        SqlStatement::Flush {
            object_type,
            location,
            ..
        } => Ok(Statement::Flush(FlushStmt {
            tp: match object_type {
                FlushType::Privileges => FlushStmtType::Privileges,
                FlushType::Tables => FlushStmtType::Tables,
                _ => FlushStmtType::Other,
            },
            no_write_to_binlog: location.is_some(),
        })),
        SqlStatement::Commit {
            chain, modifier, ..
        } => {
            if modifier.is_some() {
                return Err(Error::unsupported("COMMIT modifier"));
            }
            Ok(Statement::Commit(CommitStmt { chain: *chain }))
        }
        SqlStatement::Rollback { chain, savepoint } => {
            if *chain && savepoint.is_some() {
                return Err(Error::unsupported("ROLLBACK AND CHAIN TO SAVEPOINT"));
            }
            Ok(Statement::Rollback(RollbackStmt {
                savepoint: savepoint.as_ref().map(|name| name.value.clone()),
                chain: *chain,
            }))
        }
        SqlStatement::Savepoint { name } => Ok(Statement::Savepoint(SavepointStmt {
            name: name.value.clone(),
        })),
        SqlStatement::ReleaseSavepoint { name } => {
            Ok(Statement::ReleaseSavepoint(ReleaseSavepointStmt {
                name: name.value.clone(),
            }))
        }
        SqlStatement::Grant(grant) => lower_grant(grant),
        SqlStatement::Revoke(revoke) => lower_revoke(revoke),
        SqlStatement::Analyze(analyze) => lower_analyze(analyze),
        SqlStatement::Kill { modifier, id } => Ok(Statement::Kill(KillStmt {
            query: matches!(modifier, Some(KillType::Query)),
            connection_id: *id,
        })),
        SqlStatement::LockTables { tables } => Ok(Statement::LockTables(LockTablesStmt {
            table_locks: tables
                .iter()
                .map(|t| {
                    Ok(TableLockItem {
                        table: TableName::new(None, t.table.value.clone()),
                        lock_type: match &t.lock_type {
                            SqlLockTableType::Read { .. } => TableLockType::Read,
                            SqlLockTableType::Write { .. } => TableLockType::Write,
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })),
        SqlStatement::UnlockTables => Ok(Statement::CleanupTableLock(CleanupTableLockStmt {
            tables: Vec::new(),
        })),
        SqlStatement::Prepare {
            name, statement, ..
        } => {
            let mut ctx = crate::format::RestoreCtx::new();
            let lowered = lower_statement(statement)?;
            lowered.restore(&mut ctx)?;
            Ok(Statement::Prepare(PrepareStmt {
                name: name.value.clone(),
                sql: ctx.into_string(),
            }))
        }
        SqlStatement::Execute {
            name,
            using,
            parameters,
            ..
        } => {
            let name = name
                .as_ref()
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing prepared statement name"))?;
            let mut values = Vec::new();
            for expr in using.iter().map(|e| &e.expr).chain(parameters.iter()) {
                values.push(lower_expr(expr)?);
            }
            Ok(Statement::Execute(ExecuteStmt {
                name: object_name_last(name)?,
                using: values,
            }))
        }
        SqlStatement::Deallocate { name, .. } => Ok(Statement::Deallocate(DeallocateStmt {
            name: name.value.clone(),
        })),
        SqlStatement::Explain {
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
            ..
        } => {
            if *query_plan || *estimate || options.is_some() {
                return Err(Error::unsupported("EXPLAIN QUERY PLAN / ESTIMATE"));
            }
            Ok(Statement::Explain(ExplainStmt {
                analyze: *analyze,
                format: lower_explain_format(*verbose, format.as_ref())?,
                stmt: Box::new(lower_statement(statement)?),
            }))
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("statement {other} is not available in this phase"),
        )),
    }
}

const SHOW_GRANT_MARKER: &str = "__sealdb_grant__";
const SHOW_GRANT_PREFIX: &str = "__sealdb_grant_";
const SHOW_CREATE_DB_SCHEMA: &str = "__sealdb_db__";
const SHOW_INDEX_SCHEMA: &str = "__sealdb_index__";
const SHOW_DDL_JOBS_SCHEMA: &str = "__sealdb_ddl_jobs__";
const SHOW_BINDINGS_SCHEMA: &str = "__sealdb_bindings__";
const RENAME_INDEX_MARKER: &str = "__sealdb_rename_index__";
const RENAME_INDEX_SEP: char = '\u{1f}';

/// sqlparser has no `ADMIN SHOW DDL JOBS`; rewrite to SHOW TABLES FROM a marker schema.
/// sqlparser MySQL dialect has no `ALTER TABLE t RENAME INDEX`. Rewrite to a
/// marked `RENAME COLUMN` that `lower_alter_spec` turns back into RenameIndex.
fn rewrite_rename_index(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 24);
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "RENAME") {
            let after_rename = skip_ws(bytes, i + "RENAME".len());
            if keyword_at(bytes, after_rename, "INDEX") {
                let after_index = skip_ws(bytes, after_rename + "INDEX".len());
                let (old_name, after_old) = read_ident(bytes, after_index);
                let after_old = skip_ws(bytes, after_old);
                if keyword_at(bytes, after_old, "TO") {
                    let after_to = skip_ws(bytes, after_old + "TO".len());
                    let (new_name, after_new) = read_ident(bytes, after_to);
                    if !old_name.is_empty() && !new_name.is_empty() {
                        out.push_str("RENAME COLUMN `");
                        out.push_str(RENAME_INDEX_MARKER);
                        out.push_str("` TO `");
                        out.push_str(&escape_backticks(&old_name));
                        out.push(RENAME_INDEX_SEP);
                        out.push_str(&escape_backticks(&new_name));
                        out.push('`');
                        i = after_new;
                        continue;
                    }
                }
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    out
}

fn escape_backticks(name: &str) -> String {
    name.replace('`', "``")
}

fn split_rename_index_marker(encoded: &str) -> Result<(String, String)> {
    let (old_name, new_name) = encoded
        .split_once(RENAME_INDEX_SEP)
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "malformed RENAME INDEX marker"))?;
    Ok((old_name.to_string(), new_name.to_string()))
}

fn rewrite_admin_show_ddl(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 24);
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "ADMIN") {
            let after_admin = skip_ws(bytes, i + "ADMIN".len());
            if keyword_at(bytes, after_admin, "SHOW") {
                let after_show = skip_ws(bytes, after_admin + "SHOW".len());
                if keyword_at(bytes, after_show, "DDL") {
                    let after_ddl = skip_ws(bytes, after_show + "DDL".len());
                    let end = if keyword_at(bytes, after_ddl, "JOBS") {
                        after_ddl + "JOBS".len()
                    } else {
                        after_show + "DDL".len()
                    };
                    out.push_str("SHOW TABLES FROM `");
                    out.push_str(SHOW_DDL_JOBS_SCHEMA);
                    out.push('`');
                    i = end;
                    continue;
                }
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    out
}

fn decode_show_ddl_jobs(show: &mut ShowStmt) {
    if show.kind == ShowStmtKind::Tables
        && show
            .db_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(SHOW_DDL_JOBS_SCHEMA))
    {
        show.kind = ShowStmtKind::DdlJobs;
        show.db_name = None;
    }
}

/// sqlparser has no `SHOW BINDINGS`; rewrite to SHOW TABLES FROM a marker schema.
fn rewrite_show_bindings(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 24);
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "SHOW") {
            let after_show = skip_ws(bytes, i + "SHOW".len());
            if keyword_at(bytes, after_show, "BINDINGS") {
                out.push_str("SHOW TABLES FROM `");
                out.push_str(SHOW_BINDINGS_SCHEMA);
                out.push('`');
                i = after_show + "BINDINGS".len();
                continue;
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    out
}

fn decode_show_bindings(show: &mut ShowStmt) {
    if show.kind == ShowStmtKind::Tables
        && show
            .db_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(SHOW_BINDINGS_SCHEMA))
    {
        show.kind = ShowStmtKind::Bindings;
        show.db_name = None;
    }
}

/// sqlparser rejects TiDB `CREATE/DROP BINDING`; replace with `SET NAMES` placeholders.
fn extract_binding_ddl(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "CREATE") {
            let after = skip_ws(bytes, i + "CREATE".len());
            let after_scope = skip_binding_scope(bytes, after);
            if keyword_at(bytes, after_scope, "BINDING") {
                let (stmt, end) = parse_create_binding(bytes, after_scope + "BINDING".len())?;
                out.push_str("SET NAMES '__sealdb_binding__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "DROP") {
            let after = skip_ws(bytes, i + "DROP".len());
            let after_scope = skip_binding_scope(bytes, after);
            if keyword_at(bytes, after_scope, "BINDING") {
                let (stmt, end) = parse_drop_binding(bytes, after_scope + "BINDING".len())?;
                out.push_str("SET NAMES '__sealdb_binding__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok((out, ops))
}

fn skip_binding_scope(bytes: &[u8], i: usize) -> usize {
    if keyword_at(bytes, i, "GLOBAL") || keyword_at(bytes, i, "SESSION") {
        let kw = if keyword_at(bytes, i, "GLOBAL") {
            "GLOBAL"
        } else {
            "SESSION"
        };
        return skip_ws(bytes, i + kw.len());
    }
    i
}

fn parse_create_binding(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if !keyword_at(bytes, i, "FOR") {
        return Err(Error::unsupported("CREATE BINDING without FOR"));
    }
    i = skip_ws(bytes, i + "FOR".len());
    let (original, using_at) = take_sql_until_keyword(bytes, i, "USING")
        .ok_or_else(|| Error::unsupported("CREATE BINDING without USING"))?;
    i = skip_ws(bytes, using_at + "USING".len());
    let (bind_sql, end) = take_sql_until_semicolon(bytes, i);
    let original_sql = original.trim().to_string();
    let bind_sql = bind_sql.trim().to_string();
    if original_sql.is_empty() || bind_sql.is_empty() {
        return Err(Error::unsupported("CREATE BINDING with empty SQL"));
    }
    Ok((
        Statement::CreateBinding(CreateBindingStmt {
            original_sql,
            bind_sql,
        }),
        end,
    ))
}

fn parse_drop_binding(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if !keyword_at(bytes, i, "FOR") {
        return Err(Error::unsupported("DROP BINDING without FOR"));
    }
    i = skip_ws(bytes, i + "FOR".len());
    let (original, end) = take_sql_until_semicolon(bytes, i);
    let original_sql = original.trim().to_string();
    if original_sql.is_empty() {
        return Err(Error::unsupported("DROP BINDING with empty SQL"));
    }
    Ok((
        Statement::DropBinding(DropBindingStmt { original_sql }),
        end,
    ))
}

/// Scan SQL text from `start` until a top-level `keyword` (not inside quotes).
fn take_sql_until_keyword(bytes: &[u8], start: usize, keyword: &str) -> Option<(String, usize)> {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            i = skip_quoted(bytes, i);
            continue;
        }
        if keyword_at(bytes, i, keyword) {
            let sql = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            return Some((sql, i));
        }
        i += 1;
    }
    None
}

fn take_sql_until_semicolon(bytes: &[u8], start: usize) -> (String, usize) {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            i = skip_quoted(bytes, i);
            continue;
        }
        if b == b';' {
            let sql = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            return (sql, i);
        }
        i += 1;
    }
    (
        String::from_utf8_lossy(&bytes[start..]).into_owned(),
        bytes.len(),
    )
}

fn skip_quoted(bytes: &[u8], i: usize) -> usize {
    let quote = bytes[i];
    let mut j = i + 1;
    while j < bytes.len() {
        if bytes[j] == quote {
            // MySQL escaped quote by doubling.
            if j + 1 < bytes.len() && bytes[j + 1] == quote {
                j += 2;
                continue;
            }
            return j + 1;
        }
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        j += 1;
    }
    bytes.len()
}

fn rewrite_show_create_database(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "SHOW") {
            let after_show = skip_ws(bytes, i + "SHOW".len());
            if keyword_at(bytes, after_show, "CREATE") {
                let after_create = skip_ws(bytes, after_show + "CREATE".len());
                if keyword_at(bytes, after_create, "DATABASE")
                    || keyword_at(bytes, after_create, "SCHEMA")
                {
                    let kw_len = if keyword_at(bytes, after_create, "DATABASE") {
                        "DATABASE".len()
                    } else {
                        "SCHEMA".len()
                    };
                    let after_kw = skip_ws(bytes, after_create + kw_len);
                    let (name, end) = read_ident(bytes, after_kw);
                    if !name.is_empty() {
                        out.push_str("SHOW CREATE TABLE `");
                        out.push_str(SHOW_CREATE_DB_SCHEMA);
                        out.push_str("`.`");
                        out.push_str(&name);
                        out.push('`');
                        i = end;
                        continue;
                    }
                }
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    out
}

fn read_ident(bytes: &[u8], mut i: usize) -> (String, usize) {
    if i < bytes.len() && (bytes[i] == b'`' || bytes[i] == b'\'' || bytes[i] == b'"') {
        let q = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != q {
            i += 1;
        }
        let name = String::from_utf8_lossy(&bytes[start..i]).into_owned();
        if i < bytes.len() {
            i += 1;
        }
        return (name, i);
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    (String::from_utf8_lossy(&bytes[start..i]).into_owned(), i)
}

fn rewrite_show_index(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 24);
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "SHOW") {
            let after_show = skip_ws(bytes, i + "SHOW".len());
            let (kw, kw_len) = if keyword_at(bytes, after_show, "INDEXES") {
                ("INDEXES", "INDEXES".len())
            } else if keyword_at(bytes, after_show, "INDEX") {
                ("INDEX", "INDEX".len())
            } else if keyword_at(bytes, after_show, "KEYS") {
                ("KEYS", "KEYS".len())
            } else {
                ("", 0)
            };
            if !kw.is_empty() {
                let after_kw = skip_ws(bytes, after_show + kw_len);
                let after_from = if keyword_at(bytes, after_kw, "FROM") {
                    skip_ws(bytes, after_kw + "FROM".len())
                } else if keyword_at(bytes, after_kw, "IN") {
                    skip_ws(bytes, after_kw + "IN".len())
                } else {
                    after_kw
                };
                let (first, mut end) = read_ident(bytes, after_from);
                if !first.is_empty() {
                    let mut name = first;
                    let after_first = skip_ws(bytes, end);
                    if after_first < bytes.len() && bytes[after_first] == b'.' {
                        let (second, second_end) =
                            read_ident(bytes, skip_ws(bytes, after_first + 1));
                        if !second.is_empty() {
                            name = format!("{name}.{second}");
                            end = second_end;
                        }
                    }
                    out.push_str("SHOW COLUMNS FROM `");
                    out.push_str(SHOW_INDEX_SCHEMA);
                    out.push_str("`.`");
                    out.push_str(&name);
                    out.push('`');
                    i = end;
                    continue;
                }
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    out
}

fn decode_show_index(show: &mut ShowStmt) {
    let Some(table) = show.table.as_mut() else {
        return;
    };
    if !table
        .schema
        .as_deref()
        .is_some_and(|schema| schema.eq_ignore_ascii_case(SHOW_INDEX_SCHEMA))
    {
        return;
    }
    show.kind = ShowStmtKind::Index;
    if let Some((db, name)) = table.name.split_once('.') {
        table.schema = Some(db.to_string());
        table.name = name.to_string();
    } else {
        table.schema = None;
    }
}

fn rewrite_show_grants(sql: &str) -> Result<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "SHOW") {
            let after_show = skip_ws(bytes, i + "SHOW".len());
            if keyword_at(bytes, after_show, "GRANTS") {
                let after_grants = skip_ws(bytes, after_show + "GRANTS".len());
                let (marker, end) = if keyword_at(bytes, after_grants, "FOR") {
                    let after_for = skip_ws(bytes, after_grants + "FOR".len());
                    if keyword_at(bytes, after_for, "CURRENT_USER") {
                        let after_user = after_for + "CURRENT_USER".len();
                        let after_user = if after_user < bytes.len() && bytes[after_user] == b'(' {
                            skip_ws(bytes, after_user + 1).max(after_user)
                                + usize::from(
                                    skip_ws(bytes, after_user + 1) < bytes.len()
                                        && bytes[skip_ws(bytes, after_user + 1)] == b')',
                                )
                        } else {
                            after_user
                        };
                        (SHOW_GRANT_MARKER.to_string(), after_user)
                    } else {
                        let (user, end) = parse_user_identity_bytes(bytes, after_for)?;
                        (
                            format!("{SHOW_GRANT_PREFIX}{}@{}__", user.username, user.hostname),
                            end,
                        )
                    }
                } else {
                    (SHOW_GRANT_MARKER.to_string(), after_grants)
                };
                out.push_str("DESC `");
                out.push_str(&marker);
                out.push('`');
                i = end;
                continue;
            }
        }
        out.push(sql[i..].chars().next().unwrap());
        i += sql[i..].chars().next().unwrap().len_utf8();
    }
    Ok(out)
}

fn decode_show_grants(show: &mut ShowStmt) {
    let Some(table) = show.table.as_ref() else {
        return;
    };
    let name = table.name.clone();
    if name == SHOW_GRANT_MARKER {
        show.kind = ShowStmtKind::Grants;
        show.table = None;
        show.user = None;
        return;
    }
    if let Some(rest) = name
        .strip_prefix(SHOW_GRANT_PREFIX)
        .and_then(|s| s.strip_suffix("__"))
        && let Some((user, host)) = rest.split_once('@')
    {
        show.kind = ShowStmtKind::Grants;
        show.table = None;
        show.user = Some(UserIdentity::new(user, host));
    }
}

fn parse_user_identity_bytes(bytes: &[u8], i: usize) -> Result<(UserIdentity, usize)> {
    let (user, after_user) = parse_quoted_or_ident(bytes, i)?;
    let i = skip_ws(bytes, after_user);
    if i < bytes.len() && bytes[i] == b'@' {
        let (host, end) = parse_quoted_or_ident(bytes, skip_ws(bytes, i + 1))?;
        return Ok((UserIdentity::new(user, host), end));
    }
    Ok((UserIdentity::new(user, "%"), after_user))
}

fn parse_quoted_or_ident(bytes: &[u8], i: usize) -> Result<(String, usize)> {
    if i >= bytes.len() {
        return Err(Error::new(ERR_PARSE_ERROR, "missing user identity"));
    }
    let quote = bytes[i];
    if quote == b'\'' || quote == b'"' || quote == b'`' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != quote {
            j += 1;
        }
        if j >= bytes.len() {
            return Err(Error::new(ERR_PARSE_ERROR, "unclosed user identity"));
        }
        return Ok((
            String::from_utf8_lossy(&bytes[i + 1..j]).into_owned(),
            j + 1,
        ));
    }
    let mut j = i;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j == i {
        return Err(Error::new(ERR_PARSE_ERROR, "missing user identity"));
    }
    Ok((String::from_utf8_lossy(&bytes[i..j]).into_owned(), j))
}

fn lower_analyze(analyze: &sqlparser::ast::Analyze) -> Result<Statement> {
    // gaps: ANALYZE INDEX, PARTITION, columns, NOSCAN, COMPUTE STATISTICS extras.
    if analyze.partitions.as_ref().is_some_and(|p| !p.is_empty()) {
        return Err(Error::unsupported("ANALYZE PARTITION"));
    }
    if analyze.for_columns || !analyze.columns.is_empty() {
        return Err(Error::unsupported("ANALYZE columns"));
    }
    if analyze.noscan || analyze.cache_metadata {
        return Err(Error::unsupported("ANALYZE option"));
    }
    let table = analyze
        .table_name
        .as_ref()
        .ok_or_else(|| Error::unsupported("ANALYZE without table"))?;
    Ok(Statement::AnalyzeTable(AnalyzeTableStmt {
        table: object_table_name(table)?,
        indexes: Vec::new(),
    }))
}

fn lower_grant(grant: &Grant) -> Result<Statement> {
    // WITH GRANT OPTION stored on the grant entry. Gaps: IDENTIFIED BY,
    // column grants.
    if grant.as_grantor.is_some() || grant.granted_by.is_some() || grant.current_grants.is_some() {
        return Err(Error::unsupported("GRANT option"));
    }
    Ok(Statement::Grant(GrantStmt {
        privs: lower_privileges(&grant.privileges)?,
        level: lower_grant_objects(grant.objects.as_ref())?,
        users: lower_grantees(&grant.grantees)?,
        with_grant: grant.with_grant_option,
        dynamic_privs: Vec::new(),
    }))
}

fn lower_revoke(revoke: &Revoke) -> Result<Statement> {
    if revoke.granted_by.is_some() || revoke.cascade.is_some() {
        return Err(Error::unsupported("REVOKE option"));
    }
    Ok(Statement::Revoke(RevokeStmt {
        privs: lower_privileges(&revoke.privileges)?,
        level: lower_grant_objects(revoke.objects.as_ref())?,
        users: lower_grantees(&revoke.grantees)?,
        grant_option: revoke.grant_option_for,
        dynamic_privs: Vec::new(),
    }))
}

fn lower_privileges(privs: &Privileges) -> Result<Vec<PrivElem>> {
    match privs {
        Privileges::All { .. } => Ok(vec![PrivElem {
            tp: PrivilegeType::ALL,
            cols: Vec::new(),
        }]),
        Privileges::Actions(actions) => {
            let mut out = Vec::new();
            for action in actions {
                out.push(lower_priv_elem(action)?);
            }
            if out.is_empty() {
                return Err(Error::unsupported("GRANT without privilege"));
            }
            Ok(out)
        }
    }
}

fn lower_priv_elem(action: &Action) -> Result<PrivElem> {
    match action {
        Action::Select { columns } => Ok(PrivElem {
            tp: PrivilegeType::SELECT,
            cols: lower_priv_columns(columns.as_ref())?,
        }),
        Action::Insert { columns } => Ok(PrivElem {
            tp: PrivilegeType::INSERT,
            cols: lower_priv_columns(columns.as_ref())?,
        }),
        Action::Update { columns } => Ok(PrivElem {
            tp: PrivilegeType::UPDATE,
            cols: lower_priv_columns(columns.as_ref())?,
        }),
        Action::Delete => Ok(PrivElem {
            tp: PrivilegeType::DELETE,
            cols: Vec::new(),
        }),
        Action::Create { obj_type } if obj_type.is_none() => Ok(PrivElem {
            tp: PrivilegeType::CREATE,
            cols: Vec::new(),
        }),
        Action::Drop => Ok(PrivElem {
            tp: PrivilegeType::DROP,
            cols: Vec::new(),
        }),
        Action::Role { .. } => Err(Error::unsupported("GRANT ROLE")),
        other => Err(Error::unsupported(format!("privilege {other}"))),
    }
}

fn lower_priv_columns(columns: Option<&Vec<sqlparser::ast::Ident>>) -> Result<Vec<String>> {
    let Some(cols) = columns else {
        return Ok(Vec::new());
    };
    Ok(cols.iter().map(|c| c.value.clone()).collect())
}

fn lower_grant_objects(objects: Option<&GrantObjects>) -> Result<GrantLevel> {
    let Some(GrantObjects::Tables(names)) = objects else {
        return Err(Error::unsupported("GRANT object type"));
    };
    if names.len() != 1 {
        return Err(Error::unsupported("GRANT multiple objects"));
    }
    let parts = object_name_parts(&names[0])?;
    match parts.as_slice() {
        [a, b] if a == "*" && b == "*" => Ok(GrantLevel {
            level: GrantLevelType::Global,
            db_name: String::new(),
            table_name: String::new(),
        }),
        [a, b] if b == "*" => Ok(GrantLevel {
            level: GrantLevelType::DB,
            db_name: a.clone(),
            table_name: String::new(),
        }),
        [a, b] => Ok(GrantLevel {
            level: GrantLevelType::Table,
            db_name: a.clone(),
            table_name: b.clone(),
        }),
        [a] if a == "*" => Ok(GrantLevel {
            level: GrantLevelType::DB,
            db_name: String::new(),
            table_name: String::new(),
        }),
        [a] => Ok(GrantLevel {
            level: GrantLevelType::Table,
            db_name: String::new(),
            table_name: a.clone(),
        }),
        _ => Err(Error::unsupported("GRANT level")),
    }
}

fn lower_grantees(grantees: &[sqlparser::ast::Grantee]) -> Result<Vec<UserIdentity>> {
    if grantees.is_empty() {
        return Err(Error::unsupported("GRANT without user"));
    }
    let mut users = Vec::new();
    for grantee in grantees {
        if !matches!(
            grantee.grantee_type,
            sqlparser::ast::GranteesType::None | sqlparser::ast::GranteesType::User
        ) {
            return Err(Error::unsupported("GRANT ROLE"));
        }
        match &grantee.name {
            Some(GranteeName::UserHost { user, host }) => {
                users.push(UserIdentity::new(user.value.clone(), host.value.clone()));
            }
            Some(GranteeName::ObjectName(name)) => {
                users.push(UserIdentity::new(object_name_last(name)?, "%"));
            }
            None => return Err(Error::unsupported("GRANT PUBLIC")),
        }
    }
    Ok(users)
}

/// sqlparser MySQL dialect does not accept `LOCK IN SHARE MODE`; rewrite
/// it to `FOR SHARE` (same InnoDB shared lock) before parse.
/// sqlparser's MySQL dialect rejects `AS MATERIALIZED`; strip it like
/// PostgreSQL CTE hints and reattach the hint after lowering.
fn rewrite_cte_materialized(sql: &str) -> (String, Vec<(String, CteMaterialize)>) {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut hints = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "AS") {
            let after_as = skip_ws(bytes, i + 2);
            if let Some((hint, paren)) = cte_materialize_at(bytes, after_as) {
                if let Some(name) = cte_name_before(bytes, i) {
                    hints.push((name, hint));
                }
                out.push_str("AS ");
                i = paren;
                continue;
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    (out, hints)
}

fn cte_materialize_at(bytes: &[u8], i: usize) -> Option<(CteMaterialize, usize)> {
    if keyword_at(bytes, i, "NOT") {
        let after_not = skip_ws(bytes, i + 3);
        if keyword_at(bytes, after_not, "MATERIALIZED") {
            let paren = skip_ws(bytes, after_not + "MATERIALIZED".len());
            if paren < bytes.len() && bytes[paren] == b'(' {
                return Some((CteMaterialize::NotMaterialized, paren));
            }
        }
        return None;
    }
    if keyword_at(bytes, i, "MATERIALIZED") {
        let paren = skip_ws(bytes, i + "MATERIALIZED".len());
        if paren < bytes.len() && bytes[paren] == b'(' {
            return Some((CteMaterialize::Materialized, paren));
        }
    }
    None
}

fn cte_name_before(bytes: &[u8], mut i: usize) -> Option<String> {
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    if bytes[i - 1] == b')' {
        let mut depth = 0;
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
    }
    if i == 0 {
        return None;
    }
    if matches!(bytes[i - 1], b'`' | b'"' | b'\'') {
        let q = bytes[i - 1];
        i -= 1;
        let end = i;
        while i > 0 && bytes[i - 1] != q {
            i -= 1;
        }
        let start = i;
        return Some(String::from_utf8_lossy(&bytes[start..end]).into_owned());
    }
    let end = i;
    while i > 0 && is_ident_byte(bytes[i - 1]) {
        i -= 1;
    }
    let name = String::from_utf8_lossy(&bytes[i..end]).into_owned();
    if name.is_empty() { None } else { Some(name) }
}

fn apply_cte_materialize(stmt: &mut Statement, hints: &mut Vec<(String, CteMaterialize)>) {
    match stmt {
        Statement::Select(select) => apply_with_materialize(&mut select.with, hints),
        Statement::SetOpr(set) => {
            apply_with_materialize(&mut set.with, hints);
            apply_with_materialize(&mut set.first.with, hints);
            for (_, select) in &mut set.rest {
                apply_with_materialize(&mut select.with, hints);
            }
        }
        Statement::Explain(explain) => apply_cte_materialize(&mut explain.stmt, hints),
        _ => {}
    }
}

fn apply_with_materialize(
    with: &mut Option<WithClause>,
    hints: &mut Vec<(String, CteMaterialize)>,
) {
    let Some(with) = with else {
        return;
    };
    for cte in &mut with.ctes {
        if let Some(i) = hints
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(&cte.name))
        {
            cte.materialize = hints.remove(i).1;
        }
    }
}

fn rewrite_lock_in_share_mode(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "LOCK") {
            let after_lock = skip_ws(bytes, i + 4);
            if keyword_at(bytes, after_lock, "IN") {
                let after_in = skip_ws(bytes, after_lock + 2);
                if keyword_at(bytes, after_in, "SHARE") {
                    let after_share = skip_ws(bytes, after_in + 5);
                    if keyword_at(bytes, after_share, "MODE") {
                        out.push_str("FOR SHARE");
                        i = after_share + 4;
                        continue;
                    }
                }
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn rewrite_begin_pessimistic(sql: &str) -> (String, Vec<bool>) {
    let mut out = String::with_capacity(sql.len());
    let mut flags = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "START") {
            let after_start = skip_ws(bytes, i + 5);
            if keyword_at(bytes, after_start, "TRANSACTION") {
                let after_txn = skip_ws(bytes, after_start + "TRANSACTION".len());
                let pessimistic = keyword_at(bytes, after_txn, "PESSIMISTIC");
                flags.push(pessimistic);
                out.push_str(&sql[i..after_start + "TRANSACTION".len()]);
                i = if pessimistic {
                    after_txn + "PESSIMISTIC".len()
                } else {
                    after_start + "TRANSACTION".len()
                };
                continue;
            }
        }
        if keyword_at(bytes, i, "BEGIN") {
            let after = skip_ws(bytes, i + 5);
            let pessimistic = keyword_at(bytes, after, "PESSIMISTIC");
            flags.push(pessimistic);
            out.push_str("BEGIN");
            i = if pessimistic {
                after + "PESSIMISTIC".len()
            } else {
                i + 5
            };
            continue;
        }
        out.push(sql[i..].chars().next().unwrap());
        i += sql[i..].chars().next().unwrap().len_utf8();
    }
    (out, flags)
}

fn keyword_at(bytes: &[u8], i: usize, keyword: &str) -> bool {
    let end = i + keyword.len();
    if end > bytes.len() {
        return false;
    }
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return false;
    }
    if end < bytes.len() && is_ident_byte(bytes[end]) {
        return false;
    }
    bytes[i..end].eq_ignore_ascii_case(keyword.as_bytes())
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Copy one UTF-8 character from `sql` at byte offset `i` into `out`.
/// Rewrites must not use `bytes[i] as char` — that Latin-1-recodes multi-byte UTF-8.
fn push_sql_char(out: &mut String, sql: &str, i: usize) -> usize {
    let ch = sql[i..].chars().next().expect("sql index in bounds");
    out.push(ch);
    i + ch.len_utf8()
}

/// sqlparser rejects TiDB `ANALYZE TABLE t INDEX idx [, idx2]`.
/// Strip the INDEX clause and re-attach index names after lower.
fn extract_analyze_index(sql: &str) -> Result<(String, Vec<Option<Vec<String>>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut indexes = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "ANALYZE") {
            let after = skip_ws(bytes, i + "ANALYZE".len());
            if keyword_at(bytes, after, "TABLE") {
                let mut j = skip_ws(bytes, after + "TABLE".len());
                j = skip_ident_or_quoted(bytes, j);
                j = skip_ws(bytes, j);
                if j < bytes.len() && bytes[j] == b'.' {
                    j = skip_ident_or_quoted(bytes, skip_ws(bytes, j + 1));
                    j = skip_ws(bytes, j);
                }
                if keyword_at(bytes, j, "INDEX") {
                    let mut k = skip_ws(bytes, j + "INDEX".len());
                    let mut names = Vec::new();
                    loop {
                        let (name, next) = read_quoted_or_ident(bytes, k);
                        if name.is_empty() {
                            break;
                        }
                        names.push(name);
                        k = skip_ws(bytes, next);
                        if k < bytes.len() && bytes[k] == b',' {
                            k = skip_ws(bytes, k + 1);
                            continue;
                        }
                        break;
                    }
                    out.push_str(&sql[i..j]);
                    indexes.push(Some(names));
                    i = k;
                    continue;
                }
                out.push_str(&sql[i..j]);
                indexes.push(None);
                i = j;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok((out, indexes))
}

/// sqlparser rejects MySQL `CREATE/DROP USER 'u'@'%'`.
fn extract_user_ddl(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "CREATE") {
            let after = skip_ws(bytes, i + "CREATE".len());
            if keyword_at(bytes, after, "USER") {
                let (stmt, end) = parse_create_user(bytes, after + "USER".len())?;
                out.push_str("SET NAMES '__sealdb_user__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "DROP") {
            let after = skip_ws(bytes, i + "DROP".len());
            if keyword_at(bytes, after, "USER") {
                let (stmt, end) = parse_drop_user(bytes, after + "USER".len())?;
                out.push_str("SET NAMES '__sealdb_user__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok((out, ops))
}

/// Extract MySQL/TiDB stmts sqlparser lacks or mishandles: `DO`, `SET PASSWORD`,
/// `SHUTDOWN`, `ALTER DATABASE … CHARSET`, leftover allowlist ops (OPTIMIZE /
/// REPAIR / RENAME USER / BINLOG / CHANGE / ALTER INSTANCE / LOCK TABLES with
/// schema-qualified names / ADMIN CLEANUP).
fn extract_misc_allowlist(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "DO") {
            let (stmt, end) = parse_do_stmt(bytes, i + "DO".len())?;
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(stmt));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "SHUTDOWN") {
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(Statement::Shutdown(ShutdownStmt)));
            i = skip_ws(bytes, i + "SHUTDOWN".len());
            continue;
        }
        if keyword_at(bytes, i, "TRACE") {
            let (stmt, end) = parse_trace_stmt(bytes, i + "TRACE".len())?;
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(stmt));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "PLAN") {
            let after = skip_ws(bytes, i + "PLAN".len());
            if keyword_at(bytes, after, "REPLAYER") {
                let (stmt, end) = parse_plan_replayer_stmt(bytes, after + "REPLAYER".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "BACKUP") || keyword_at(bytes, i, "RESTORE") {
            let (raw_sql, end) = capture_raw_stmt(bytes, i);
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(Statement::BRIE(BRIEStmt { raw_sql })));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "IMPORT") {
            let after = skip_ws(bytes, i + "IMPORT".len());
            if keyword_at(bytes, after, "INTO") {
                let (raw_sql, end) = capture_raw_stmt(bytes, i);
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::ImportInto(ImportIntoStmt { raw_sql })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "SPLIT") {
            let (raw_sql, end) = capture_raw_stmt(bytes, i);
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(Statement::SplitRegion(SplitRegionStmt { raw_sql })));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "DISTRIBUTE") {
            let (raw_sql, end) = capture_raw_stmt(bytes, i);
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(Statement::DistributeTable(DistributeTableStmt {
                raw_sql,
            })));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "RECOMMEND") {
            let after = skip_ws(bytes, i + "RECOMMEND".len());
            if keyword_at(bytes, after, "INDEX") {
                let (raw_sql, end) = capture_raw_stmt(bytes, i);
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::RecommendIndex(RecommendIndexStmt {
                    raw_sql,
                })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "CALIBRATE") {
            let after = skip_ws(bytes, i + "CALIBRATE".len());
            if keyword_at(bytes, after, "RESOURCE") {
                let (raw_sql, end) = capture_raw_stmt(bytes, i);
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::CalibrateResource(CalibrateResourceStmt {
                    raw_sql,
                })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "BINLOG") {
            let (stmt, end) = parse_binlog_stmt(bytes, i + "BINLOG".len())?;
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(stmt));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "CHANGE") {
            let after = skip_ws(bytes, i + "CHANGE".len());
            // TiDB `CHANGE PUMP/DRAINER …` only — skip `ALTER TABLE … CHANGE COLUMN`.
            if keyword_at(bytes, after, "PUMP") || keyword_at(bytes, after, "DRAINER") {
                let (stmt, end) = parse_change_stmt(bytes, i + "CHANGE".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "LOAD") {
            let after = skip_ws(bytes, i + "LOAD".len());
            if keyword_at(bytes, after, "STATS") {
                let j = skip_ws(bytes, after + "STATS".len());
                let (path, end) = read_sql_string(bytes, j)?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::LoadStats(LoadStatsStmt { path })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "LOCK") {
            let after = skip_ws(bytes, i + "LOCK".len());
            if keyword_at(bytes, after, "STATS") {
                let j = skip_ws(bytes, after + "STATS".len());
                let (tables, end) = parse_table_name_list(bytes, j)?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::LockStats(LockStatsStmt { tables })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "UNLOCK") {
            let after = skip_ws(bytes, i + "UNLOCK".len());
            if keyword_at(bytes, after, "STATS") {
                let j = skip_ws(bytes, after + "STATS".len());
                let (tables, end) = parse_table_name_list(bytes, j)?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::UnlockStats(UnlockStatsStmt { tables })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "OPTIMIZE") {
            let (stmt, end) = parse_optimize_table(bytes, i + "OPTIMIZE".len())?;
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(stmt));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "REPAIR") {
            let (stmt, end) = parse_repair_table(bytes, i + "REPAIR".len())?;
            out.push_str("SET NAMES '__sealdb_misc__'");
            ops.push(Some(stmt));
            i = end;
            continue;
        }
        if keyword_at(bytes, i, "LOCK") {
            let after = skip_ws(bytes, i + "LOCK".len());
            if keyword_at(bytes, after, "TABLES") || keyword_at(bytes, after, "TABLE") {
                let kw = if keyword_at(bytes, after, "TABLES") {
                    "TABLES".len()
                } else {
                    "TABLE".len()
                };
                let (stmt, end) = parse_lock_tables(bytes, after + kw)?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "UNLOCK") {
            let after = skip_ws(bytes, i + "UNLOCK".len());
            if keyword_at(bytes, after, "TABLES") || keyword_at(bytes, after, "TABLE") {
                let kw = if keyword_at(bytes, after, "TABLES") {
                    "TABLES".len()
                } else {
                    "TABLE".len()
                };
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(Statement::CleanupTableLock(CleanupTableLockStmt {
                    tables: Vec::new(),
                })));
                i = skip_ws(bytes, after + kw);
                continue;
            }
        }
        if keyword_at(bytes, i, "ADMIN") {
            let after = skip_ws(bytes, i + "ADMIN".len());
            if keyword_at(bytes, after, "CLEANUP") {
                let (stmt, end) = parse_admin_cleanup_table_lock(bytes, after + "CLEANUP".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "SET") {
            let after = skip_ws(bytes, i + "SET".len());
            if keyword_at(bytes, after, "PASSWORD") {
                let (stmt, end) = parse_set_password(bytes, after + "PASSWORD".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "RENAME") {
            let after = skip_ws(bytes, i + "RENAME".len());
            if keyword_at(bytes, after, "USER") {
                let (stmt, end) = parse_rename_user(bytes, after + "USER".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "ALTER") {
            let after = skip_ws(bytes, i + "ALTER".len());
            if keyword_at(bytes, after, "DATABASE") || keyword_at(bytes, after, "SCHEMA") {
                let kw_len = if keyword_at(bytes, after, "DATABASE") {
                    "DATABASE".len()
                } else {
                    "SCHEMA".len()
                };
                let (stmt, end) = parse_alter_database(bytes, after + kw_len)?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
            if keyword_at(bytes, after, "INSTANCE") {
                let (stmt, end) = parse_alter_instance(bytes, after + "INSTANCE".len())?;
                out.push_str("SET NAMES '__sealdb_misc__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
            if keyword_at(bytes, after, "TABLE") {
                let end = skip_stmt_tail(bytes, i);
                if stmt_contains_keyword(&bytes[i..end], "COMPACT") {
                    let raw_sql = String::from_utf8_lossy(&bytes[i..end]).trim().to_string();
                    out.push_str("SET NAMES '__sealdb_misc__'");
                    ops.push(Some(Statement::CompactTable(CompactTableStmt { raw_sql })));
                    i = end;
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok((out, ops))
}

fn stmt_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b';' => return i,
            _ => i += 1,
        }
    }
    i
}

fn parse_do_stmt(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let end = stmt_end(bytes, i);
    let expr_sql = String::from_utf8_lossy(&bytes[i..end]).trim().to_string();
    if expr_sql.is_empty() {
        return Err(Error::unsupported("DO without expressions"));
    }
    let select_sql = format!("SELECT {expr_sql}");
    let parsed = Parser::parse_sql(&dialect(), &select_sql).map_err(|e| {
        Error::new(
            ERR_PARSE_ERROR,
            format!("You have an error in your SQL syntax: {e}"),
        )
    })?;
    let first = parsed
        .first()
        .ok_or_else(|| Error::unsupported("DO without expressions"))?;
    let lowered = lower_statement(first)?;
    let exprs = match lowered {
        Statement::Select(select) => select.fields.into_iter().map(|f| f.expr).collect(),
        other => {
            return Err(Error::unsupported(format!("DO body {}", other.stmt_type())));
        }
    };
    Ok((Statement::Do(DoStmt { exprs }), end))
}

fn parse_set_password(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let mut user = None;
    if keyword_at(bytes, i, "FOR") {
        i = skip_ws(bytes, i + "FOR".len());
        let (ident, after) = read_user_identity(bytes, i)?;
        user = Some(ident);
        i = skip_ws(bytes, after);
    }
    if i >= bytes.len() || bytes[i] != b'=' {
        return Err(Error::unsupported("SET PASSWORD without ="));
    }
    i = skip_ws(bytes, i + 1);
    let (password, after) = read_quoted_or_ident(bytes, i);
    Ok((
        Statement::SetPwd(SetPwdStmt { user, password }),
        skip_ws(bytes, after),
    ))
}

fn parse_binlog_stmt(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let (payload, after) = read_quoted_or_ident(bytes, i);
    Ok((
        Statement::Binlog(BinlogStmt { payload }),
        skip_ws(bytes, after),
    ))
}

fn capture_raw_stmt(bytes: &[u8], start: usize) -> (String, usize) {
    let end = skip_stmt_tail(bytes, start);
    let raw_sql = String::from_utf8_lossy(&bytes[start..end]).trim().to_string();
    (raw_sql, end)
}

fn stmt_contains_keyword(bytes: &[u8], keyword: &str) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, keyword) {
            return true;
        }
        // Skip string / ident literals so embedded text does not false-match.
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let q = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    false
}

/// Advance past one statement body (quotes / parens aware) up to `;` or EOF.
fn skip_stmt_tail(bytes: &[u8], mut i: usize) -> usize {
    let mut depth = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut in_bt = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_sq {
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_sq = false;
            } else if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_dq {
            if c == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_dq = false;
            } else if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_bt {
            if c == b'`' {
                in_bt = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_sq = true,
            b'"' => in_dq = true,
            b'`' => in_bt = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    i
}

fn parse_trace_stmt(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let mut format = "row".to_string();
    let mut trace_plan = false;
    if keyword_at(bytes, i, "PLAN") {
        trace_plan = true;
        i = skip_ws(bytes, i + "PLAN".len());
        if keyword_at(bytes, i, "TARGET") {
            i = skip_ws(bytes, i + "TARGET".len());
            if i < bytes.len() && bytes[i] == b'=' {
                i = skip_ws(bytes, i + 1);
            }
            let (_target, after) = read_quoted_or_ident(bytes, i);
            i = skip_ws(bytes, after);
        }
    }
    if keyword_at(bytes, i, "FORMAT") {
        i = skip_ws(bytes, i + "FORMAT".len());
        if i < bytes.len() && bytes[i] == b'=' {
            i = skip_ws(bytes, i + 1);
        }
        let (fmt, after) = read_quoted_or_ident(bytes, i);
        format = fmt;
        i = skip_ws(bytes, after);
    }
    let end = skip_stmt_tail(bytes, i);
    let stmt_sql = String::from_utf8_lossy(&bytes[i..end]).trim().to_string();
    if stmt_sql.is_empty() {
        return Err(Error::unsupported("TRACE without statement"));
    }
    Ok((
        Statement::Trace(TraceStmt {
            format,
            trace_plan,
            stmt_sql,
        }),
        end,
    ))
}

fn parse_plan_replayer_stmt(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if keyword_at(bytes, i, "LOAD") {
        i = skip_ws(bytes, i + "LOAD".len());
        let (file, after) = read_sql_string(bytes, i)?;
        return Ok((
            Statement::PlanReplayer(PlanReplayerStmt {
                load: true,
                capture: false,
                remove: false,
                analyze: false,
                file,
                sql_digest: String::new(),
                plan_digest: String::new(),
                stmt_sql: String::new(),
            }),
            skip_ws(bytes, after),
        ));
    }
    if keyword_at(bytes, i, "CAPTURE") {
        i = skip_ws(bytes, i + "CAPTURE".len());
        let mut remove = false;
        if keyword_at(bytes, i, "REMOVE") {
            remove = true;
            i = skip_ws(bytes, i + "REMOVE".len());
        }
        let (sql_digest, after_sql) = read_sql_string(bytes, i)?;
        i = skip_ws(bytes, after_sql);
        let (plan_digest, after_plan) = read_sql_string(bytes, i)?;
        return Ok((
            Statement::PlanReplayer(PlanReplayerStmt {
                load: false,
                capture: true,
                remove,
                analyze: false,
                file: String::new(),
                sql_digest,
                plan_digest,
                stmt_sql: String::new(),
            }),
            skip_ws(bytes, after_plan),
        ));
    }
    // DUMP [EXPLAIN] [ANALYZE] … | DUMP EXPLAIN ANALYZE 'file'
    let mut analyze = false;
    if keyword_at(bytes, i, "DUMP") {
        i = skip_ws(bytes, i + "DUMP".len());
    }
    if keyword_at(bytes, i, "EXPLAIN") {
        i = skip_ws(bytes, i + "EXPLAIN".len());
    }
    if keyword_at(bytes, i, "ANALYZE") {
        analyze = true;
        i = skip_ws(bytes, i + "ANALYZE".len());
    }
    i = skip_ws(bytes, i);
    if i < bytes.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
        let (file, after) = read_sql_string(bytes, i)?;
        return Ok((
            Statement::PlanReplayer(PlanReplayerStmt {
                load: false,
                capture: false,
                remove: false,
                analyze,
                file,
                sql_digest: String::new(),
                plan_digest: String::new(),
                stmt_sql: String::new(),
            }),
            skip_ws(bytes, after),
        ));
    }
    let end = skip_stmt_tail(bytes, i);
    let stmt_sql = String::from_utf8_lossy(&bytes[i..end]).trim().to_string();
    Ok((
        Statement::PlanReplayer(PlanReplayerStmt {
            load: false,
            capture: false,
            remove: false,
            analyze,
            file: String::new(),
            sql_digest: String::new(),
            plan_digest: String::new(),
            stmt_sql,
        }),
        end,
    ))
}

fn parse_change_stmt(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let (node_type, after_type) = read_quoted_or_ident(bytes, i);
    if node_type.is_empty() {
        return Err(Error::unsupported("CHANGE without node type"));
    }
    i = skip_ws(bytes, after_type);
    if !keyword_at(bytes, i, "TO") {
        return Err(Error::unsupported("CHANGE without TO"));
    }
    i = skip_ws(bytes, i + "TO".len());
    if !keyword_at(bytes, i, "NODE_STATE") {
        return Err(Error::unsupported("CHANGE without NODE_STATE"));
    }
    i = skip_ws(bytes, i + "NODE_STATE".len());
    if i < bytes.len() && bytes[i] == b'=' {
        i = skip_ws(bytes, i + 1);
    }
    let (state, after_state) = read_quoted_or_ident(bytes, i);
    i = skip_ws(bytes, after_state);
    if !keyword_at(bytes, i, "FOR") {
        return Err(Error::unsupported("CHANGE without FOR NODE_ID"));
    }
    i = skip_ws(bytes, i + "FOR".len());
    if !keyword_at(bytes, i, "NODE_ID") {
        return Err(Error::unsupported("CHANGE without NODE_ID"));
    }
    i = skip_ws(bytes, i + "NODE_ID".len());
    let (node_id, after_id) = read_quoted_or_ident(bytes, i);
    Ok((
        Statement::Change(ChangeStmt {
            node_type,
            state,
            node_id,
        }),
        skip_ws(bytes, after_id),
    ))
}

fn parse_optimize_table(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if keyword_at(bytes, i, "NO_WRITE_TO_BINLOG") || keyword_at(bytes, i, "LOCAL") {
        let kw = if keyword_at(bytes, i, "LOCAL") {
            "LOCAL".len()
        } else {
            "NO_WRITE_TO_BINLOG".len()
        };
        i = skip_ws(bytes, i + kw);
    }
    if !keyword_at(bytes, i, "TABLE") {
        return Err(Error::unsupported("OPTIMIZE without TABLE"));
    }
    i = skip_ws(bytes, i + "TABLE".len());
    let (tables, end) = parse_table_name_list(bytes, i)?;
    Ok((Statement::OptimizeTable(OptimizeTableStmt { tables }), end))
}

fn parse_repair_table(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if keyword_at(bytes, i, "NO_WRITE_TO_BINLOG") || keyword_at(bytes, i, "LOCAL") {
        let kw = if keyword_at(bytes, i, "LOCAL") {
            "LOCAL".len()
        } else {
            "NO_WRITE_TO_BINLOG".len()
        };
        i = skip_ws(bytes, i + kw);
    }
    if !keyword_at(bytes, i, "TABLE") {
        return Err(Error::unsupported("REPAIR without TABLE"));
    }
    i = skip_ws(bytes, i + "TABLE".len());
    let (tables, end) = parse_table_name_list(bytes, i)?;
    Ok((Statement::RepairTable(RepairTableStmt { tables }), end))
}

fn parse_lock_tables(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    let mut table_locks = Vec::new();
    loop {
        i = skip_ws(bytes, i);
        let (table, after_table) = parse_table_name(bytes, i)?;
        i = skip_ws(bytes, after_table);
        // Optional AS alias.
        if keyword_at(bytes, i, "AS") {
            i = skip_ws(bytes, i + "AS".len());
            let (_alias, after_alias) = read_quoted_or_ident(bytes, i);
            i = skip_ws(bytes, after_alias);
        } else if i < bytes.len()
            && is_ident_byte(bytes[i])
            && !keyword_at(bytes, i, "READ")
            && !keyword_at(bytes, i, "WRITE")
            && !keyword_at(bytes, i, "LOW_PRIORITY")
        {
            let (_alias, after_alias) = read_quoted_or_ident(bytes, i);
            i = skip_ws(bytes, after_alias);
        }
        let lock_type = if keyword_at(bytes, i, "READ") {
            i = skip_ws(bytes, i + "READ".len());
            if keyword_at(bytes, i, "LOCAL") {
                i = skip_ws(bytes, i + "LOCAL".len());
            }
            TableLockType::Read
        } else if keyword_at(bytes, i, "LOW_PRIORITY") {
            i = skip_ws(bytes, i + "LOW_PRIORITY".len());
            if !keyword_at(bytes, i, "WRITE") {
                return Err(Error::unsupported("LOCK TABLES LOW_PRIORITY without WRITE"));
            }
            i = skip_ws(bytes, i + "WRITE".len());
            TableLockType::Write
        } else if keyword_at(bytes, i, "WRITE") {
            i = skip_ws(bytes, i + "WRITE".len());
            TableLockType::Write
        } else {
            return Err(Error::unsupported("LOCK TABLES without READ/WRITE"));
        };
        table_locks.push(TableLockItem { table, lock_type });
        i = skip_ws(bytes, i);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    Ok((Statement::LockTables(LockTablesStmt { table_locks }), i))
}

fn parse_admin_cleanup_table_lock(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    if !keyword_at(bytes, i, "TABLE") {
        return Err(Error::unsupported("ADMIN CLEANUP without TABLE"));
    }
    i = skip_ws(bytes, i + "TABLE".len());
    if !keyword_at(bytes, i, "LOCK") {
        return Err(Error::unsupported("ADMIN CLEANUP TABLE without LOCK"));
    }
    i = skip_ws(bytes, i + "LOCK".len());
    let (tables, end) = parse_table_name_list(bytes, i)?;
    Ok((
        Statement::CleanupTableLock(CleanupTableLockStmt { tables }),
        end,
    ))
}

fn parse_rename_user(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    let mut user_to_users = Vec::new();
    loop {
        i = skip_ws(bytes, i);
        let (old_user, after_old) = read_user_identity(bytes, i)?;
        i = skip_ws(bytes, after_old);
        if !keyword_at(bytes, i, "TO") {
            return Err(Error::unsupported("RENAME USER without TO"));
        }
        i = skip_ws(bytes, i + "TO".len());
        let (new_user, after_new) = read_user_identity(bytes, i)?;
        user_to_users.push(UserToUser { old_user, new_user });
        i = skip_ws(bytes, after_new);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    Ok((Statement::RenameUser(RenameUserStmt { user_to_users }), i))
}

fn parse_alter_instance(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let mut reload_tls = false;
    let mut no_rollback_on_error = false;
    while i < bytes.len() && bytes[i] != b';' {
        i = skip_ws(bytes, i);
        if i >= bytes.len() || bytes[i] == b';' {
            break;
        }
        if keyword_at(bytes, i, "RELOAD") {
            i = skip_ws(bytes, i + "RELOAD".len());
            if !keyword_at(bytes, i, "TLS") {
                return Err(Error::unsupported("ALTER INSTANCE RELOAD without TLS"));
            }
            i = skip_ws(bytes, i + "TLS".len());
            reload_tls = true;
            continue;
        }
        if keyword_at(bytes, i, "NO") {
            i = skip_ws(bytes, i + "NO".len());
            if !keyword_at(bytes, i, "ROLLBACK") {
                return Err(Error::unsupported("ALTER INSTANCE NO without ROLLBACK"));
            }
            i = skip_ws(bytes, i + "ROLLBACK".len());
            if !keyword_at(bytes, i, "ON") {
                return Err(Error::unsupported("ALTER INSTANCE NO ROLLBACK without ON"));
            }
            i = skip_ws(bytes, i + "ON".len());
            if !keyword_at(bytes, i, "ERROR") {
                return Err(Error::unsupported(
                    "ALTER INSTANCE NO ROLLBACK ON without ERROR",
                ));
            }
            i = skip_ws(bytes, i + "ERROR".len());
            no_rollback_on_error = true;
            continue;
        }
        break;
    }
    if !reload_tls {
        return Err(Error::unsupported("ALTER INSTANCE without RELOAD TLS"));
    }
    Ok((
        Statement::AlterInstance(AlterInstanceStmt {
            reload_tls,
            no_rollback_on_error,
        }),
        i,
    ))
}

fn parse_table_name(bytes: &[u8], mut i: usize) -> Result<(TableName, usize)> {
    i = skip_ws(bytes, i);
    let (first, after_first) = read_quoted_or_ident(bytes, i);
    if first.is_empty() {
        return Err(Error::new(ERR_PARSE_ERROR, "missing table name"));
    }
    let mut j = skip_ws(bytes, after_first);
    if j < bytes.len() && bytes[j] == b'.' {
        j = skip_ws(bytes, j + 1);
        let (second, after_second) = read_quoted_or_ident(bytes, j);
        if second.is_empty() {
            return Err(Error::new(ERR_PARSE_ERROR, "missing table name after ."));
        }
        return Ok((TableName::new(Some(first), second), after_second));
    }
    Ok((TableName::new(None, first), after_first))
}

fn parse_table_name_list(bytes: &[u8], mut i: usize) -> Result<(Vec<TableName>, usize)> {
    let mut tables = Vec::new();
    loop {
        let (table, after) = parse_table_name(bytes, i)?;
        tables.push(table);
        i = skip_ws(bytes, after);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    Ok((tables, i))
}

fn parse_alter_database(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let end = stmt_end(bytes, i);
    let mut name = String::new();
    // Optional db name before CHARACTER/COLLATE.
    if i < end
        && !keyword_at(bytes, i, "CHARACTER")
        && !keyword_at(bytes, i, "CHARSET")
        && !keyword_at(bytes, i, "COLLATE")
        && !keyword_at(bytes, i, "DEFAULT")
    {
        let (ident, after) = read_quoted_or_ident(bytes, i);
        name = ident;
        i = skip_ws(bytes, after);
    }
    let mut charset = None;
    let mut collate = None;
    while i < end {
        i = skip_ws(bytes, i);
        if i >= end {
            break;
        }
        if keyword_at(bytes, i, "DEFAULT") {
            i = skip_ws(bytes, i + "DEFAULT".len());
            continue;
        }
        if keyword_at(bytes, i, "CHARACTER") {
            i = skip_ws(bytes, i + "CHARACTER".len());
            if !keyword_at(bytes, i, "SET") {
                return Err(Error::unsupported("ALTER DATABASE CHARACTER without SET"));
            }
            i = skip_ws(bytes, i + "SET".len());
            if i < end && bytes[i] == b'=' {
                i = skip_ws(bytes, i + 1);
            }
            let (cs, after) = read_quoted_or_ident(bytes, i);
            charset = Some(cs);
            i = after;
            continue;
        }
        if keyword_at(bytes, i, "CHARSET") {
            i = skip_ws(bytes, i + "CHARSET".len());
            if i < end && bytes[i] == b'=' {
                i = skip_ws(bytes, i + 1);
            }
            let (cs, after) = read_quoted_or_ident(bytes, i);
            charset = Some(cs);
            i = after;
            continue;
        }
        if keyword_at(bytes, i, "COLLATE") {
            i = skip_ws(bytes, i + "COLLATE".len());
            if i < end && bytes[i] == b'=' {
                i = skip_ws(bytes, i + 1);
            }
            let (coll, after) = read_quoted_or_ident(bytes, i);
            collate = Some(coll);
            i = after;
            continue;
        }
        return Err(Error::unsupported("ALTER DATABASE option"));
    }
    if charset.is_none() && collate.is_none() {
        return Err(Error::unsupported("ALTER DATABASE without options"));
    }
    Ok((
        Statement::AlterDatabase(AlterDatabaseStmt {
            name,
            charset,
            collate,
        }),
        end,
    ))
}

fn parse_create_user(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let if_not_exists = if keyword_at(bytes, i, "IF") {
        let after_if = skip_ws(bytes, i + 2);
        if keyword_at(bytes, after_if, "NOT") {
            let after_not = skip_ws(bytes, after_if + 3);
            if keyword_at(bytes, after_not, "EXISTS") {
                i = skip_ws(bytes, after_not + "EXISTS".len());
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    let mut users = Vec::new();
    loop {
        i = skip_ws(bytes, i);
        let (user, after_user) = read_user_identity(bytes, i)?;
        i = skip_ws(bytes, after_user);
        let mut auth = None;
        if keyword_at(bytes, i, "IDENTIFIED") {
            i = skip_ws(bytes, i + "IDENTIFIED".len());
            if !keyword_at(bytes, i, "BY") {
                return Err(Error::unsupported("IDENTIFIED without BY"));
            }
            i = skip_ws(bytes, i + "BY".len());
            let (pwd, after_pwd) = read_quoted_or_ident(bytes, i);
            auth = Some(pwd);
            i = skip_ws(bytes, after_pwd);
        }
        users.push(UserSpec { user, auth });
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    if users.is_empty() {
        return Err(Error::unsupported("CREATE USER without users"));
    }
    Ok((
        Statement::CreateUser(CreateUserStmt {
            if_not_exists,
            users,
        }),
        i,
    ))
}

fn parse_drop_user(bytes: &[u8], mut i: usize) -> Result<(Statement, usize)> {
    i = skip_ws(bytes, i);
    let if_exists = if keyword_at(bytes, i, "IF") {
        let after_if = skip_ws(bytes, i + 2);
        if keyword_at(bytes, after_if, "EXISTS") {
            i = skip_ws(bytes, after_if + "EXISTS".len());
            true
        } else {
            false
        }
    } else {
        false
    };
    let (users, end) = parse_user_list(bytes, i)?;
    if users.is_empty() {
        return Err(Error::unsupported("DROP USER without users"));
    }
    Ok((Statement::DropUser(DropUserStmt { if_exists, users }), end))
}

/// sqlparser rejects MySQL `GRANT role TO user` / dynamic privs like
/// `GRANT BACKUP_ADMIN ON *.*`. Replace with `SET NAMES '__sealdb_role__'`
/// and re-attach after lower.
fn extract_role_grants(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "GRANT") {
            if let Some((stmt, end)) = try_parse_grant_role(bytes, i)? {
                out.push_str("SET NAMES '__sealdb_role__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
            if let Some((stmt, end)) = try_parse_dynamic_grant(bytes, i)? {
                out.push_str("SET NAMES '__sealdb_role__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "REVOKE") {
            if let Some((stmt, end)) = try_parse_revoke_role(bytes, i)? {
                out.push_str("SET NAMES '__sealdb_role__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
            if let Some((stmt, end)) = try_parse_dynamic_revoke(bytes, i)? {
                out.push_str("SET NAMES '__sealdb_role__'");
                ops.push(Some(stmt));
                i = end;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok((out, ops))
}

fn try_parse_dynamic_grant(bytes: &[u8], start: usize) -> Result<Option<(Statement, usize)>> {
    let mut i = skip_ws(bytes, start + "GRANT".len());
    let (privs, after_privs) = parse_dynamic_priv_list(bytes, i)?;
    if privs.is_empty() {
        return Ok(None);
    }
    i = skip_ws(bytes, after_privs);
    if !keyword_at(bytes, i, "ON") {
        return Ok(None);
    }
    i = skip_ws(bytes, i + "ON".len());
    // Dynamic privileges are global (`*.*`).
    if !(i + 2 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'.' && bytes[i + 2] == b'*') {
        return Ok(None);
    }
    i = skip_ws(bytes, i + 3);
    if !keyword_at(bytes, i, "TO") {
        return Ok(None);
    }
    i = skip_ws(bytes, i + "TO".len());
    let (users, end) = parse_user_list(bytes, i)?;
    Ok(Some((
        Statement::Grant(GrantStmt {
            privs: Vec::new(),
            level: GrantLevel {
                level: GrantLevelType::Global,
                db_name: String::new(),
                table_name: String::new(),
            },
            users,
            with_grant: false,
            dynamic_privs: privs,
        }),
        end,
    )))
}

fn try_parse_dynamic_revoke(bytes: &[u8], start: usize) -> Result<Option<(Statement, usize)>> {
    let mut i = skip_ws(bytes, start + "REVOKE".len());
    let (privs, after_privs) = parse_dynamic_priv_list(bytes, i)?;
    if privs.is_empty() {
        return Ok(None);
    }
    i = skip_ws(bytes, after_privs);
    if !keyword_at(bytes, i, "ON") {
        return Ok(None);
    }
    i = skip_ws(bytes, i + "ON".len());
    if !(i + 2 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'.' && bytes[i + 2] == b'*') {
        return Ok(None);
    }
    i = skip_ws(bytes, i + 3);
    if !keyword_at(bytes, i, "FROM") {
        return Ok(None);
    }
    i = skip_ws(bytes, i + "FROM".len());
    let (users, end) = parse_user_list(bytes, i)?;
    Ok(Some((
        Statement::Revoke(RevokeStmt {
            privs: Vec::new(),
            level: GrantLevel {
                level: GrantLevelType::Global,
                db_name: String::new(),
                table_name: String::new(),
            },
            users,
            grant_option: false,
            dynamic_privs: privs,
        }),
        end,
    )))
}

fn parse_dynamic_priv_list(bytes: &[u8], mut i: usize) -> Result<(Vec<String>, usize)> {
    let mut privs = Vec::new();
    loop {
        i = skip_ws(bytes, i);
        let (name, next) = read_quoted_or_ident(bytes, i);
        if name.is_empty() || !is_dynamic_priv(&name) {
            break;
        }
        privs.push(name.to_ascii_uppercase());
        i = skip_ws(bytes, next);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    Ok((privs, i))
}

fn is_dynamic_priv(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BACKUP_ADMIN"
            | "RESTORE_ADMIN"
            | "SYSTEM_VARIABLES_ADMIN"
            | "ROLE_ADMIN"
            | "CONNECTION_ADMIN"
            | "RESTRICTED_TABLES_ADMIN"
            | "RESTRICTED_USER_ADMIN"
            | "RESTRICTED_STATUS_ADMIN"
            | "RESTRICTED_VARIABLES_ADMIN"
            | "RESTRICTED_REPLICA_WRITER_ADMIN"
            | "RESTRICTED_PRIVILEGES_ADMIN"
    )
}

fn try_parse_grant_role(bytes: &[u8], start: usize) -> Result<Option<(Statement, usize)>> {
    let mut i = skip_ws(bytes, start + "GRANT".len());
    // Privilege GRANT has ON before TO; role GRANT has TO without a prior ON.
    let roles_start = i;
    let mut saw_on = false;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            _ if depth == 0 && keyword_at(bytes, i, "ON") => {
                saw_on = true;
                break;
            }
            _ if depth == 0 && keyword_at(bytes, i, "TO") => break,
            _ => {}
        }
        i += 1;
    }
    if saw_on || !keyword_at(bytes, i, "TO") {
        return Ok(None);
    }
    let roles = parse_role_list(&bytes[roles_start..i])?;
    i = skip_ws(bytes, i + "TO".len());
    let (users, end) = parse_user_list(bytes, i)?;
    if roles.is_empty() || users.is_empty() {
        return Err(Error::unsupported("GRANT ROLE without roles/users"));
    }
    Ok(Some((
        Statement::GrantRole(GrantRoleStmt { roles, users }),
        end,
    )))
}

fn try_parse_revoke_role(bytes: &[u8], start: usize) -> Result<Option<(Statement, usize)>> {
    let mut i = skip_ws(bytes, start + "REVOKE".len());
    let roles_start = i;
    let mut saw_on = false;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            _ if depth == 0 && keyword_at(bytes, i, "ON") => {
                saw_on = true;
                break;
            }
            _ if depth == 0 && keyword_at(bytes, i, "FROM") => break,
            _ => {}
        }
        i += 1;
    }
    if saw_on || !keyword_at(bytes, i, "FROM") {
        return Ok(None);
    }
    let roles = parse_role_list(&bytes[roles_start..i])?;
    i = skip_ws(bytes, i + "FROM".len());
    let (users, end) = parse_user_list(bytes, i)?;
    if roles.is_empty() || users.is_empty() {
        return Err(Error::unsupported("REVOKE ROLE without roles/users"));
    }
    Ok(Some((
        Statement::RevokeRole(RevokeRoleStmt { roles, users }),
        end,
    )))
}

fn parse_role_list(raw: &[u8]) -> Result<Vec<UserIdentity>> {
    let text = std::str::from_utf8(raw).map_err(|_| Error::new(ERR_PARSE_ERROR, "role list"))?;
    let mut roles = Vec::new();
    for part in text.split(',') {
        let name = part.trim().trim_matches('`').trim_matches('\'').trim();
        if name.is_empty() {
            continue;
        }
        if name.contains('@') {
            let (u, h) = name.split_once('@').unwrap();
            roles.push(UserIdentity::new(
                u.trim().trim_matches('\'').trim_matches('`'),
                h.trim().trim_matches('\'').trim_matches('`'),
            ));
        } else {
            roles.push(UserIdentity::new(name, "%"));
        }
    }
    Ok(roles)
}

fn parse_user_list(bytes: &[u8], mut i: usize) -> Result<(Vec<UserIdentity>, usize)> {
    let mut users = Vec::new();
    loop {
        i = skip_ws(bytes, i);
        if i >= bytes.len() || bytes[i] == b';' {
            break;
        }
        let (user, next) = read_user_identity(bytes, i)?;
        users.push(user);
        i = skip_ws(bytes, next);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    Ok((users, i))
}

fn read_user_identity(bytes: &[u8], mut i: usize) -> Result<(UserIdentity, usize)> {
    i = skip_ws(bytes, i);
    let (user, after_user) = read_quoted_or_ident(bytes, i);
    if user.is_empty() {
        return Err(Error::new(ERR_PARSE_ERROR, "missing user"));
    }
    let mut j = skip_ws(bytes, after_user);
    if j < bytes.len() && bytes[j] == b'@' {
        j = skip_ws(bytes, j + 1);
        let (host, after_host) = read_quoted_or_ident(bytes, j);
        return Ok((UserIdentity::new(user, host), after_host));
    }
    Ok((UserIdentity::new(user, "%"), after_user))
}

fn read_quoted_or_ident(bytes: &[u8], i: usize) -> (String, usize) {
    if i >= bytes.len() {
        return (String::new(), i);
    }
    match bytes[i] {
        b'\'' | b'"' | b'`' => {
            let q = bytes[i];
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != q {
                j += 1;
            }
            let s = String::from_utf8_lossy(&bytes[i + 1..j]).into_owned();
            let end = if j < bytes.len() { j + 1 } else { j };
            (s, end)
        }
        _ => {
            let mut j = i;
            while j < bytes.len() && is_ident_byte(bytes[j]) {
                j += 1;
            }
            (String::from_utf8_lossy(&bytes[i..j]).into_owned(), j)
        }
    }
}

fn role_from_object_name(name: &ObjectName) -> Result<UserIdentity> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [user] => Ok(UserIdentity::new(user.clone(), "%")),
        [user, host] => Ok(UserIdentity::new(user.clone(), host.clone())),
        _ => Err(Error::unsupported("role name")),
    }
}

fn extract_prepare_from(sql: &str) -> (String, Vec<Option<String>>) {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut sqls = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "PREPARE") {
            let after = skip_ws(bytes, i + "PREPARE".len());
            let (name, name_end) = take_ident(bytes, after);
            let after_name = skip_ws(bytes, name_end);
            if !name.is_empty() && keyword_at(bytes, after_name, "FROM") {
                let value_at = skip_ws(bytes, after_name + "FROM".len());
                let (value, end) = take_explain_format(bytes, value_at);
                out.push_str("PREPARE ");
                out.push_str(&name);
                out.push_str(" AS SELECT 1");
                sqls.push(Some(value));
                i = end;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, sqls)
}

fn take_ident(bytes: &[u8], i: usize) -> (String, usize) {
    if i >= bytes.len() {
        return (String::new(), i);
    }
    if bytes[i] == b'`' || bytes[i] == b'\'' || bytes[i] == b'"' {
        let (value, end) = take_explain_format(bytes, i);
        return (value, end);
    }
    let mut j = i;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    (String::from_utf8_lossy(&bytes[i..j]).into_owned(), j)
}

fn lower_set(set: &Set) -> Result<Statement> {
    match set {
        Set::SetTransaction {
            modes, snapshot, ..
        } => {
            if snapshot.is_some() {
                return Err(Error::unsupported("SET TRANSACTION SNAPSHOT"));
            }
            let chars = txn_chars(modes)?;
            Ok(Statement::SetTxn(SetTxnStmt {
                isolation: chars.isolation,
                read_only: chars.read_only,
            }))
        }
        Set::SetNames {
            charset_name,
            collation_name,
        } => {
            let mut assignments = vec![SetAssignment {
                name: "names".to_string(),
                value: charset_name.value.clone(),
                scope: SetVarScope::Session,
            }];
            if let Some(collation) = collation_name {
                assignments.push(SetAssignment {
                    name: "collation_connection".to_string(),
                    value: collation.clone(),
                    scope: SetVarScope::Session,
                });
            }
            Ok(Statement::Set(SetStmt { assignments }))
        }
        Set::SetNamesDefault {} => Ok(Statement::Set(SetStmt {
            assignments: vec![SetAssignment {
                name: "names".to_string(),
                value: "utf8mb4".to_string(),
                scope: SetVarScope::Session,
            }],
        })),
        Set::SingleAssignment {
            scope,
            variable,
            values,
            ..
        } => {
            let (name, var_scope) = set_var_name_and_scope(variable, *scope)?;
            Ok(Statement::Set(SetStmt {
                assignments: vec![SetAssignment {
                    name,
                    value: values.first().map(|v| v.to_string()).unwrap_or_default(),
                    scope: var_scope,
                }],
            }))
        }
        Set::MultipleAssignments { assignments } => Ok(Statement::Set(SetStmt {
            assignments: assignments
                .iter()
                .map(|a| {
                    let (name, scope) = set_var_name_and_scope(&a.name, a.scope)?;
                    Ok(SetAssignment {
                        name,
                        value: a.value.to_string(),
                        scope,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })),
        Set::SetRole { role_name, .. } => {
            let kind = match role_name {
                None => SetRoleKind::None,
                Some(ident) if ident.value.eq_ignore_ascii_case("NONE") => SetRoleKind::None,
                Some(ident) if ident.value.eq_ignore_ascii_case("ALL") => SetRoleKind::All,
                Some(ident) => {
                    SetRoleKind::Roles(vec![UserIdentity::new(ident.value.clone(), "%")])
                }
            };
            Ok(Statement::SetRole(SetRoleStmt { kind }))
        }
        other => Err(Error::unsupported(format!("SET {other}"))),
    }
}

/// Isolation and access mode parsed from `START` / `SET TRANSACTION`.
struct TxnChars {
    isolation: Option<IsolationLevel>,
    read_only: Option<bool>,
}

fn txn_chars(modes: &[TransactionMode]) -> Result<TxnChars> {
    let mut isolation = None;
    let mut read_only = None;
    for mode in modes {
        match mode {
            TransactionMode::IsolationLevel(TransactionIsolationLevel::ReadCommitted) => {
                isolation = Some(IsolationLevel::ReadCommitted);
            }
            TransactionMode::IsolationLevel(TransactionIsolationLevel::RepeatableRead) => {
                isolation = Some(IsolationLevel::RepeatableRead);
            }
            TransactionMode::IsolationLevel(TransactionIsolationLevel::Serializable) => {
                // MVB: accept SERIALIZABLE; session maps to RR semantics.
                isolation = Some(IsolationLevel::Serializable);
            }
            TransactionMode::IsolationLevel(other) => {
                return Err(Error::unsupported(format!("ISOLATION LEVEL {other}")));
            }
            TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => {
                read_only = Some(true);
            }
            TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => {
                read_only = Some(false);
            }
        }
    }
    Ok(TxnChars {
        isolation,
        read_only,
    })
}

/// sqlparser-rs accepts `FORMAT JSON`, not TiDB `FORMAT = 'dot'`.
fn rewrite_explain_format(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "EXPLAIN") {
            out.push_str(&sql[i..i + "EXPLAIN".len()]);
            i += "EXPLAIN".len();
            let after = skip_ws(bytes, i);
            if keyword_at(bytes, after, "ANALYZE") {
                out.push_str(&sql[i..after + "ANALYZE".len()]);
                i = after + "ANALYZE".len();
            }
            let fmt = skip_ws(bytes, i);
            if keyword_at(bytes, fmt, "FORMAT") {
                let after_fmt = skip_ws(bytes, fmt + "FORMAT".len());
                let value_at = if after_fmt < bytes.len() && bytes[after_fmt] == b'=' {
                    skip_ws(bytes, after_fmt + 1)
                } else {
                    after_fmt
                };
                let (value, end) = take_explain_format(bytes, value_at);
                out.push_str(&sql[i..fmt]);
                match map_explain_format_kw(&value) {
                    ExplainFormatRewrite::Strip => {}
                    ExplainFormatRewrite::Keyword(kw) => {
                        out.push_str("FORMAT ");
                        out.push_str(kw);
                        out.push(' ');
                    }
                    ExplainFormatRewrite::Keep => {
                        out.push_str(&sql[fmt..end]);
                    }
                }
                i = end;
                continue;
            }
            continue;
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn take_explain_format(bytes: &[u8], i: usize) -> (String, usize) {
    if i >= bytes.len() {
        return (String::new(), i);
    }
    let quote = bytes[i];
    if quote == b'\'' || quote == b'"' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != quote {
            j += 1;
        }
        let value = String::from_utf8_lossy(&bytes[i + 1..j]).into_owned();
        let end = if j < bytes.len() { j + 1 } else { j };
        return (value, end);
    }
    let mut j = i;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    (String::from_utf8_lossy(&bytes[i..j]).into_owned(), j)
}

enum ExplainFormatRewrite {
    Strip,
    Keyword(&'static str),
    Keep,
}

fn map_explain_format_kw(value: &str) -> ExplainFormatRewrite {
    match value.to_ascii_lowercase().as_str() {
        "row" | "traditional" | "brief" | "text" | "tree" | "verbose" => {
            ExplainFormatRewrite::Strip
        }
        "json" => ExplainFormatRewrite::Keyword("JSON"),
        "dot" | "graphviz" => ExplainFormatRewrite::Keyword("GRAPHVIZ"),
        _ => ExplainFormatRewrite::Keep,
    }
}

fn lower_explain_format(
    verbose: bool,
    format: Option<&AnalyzeFormatKind>,
) -> Result<ExplainFormat> {
    let _ = verbose;
    match format {
        None => Ok(ExplainFormat::Row),
        Some(AnalyzeFormatKind::Keyword(fmt) | AnalyzeFormatKind::Assignment(fmt)) => match fmt {
            AnalyzeFormat::JSON => Ok(ExplainFormat::Json),
            AnalyzeFormat::GRAPHVIZ => Ok(ExplainFormat::Dot),
            AnalyzeFormat::TEXT | AnalyzeFormat::TRADITIONAL | AnalyzeFormat::TREE => {
                Ok(ExplainFormat::Row)
            }
        },
    }
}

#[derive(Clone, Debug)]
enum AlterPartOp {
    Add(Vec<PartitionDef>),
    Drop { if_exists: bool, names: Vec<String> },
}

/// sqlparser MySQL dialect rejects `ADD/DROP PARTITION`; strip and re-attach.
fn extract_alter_partitions(sql: &str) -> Result<(String, Vec<Option<AlterPartOp>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "ALTER") {
            let after_alter = skip_ws(bytes, i + "ALTER".len());
            if keyword_at(bytes, after_alter, "TABLE") {
                let after_table = skip_ws(bytes, after_alter + "TABLE".len());
                let after_name = skip_ident_or_quoted(bytes, after_table);
                let after_name = if after_name < bytes.len() && bytes[after_name] == b'.' {
                    skip_ident_or_quoted(bytes, skip_ws(bytes, after_name + 1))
                } else {
                    after_name
                };
                let after_name = skip_ws(bytes, after_name);
                if keyword_at(bytes, after_name, "ADD") {
                    let after_add = skip_ws(bytes, after_name + "ADD".len());
                    if keyword_at(bytes, after_add, "PARTITION") {
                        let after_part = skip_ws(bytes, after_add + "PARTITION".len());
                        if after_part < bytes.len() && bytes[after_part] == b'(' {
                            let close = matching_paren(bytes, after_part)
                                .ok_or_else(|| Error::unsupported("ADD PARTITION list"))?;
                            let body = std::str::from_utf8(&bytes[after_part + 1..close]).map_err(
                                |_| Error::new(ERR_PARSE_ERROR, "invalid ADD PARTITION"),
                            )?;
                            let defs = parse_add_partition_defs(body)?;
                            out.push_str(&sql[i..after_name]);
                            out.push_str("ALGORITHM=INPLACE");
                            ops.push(Some(AlterPartOp::Add(defs)));
                            i = skip_ws(bytes, close + 1);
                            continue;
                        }
                    }
                }
                if keyword_at(bytes, after_name, "DROP") {
                    let after_drop = skip_ws(bytes, after_name + "DROP".len());
                    if keyword_at(bytes, after_drop, "PARTITION") {
                        let mut j = skip_ws(bytes, after_drop + "PARTITION".len());
                        let if_exists = if keyword_at(bytes, j, "IF") {
                            let after_if = skip_ws(bytes, j + "IF".len());
                            if keyword_at(bytes, after_if, "EXISTS") {
                                j = skip_ws(bytes, after_if + "EXISTS".len());
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        let mut names = Vec::new();
                        loop {
                            let (name, next) = read_ident(bytes, j);
                            if name.is_empty() {
                                break;
                            }
                            names.push(name);
                            j = skip_ws(bytes, next);
                            if j < bytes.len() && bytes[j] == b',' {
                                j = skip_ws(bytes, j + 1);
                                continue;
                            }
                            break;
                        }
                        if names.is_empty() {
                            return Err(Error::unsupported("DROP PARTITION without names"));
                        }
                        out.push_str(&sql[i..after_name]);
                        out.push_str("ALGORITHM=INPLACE");
                        ops.push(Some(AlterPartOp::Drop { if_exists, names }));
                        i = j;
                        continue;
                    }
                }
                // Non-partition ALTER TABLE — still consume one alter slot as None.
                ops.push(None);
            }
        }
        i = push_sql_char(&mut out, sql, i);
    }
    Ok((out, ops))
}

fn parse_add_partition_defs(body: &str) -> Result<Vec<PartitionDef>> {
    let mut defs = Vec::new();
    for raw in split_defs(body) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let bytes = item.as_bytes();
        let mut i = skip_ws(bytes, 0);
        if !keyword_at(bytes, i, "PARTITION") {
            return Err(Error::unsupported("ADD PARTITION item"));
        }
        i = skip_ws(bytes, i + "PARTITION".len());
        let name_end = skip_ident_or_quoted(bytes, i);
        let name = item[i..name_end].trim().trim_matches('`').to_string();
        i = skip_ws(bytes, name_end);
        if !keyword_at(bytes, i, "VALUES") {
            return Err(Error::unsupported("ADD PARTITION VALUES"));
        }
        i = skip_ws(bytes, i + "VALUES".len());
        if keyword_at(bytes, i, "IN") {
            i = skip_ws(bytes, i + "IN".len());
            if i >= bytes.len() || bytes[i] != b'(' {
                return Err(Error::unsupported("VALUES IN"));
            }
            let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("VALUES IN"))?;
            let mut in_values = Vec::new();
            for raw_v in item[i + 1..close].split(',') {
                let v = raw_v.trim();
                if v.is_empty() {
                    continue;
                }
                in_values.push(
                    v.parse::<i64>()
                        .map_err(|_| Error::unsupported("non-integer LIST value"))?,
                );
            }
            defs.push(PartitionDef {
                name,
                less_than: None,
                extra_bounds: Vec::new(),
                in_values,
            });
        } else if keyword_at(bytes, i, "LESS") {
            i = skip_ws(bytes, i + "LESS".len());
            if !keyword_at(bytes, i, "THAN") {
                return Err(Error::unsupported("VALUES LESS THAN"));
            }
            i = skip_ws(bytes, i + "THAN".len());
            let bounds = parse_less_than_bounds(bytes, i, item)?;
            let less_than = bounds.first().copied().flatten();
            let extra_bounds = if bounds.len() > 1 {
                bounds[1..].to_vec()
            } else {
                Vec::new()
            };
            defs.push(PartitionDef {
                name,
                less_than,
                extra_bounds,
                in_values: Vec::new(),
            });
        } else {
            return Err(Error::unsupported("ADD PARTITION VALUES"));
        }
    }
    if defs.is_empty() {
        return Err(Error::unsupported("ADD PARTITION without definitions"));
    }
    Ok(defs)
}

/// sqlparser-rs MySQL dialect does not accept TiDB/MySQL `PARTITION BY`.
/// Strip the clause and re-attach it after lowering, same pattern as
/// `BEGIN PESSIMISTIC`.
fn extract_create_table_partitions(sql: &str) -> Result<(String, Vec<Option<PartitionOption>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut opts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "CREATE") {
            let after_create = skip_ws(bytes, i + "CREATE".len());
            if keyword_at(bytes, after_create, "TABLE") {
                let after_table = skip_ws(bytes, after_create + "TABLE".len());
                let after_name = skip_create_table_name(bytes, after_table);
                let after_name = skip_ws(bytes, after_name);
                if after_name < bytes.len()
                    && bytes[after_name] == b'('
                    && let Some(close) = matching_paren(bytes, after_name)
                {
                    let after_cols = skip_ws(bytes, close + 1);
                    if keyword_at(bytes, after_cols, "PARTITION") {
                        let after_part = skip_ws(bytes, after_cols + "PARTITION".len());
                        if keyword_at(bytes, after_part, "BY") {
                            let start = after_part + "BY".len();
                            let (opt, end) = parse_partition_by(bytes, skip_ws(bytes, start))?;
                            out.push_str(&sql[i..after_cols]);
                            opts.push(Some(opt));
                            i = end;
                            continue;
                        }
                    }
                    out.push_str(&sql[i..close + 1]);
                    opts.push(None);
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(sql[i..].chars().next().unwrap());
        i += sql[i..].chars().next().unwrap().len_utf8();
    }
    Ok((out, opts))
}

fn skip_create_table_name(bytes: &[u8], mut i: usize) -> usize {
    i = skip_ws(bytes, i);
    if keyword_at(bytes, i, "IF") {
        let after_if = skip_ws(bytes, i + 2);
        if keyword_at(bytes, after_if, "NOT") {
            let after_not = skip_ws(bytes, after_if + 3);
            if keyword_at(bytes, after_not, "EXISTS") {
                i = skip_ws(bytes, after_not + "EXISTS".len());
            }
        }
    }
    i = skip_ident_or_quoted(bytes, i);
    i = skip_ws(bytes, i);
    if i < bytes.len() && bytes[i] == b'.' {
        i = skip_ident_or_quoted(bytes, skip_ws(bytes, i + 1));
    }
    i
}

fn skip_ident_or_quoted(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    match bytes[i] {
        b'`' | b'"' | b'\'' => {
            let q = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            i
        }
        _ => {
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            i
        }
    }
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_partition_by(bytes: &[u8], mut i: usize) -> Result<(PartitionOption, usize)> {
    // SUBPARTITION BY is accepted under RANGE/LIST (TiDB strips it) and
    // rejected under HASH/KEY with ErrSubpartition.
    let tp = if keyword_at(bytes, i, "RANGE") {
        i = skip_ws(bytes, i + "RANGE".len());
        PartitionType::Range
    } else if keyword_at(bytes, i, "HASH") {
        i = skip_ws(bytes, i + "HASH".len());
        PartitionType::Hash
    } else if keyword_at(bytes, i, "LIST") {
        i = skip_ws(bytes, i + "LIST".len());
        PartitionType::List
    } else if keyword_at(bytes, i, "KEY") {
        i = skip_ws(bytes, i + "KEY".len());
        PartitionType::Key
    } else {
        return Err(Error::unsupported("partition type"));
    };
    let range_columns = tp == PartitionType::Range && keyword_at(bytes, i, "COLUMNS");
    if range_columns {
        i = skip_ws(bytes, i + "COLUMNS".len());
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return Err(Error::unsupported("PARTITION BY expression"));
    }
    let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("PARTITION BY"))?;
    let inner = std::str::from_utf8(&bytes[i + 1..close])
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid partition column"))?;
    let (column, columns, expr) = parse_partition_key(inner, range_columns)?;
    i = skip_ws(bytes, close + 1);
    match tp {
        PartitionType::Hash | PartitionType::Key => {
            if !keyword_at(bytes, i, "PARTITIONS") {
                return Err(Error::unsupported("HASH/KEY partition list"));
            }
            i = skip_ws(bytes, i + "PARTITIONS".len());
            let (num, end) = parse_u64_at(bytes, i)?;
            if num == 0 {
                return Err(Error::unsupported("HASH/KEY PARTITIONS 0"));
            }
            let (has_sub, end) = parse_optional_subpartition(bytes, skip_ws(bytes, end))?;
            if has_sub {
                return Err(subpartition_mix_error());
            }
            Ok((
                PartitionOption {
                    tp,
                    column,
                    columns,
                    expr,
                    defs: Vec::new(),
                    num,
                    subpartition_ignored: false,
                },
                end,
            ))
        }
        PartitionType::Range => {
            let (has_sub, next) = parse_optional_subpartition(bytes, i)?;
            i = next;
            if i >= bytes.len() || bytes[i] != b'(' {
                return Err(Error::unsupported("RANGE without partitions"));
            }
            let end = matching_paren(bytes, i)
                .ok_or_else(|| Error::unsupported("RANGE partition list"))?;
            let body = std::str::from_utf8(&bytes[i + 1..end])
                .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid RANGE partitions"))?;
            let defs = parse_range_defs(body)?;
            Ok((
                PartitionOption {
                    tp,
                    column,
                    columns,
                    expr,
                    num: defs.len() as u64,
                    defs,
                    subpartition_ignored: has_sub,
                },
                end + 1,
            ))
        }
        PartitionType::List => {
            let (has_sub, next) = parse_optional_subpartition(bytes, i)?;
            i = next;
            if i >= bytes.len() || bytes[i] != b'(' {
                return Err(Error::unsupported("LIST without partitions"));
            }
            let end = matching_paren(bytes, i)
                .ok_or_else(|| Error::unsupported("LIST partition list"))?;
            let body = std::str::from_utf8(&bytes[i + 1..end])
                .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid LIST partitions"))?;
            let defs = parse_list_defs(body)?;
            Ok((
                PartitionOption {
                    tp,
                    column,
                    columns,
                    expr,
                    num: defs.len() as u64,
                    defs,
                    subpartition_ignored: has_sub,
                },
                end + 1,
            ))
        }
    }
}

/// Consume `SUBPARTITION BY HASH|KEY (...) [SUBPARTITIONS n]`.
fn parse_optional_subpartition(bytes: &[u8], mut i: usize) -> Result<(bool, usize)> {
    i = skip_ws(bytes, i);
    if !keyword_at(bytes, i, "SUBPARTITION") {
        return Ok((false, i));
    }
    i = skip_ws(bytes, i + "SUBPARTITION".len());
    if !keyword_at(bytes, i, "BY") {
        return Err(Error::unsupported("SUBPARTITION without BY"));
    }
    i = skip_ws(bytes, i + "BY".len());
    if keyword_at(bytes, i, "HASH") {
        i = skip_ws(bytes, i + "HASH".len());
    } else if keyword_at(bytes, i, "KEY") {
        i = skip_ws(bytes, i + "KEY".len());
    } else {
        return Err(Error::unsupported("SUBPARTITION type"));
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return Err(Error::unsupported("SUBPARTITION BY expression"));
    }
    let close =
        matching_paren(bytes, i).ok_or_else(|| Error::unsupported("SUBPARTITION BY expression"))?;
    i = skip_ws(bytes, close + 1);
    if keyword_at(bytes, i, "SUBPARTITIONS") {
        i = skip_ws(bytes, i + "SUBPARTITIONS".len());
        let (n, end) = parse_u64_at(bytes, i)?;
        if n == 0 {
            return Err(Error::unsupported("SUBPARTITIONS 0"));
        }
        i = skip_ws(bytes, end);
    }
    Ok((true, i))
}

fn subpartition_mix_error() -> Error {
    // TiDB `ast.ErrSubpartition` / MySQL 1500.
    Error::new(
        1500,
        "It is only possible to mix RANGE/LIST partitioning with HASH/KEY partitioning for subpartitioning",
    )
}

/// Parse a column list or a scalar partition expression (`id % 4`, `ABS(id)`).
fn parse_partition_key(
    inner: &str,
    range_columns: bool,
) -> Result<(String, Vec<String>, Option<String>)> {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Err(Error::unsupported("PARTITION BY without columns"));
    }
    if range_columns {
        let columns = parse_partition_columns(trimmed, true)?;
        let column = columns[0].clone();
        return Ok((column, columns, None));
    }
    if is_partition_expr(trimmed) {
        // Validate the expression lowers; referenced columns are checked in DDL.
        let expr = parse_expr(trimmed)?;
        let mut cols = Vec::new();
        collect_expr_columns(&expr, &mut cols);
        return Ok((
            cols.first().cloned().unwrap_or_default(),
            cols,
            Some(trimmed.to_string()),
        ));
    }
    let columns = parse_partition_columns(trimmed, false)?;
    let column = columns[0].clone();
    Ok((column, columns, None))
}

fn is_partition_expr(inner: &str) -> bool {
    let bytes = inner.as_bytes();
    if bytes
        .iter()
        .any(|b| matches!(b, b'(' | b'+' | b'*' | b'/' | b'%'))
    {
        return true;
    }
    // `id - 1` / `id-1` but not a bare identifier.
    if let Some(pos) = bytes.iter().position(|b| *b == b'-') {
        let before = &bytes[..pos];
        let after = &bytes[pos + 1..];
        if before.iter().any(|b| is_ident_byte(*b)) && after.iter().any(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    false
}

fn parse_partition_columns(inner: &str, allow_list: bool) -> Result<Vec<String>> {
    if inner.trim().is_empty() {
        return Err(Error::unsupported("PARTITION BY without columns"));
    }
    if inner.contains('(') {
        return Err(Error::unsupported("partition expression"));
    }
    let names: Vec<String> = inner
        .split(',')
        .map(|p| p.trim().trim_matches('`').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if names.is_empty() {
        return Err(Error::unsupported("PARTITION BY without columns"));
    }
    if names.len() > 1 && !allow_list {
        return Err(Error::unsupported("partition expression"));
    }
    Ok(names)
}

fn collect_expr_columns(expr: &ExprNode, out: &mut Vec<String>) {
    match expr {
        ExprNode::Column(col) => {
            if !out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&col.name))
            {
                out.push(col.name.clone());
            }
        }
        ExprNode::Binary(bin) => {
            collect_expr_columns(&bin.left, out);
            collect_expr_columns(&bin.right, out);
        }
        ExprNode::Unary(unary) => collect_expr_columns(&unary.expr, out),
        ExprNode::Func(func) => {
            for arg in &func.args {
                collect_expr_columns(arg, out);
            }
        }
        ExprNode::Cast(cast) => collect_expr_columns(&cast.expr, out),
        ExprNode::Between(between) => {
            collect_expr_columns(&between.expr, out);
            collect_expr_columns(&between.low, out);
            collect_expr_columns(&between.high, out);
        }
        ExprNode::InList(in_list) => {
            collect_expr_columns(&in_list.expr, out);
            for item in &in_list.list {
                collect_expr_columns(item, out);
            }
        }
        ExprNode::IsNull(is_null) => collect_expr_columns(&is_null.expr, out),
        ExprNode::Like(like) => {
            collect_expr_columns(&like.expr, out);
            collect_expr_columns(&like.pattern, out);
        }
        _ => {}
    }
}

/// Lower a scalar SQL expression used as a PARTITION BY key.
pub fn parse_expr(sql: &str) -> Result<ExprNode> {
    let wrapped = format!("SELECT ({sql})");
    let stmts = Parser::parse_sql(&dialect(), &wrapped).map_err(|e| {
        Error::new(
            ERR_PARSE_ERROR,
            format!("You have an error in your SQL syntax: {e}"),
        )
    })?;
    let stmt = stmts
        .first()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "empty partition expression"))?;
    let lowered = lower_statement(stmt)?;
    let Statement::Select(select) = lowered else {
        return Err(Error::unsupported("partition expression"));
    };
    select
        .fields
        .first()
        .map(|f| f.expr.clone())
        .ok_or_else(|| Error::unsupported("partition expression"))
}

fn parse_range_defs(body: &str) -> Result<Vec<PartitionDef>> {
    let mut defs = Vec::new();
    for raw in split_defs(body) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let bytes = item.as_bytes();
        let mut i = skip_ws(bytes, 0);
        if !keyword_at(bytes, i, "PARTITION") {
            return Err(Error::unsupported("RANGE partition item"));
        }
        i = skip_ws(bytes, i + "PARTITION".len());
        let name_end = skip_ident_or_quoted(bytes, i);
        let name = item[i..name_end].trim().trim_matches('`').to_string();
        i = skip_ws(bytes, name_end);
        if !keyword_at(bytes, i, "VALUES") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "VALUES".len());
        if !keyword_at(bytes, i, "LESS") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "LESS".len());
        if !keyword_at(bytes, i, "THAN") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "THAN".len());
        let bounds = parse_less_than_bounds(bytes, i, item)?;
        let less_than = bounds.first().copied().flatten();
        let extra_bounds = if bounds.len() > 1 {
            bounds[1..].to_vec()
        } else {
            Vec::new()
        };
        // Named SUBPARTITION lists under a PARTITION are stripped with the clause.
        let _ = skip_named_subpartitions(bytes, after_less_than(bytes, i)?);
        defs.push(PartitionDef {
            name,
            less_than,
            extra_bounds,
            in_values: Vec::new(),
        });
    }
    if defs.is_empty() {
        return Err(Error::unsupported("RANGE without partitions"));
    }
    Ok(defs)
}

fn after_less_than(bytes: &[u8], i: usize) -> Result<usize> {
    if keyword_at(bytes, i, "MAXVALUE") {
        return Ok(skip_ws(bytes, i + "MAXVALUE".len()));
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return Err(Error::unsupported("VALUES LESS THAN"));
    }
    let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("LESS THAN"))?;
    Ok(skip_ws(bytes, close + 1))
}

/// Skip optional `(SUBPARTITION name, ...)` trailing a PARTITION definition.
fn skip_named_subpartitions(bytes: &[u8], i: usize) -> usize {
    let i = skip_ws(bytes, i);
    if i >= bytes.len() || bytes[i] != b'(' {
        return i;
    }
    matching_paren(bytes, i).map(|end| end + 1).unwrap_or(i)
}

fn parse_less_than_bounds(bytes: &[u8], i: usize, item: &str) -> Result<Vec<Option<i64>>> {
    if keyword_at(bytes, i, "MAXVALUE") {
        return Ok(vec![None]);
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return Err(Error::unsupported("VALUES LESS THAN"));
    }
    let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("LESS THAN"))?;
    let mut bounds = Vec::new();
    for raw in item[i + 1..close].split(',') {
        let bound = raw.trim();
        if bound.is_empty() {
            continue;
        }
        if bound.eq_ignore_ascii_case("MAXVALUE") {
            bounds.push(None);
        } else {
            bounds.push(Some(
                bound
                    .parse::<i64>()
                    .map_err(|_| Error::unsupported("non-integer LESS THAN"))?,
            ));
        }
    }
    if bounds.is_empty() {
        return Err(Error::unsupported("VALUES LESS THAN"));
    }
    Ok(bounds)
}

fn parse_list_defs(body: &str) -> Result<Vec<PartitionDef>> {
    let mut defs = Vec::new();
    for raw in split_defs(body) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let bytes = item.as_bytes();
        let mut i = skip_ws(bytes, 0);
        if !keyword_at(bytes, i, "PARTITION") {
            return Err(Error::unsupported("LIST partition item"));
        }
        i = skip_ws(bytes, i + "PARTITION".len());
        let name_end = skip_ident_or_quoted(bytes, i);
        let name = item[i..name_end].trim().trim_matches('`').to_string();
        i = skip_ws(bytes, name_end);
        if !keyword_at(bytes, i, "VALUES") {
            return Err(Error::unsupported("VALUES IN"));
        }
        i = skip_ws(bytes, i + "VALUES".len());
        if !keyword_at(bytes, i, "IN") {
            return Err(Error::unsupported("VALUES IN"));
        }
        i = skip_ws(bytes, i + "IN".len());
        if i >= bytes.len() || bytes[i] != b'(' {
            return Err(Error::unsupported("VALUES IN"));
        }
        let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("VALUES IN"))?;
        let mut in_values = Vec::new();
        for raw_v in item[i + 1..close].split(',') {
            let v = raw_v.trim();
            if v.is_empty() {
                continue;
            }
            in_values.push(
                v.parse::<i64>()
                    .map_err(|_| Error::unsupported("non-integer LIST value"))?,
            );
        }
        let _ = skip_named_subpartitions(bytes, close + 1);
        if in_values.is_empty() {
            return Err(Error::unsupported("LIST VALUES IN empty"));
        }
        defs.push(PartitionDef {
            name,
            less_than: None,
            extra_bounds: Vec::new(),
            in_values,
        });
    }
    if defs.is_empty() {
        return Err(Error::unsupported("LIST without partitions"));
    }
    Ok(defs)
}

fn split_defs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn parse_u64_at(bytes: &[u8], i: usize) -> Result<(u64, usize)> {
    let mut end = i;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == i {
        return Err(Error::new(ERR_PARSE_ERROR, "missing PARTITIONS count"));
    }
    let n = std::str::from_utf8(&bytes[i..end])
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid PARTITIONS count"))?
        .parse::<u64>()
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid PARTITIONS count"))?;
    Ok((n, end))
}

fn lower_select_lock(locks: &[LockClause]) -> Result<(SelectLock, SelectLockWait)> {
    if locks.is_empty() {
        return Ok((SelectLock::None, SelectLockWait::Block));
    }
    if locks.len() != 1 {
        return Err(Error::unsupported("multiple SELECT lock clauses"));
    }
    let lock = &locks[0];
    if lock.of.is_some() {
        return Err(Error::unsupported("FOR UPDATE OF"));
    }
    let wait = match lock.nonblock {
        Some(NonBlock::Nowait) => SelectLockWait::Nowait,
        Some(NonBlock::SkipLocked) => SelectLockWait::SkipLocked,
        None => SelectLockWait::Block,
    };
    let kind = match lock.lock_type {
        LockType::Update => SelectLock::ForUpdate,
        LockType::Share => SelectLock::ForShare,
    };
    Ok((kind, wait))
}

fn lower_query_stmt(query: &Query) -> Result<Statement> {
    let with = lower_with(query.with.as_ref())?;
    match query.body.as_ref() {
        SetExpr::Select(_) => Ok(Statement::Select(lower_query(query)?)),
        SetExpr::SetOperation { .. } => {
            let mut set = lower_set_opr(query)?;
            set.with = with;
            Ok(Statement::SetOpr(set))
        }
        SetExpr::Query(inner) => {
            if with.is_some() && inner.with.is_some() {
                return Err(Error::unsupported("nested WITH"));
            }
            let mut stmt = lower_query_stmt(inner)?;
            attach_dml_with(&mut stmt, with)?;
            Ok(stmt)
        }
        SetExpr::Insert(stmt) | SetExpr::Update(stmt) | SetExpr::Delete(stmt) => {
            let mut lowered = lower_statement(stmt)?;
            attach_dml_with(&mut lowered, with)?;
            Ok(lowered)
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("query body {:?} is not available in this phase", other),
        )),
    }
}

/// Attach a statement-level `WITH` to INSERT/UPDATE/DELETE (and SELECT).
fn attach_dml_with(stmt: &mut Statement, with: Option<WithClause>) -> Result<()> {
    let Some(with) = with else {
        return Ok(());
    };
    match stmt {
        Statement::Select(select) if select.with.is_none() => select.with = Some(with),
        Statement::SetOpr(set) if set.with.is_none() => set.with = Some(with),
        Statement::Insert(insert) if insert.with.is_none() => {
            if let Some(select) = insert.select.as_mut() {
                if select.with.is_none() {
                    select.with = Some(with.clone());
                } else {
                    return Err(Error::unsupported("nested WITH"));
                }
            }
            insert.with = Some(with);
        }
        Statement::Update(update) if update.with.is_none() => update.with = Some(with),
        Statement::Delete(delete) if delete.with.is_none() => delete.with = Some(with),
        Statement::Select(_)
        | Statement::SetOpr(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_) => return Err(Error::unsupported("nested WITH")),
        _ => return Err(Error::unsupported("WITH on non-SELECT")),
    }
    Ok(())
}

fn lower_set_opr(query: &Query) -> Result<SetOprStmt> {
    let (first, rest) = flatten_set_expr(query.body.as_ref())?;
    let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;
    Ok(SetOprStmt {
        first,
        rest,
        order_by: lower_order_by(query.order_by.as_ref())?,
        limit,
        offset,
        with: None,
    })
}

fn lower_with(with: Option<&With>) -> Result<Option<WithClause>> {
    let Some(with) = with else {
        return Ok(None);
    };
    // CTE FROM clause / nested WITH.
    let mut names = Vec::new();
    let mut ctes = Vec::with_capacity(with.cte_tables.len());
    for cte in &with.cte_tables {
        let lowered = lower_cte(cte, with.recursive)?;
        if names
            .iter()
            .any(|name: &String| name.eq_ignore_ascii_case(&lowered.name))
        {
            return Err(Error::unsupported("duplicate CTE name"));
        }
        names.push(lowered.name.clone());
        ctes.push(lowered);
    }
    Ok(Some(WithClause {
        is_recursive: with.recursive,
        ctes,
    }))
}

/// Lower a `WITH` item. Recursive CTEs must be `seed UNION [ALL] recursive_arm`.
fn lower_cte(cte: &Cte, recursive: bool) -> Result<CommonTableExpression> {
    if cte.from.is_some() {
        return Err(Error::unsupported("CTE FROM clause"));
    }
    if cte.query.with.is_some() {
        return Err(Error::unsupported("nested WITH"));
    }
    let (query, recursive_arm, set_rest) = match cte.query.body.as_ref() {
        SetExpr::Select(_) => (lower_query(&cte.query)?, None, Vec::new()),
        SetExpr::SetOperation { .. } if recursive => {
            let set = lower_set_opr(&cte.query)?;
            if set.rest.len() != 1
                || !matches!(set.rest[0].0, SetOprType::Union | SetOprType::UnionAll)
            {
                return Err(Error::unsupported(
                    "recursive CTE requires a single UNION / UNION ALL",
                ));
            }
            (set.first, Some(set.rest[0].clone()), Vec::new())
        }
        SetExpr::SetOperation { .. } => {
            let set = lower_set_opr(&cte.query)?;
            (set.first, None, set.rest)
        }
        other => return Err(Error::unsupported(format!("CTE body {other}"))),
    };
    Ok(CommonTableExpression {
        name: cte.alias.name.value.clone(),
        query: Box::new(query),
        col_name_list: cte
            .alias
            .columns
            .iter()
            .map(|col| col.name.value.clone())
            .collect(),
        recursive: recursive_arm,
        set_rest,
        materialize: match cte.materialized {
            Some(CteAsMaterialized::Materialized) => CteMaterialize::Materialized,
            Some(CteAsMaterialized::NotMaterialized) => CteMaterialize::NotMaterialized,
            None => CteMaterialize::Default,
        },
    })
}

/// Flatten a set-operation tree, including parenthesized operands, into a
/// left-deep `(first, rest)` list. Mixed-operator parentheses are evaluated
/// left-to-right (INTERSECT-tighter-than-UNION stays a later leftover).
fn flatten_set_expr(expr: &SetExpr) -> Result<(SelectStmt, Vec<(SetOprType, SelectStmt)>)> {
    let expr = unwrap_set_query(expr);
    match expr {
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            let tp = map_set_opr(*op, *set_quantifier)?;
            let (first, mut rest) = flatten_set_expr(left)?;
            let (right_first, right_rest) = flatten_set_expr(right)?;
            rest.push((tp, right_first));
            rest.extend(right_rest);
            Ok((first, rest))
        }
        _ => Ok((lower_set_operand(expr)?, Vec::new())),
    }
}

/// Peel parenthesized `Query` wrappers so `(SELECT …)` and
/// `(SELECT … UNION SELECT …)` flatten as ordinary set operands.
fn unwrap_set_query(expr: &SetExpr) -> &SetExpr {
    match expr {
        SetExpr::Query(query) => unwrap_set_query(query.body.as_ref()),
        other => other,
    }
}

fn lower_set_operand(expr: &SetExpr) -> Result<SelectStmt> {
    match unwrap_set_query(expr) {
        SetExpr::Select(select) => lower_select(
            select,
            None,
            None,
            None,
            SelectLock::None,
            SelectLockWait::Block,
            select.to_string(),
        ),
        SetExpr::Query(query) => lower_query(query),
        other => Err(Error::unsupported(format!("UNION operand {other}"))),
    }
}

fn map_set_opr(op: SetOperator, quantifier: SetQuantifier) -> Result<SetOprType> {
    match (op, quantifier) {
        (SetOperator::Union, SetQuantifier::All) => Ok(SetOprType::UnionAll),
        (SetOperator::Union, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(SetOprType::Union)
        }
        (SetOperator::Except, SetQuantifier::All) => Ok(SetOprType::ExceptAll),
        (SetOperator::Except, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(SetOprType::Except)
        }
        (SetOperator::Intersect, SetQuantifier::All) => Ok(SetOprType::IntersectAll),
        (SetOperator::Intersect, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(SetOprType::Intersect)
        }
        (SetOperator::Minus, SetQuantifier::All) => Ok(SetOprType::ExceptAll),
        (SetOperator::Minus, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(SetOprType::Except)
        }
        (_, other) => Err(Error::unsupported(format!("UNION {other}"))),
    }
}

fn lower_query(query: &Query) -> Result<SelectStmt> {
    match query.body.as_ref() {
        SetExpr::Select(select) => {
            let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;
            let (lock, lock_wait) = lower_select_lock(&query.locks)?;
            let mut stmt = lower_select(
                select,
                query.order_by.as_ref(),
                limit,
                offset,
                lock,
                lock_wait,
                query.to_string(),
            )?;
            let derived = stmt.with.take().map(|with| with.ctes).unwrap_or_default();
            stmt.with = merge_with_ctes(lower_with(query.with.as_ref())?, derived)?;
            Ok(stmt)
        }
        // UNION in FROM derived table still uses this path.
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("query body {:?} is not available in this phase", other),
        )),
    }
}

/// Lower a subquery body that may be either SELECT or a set operation.
fn lower_result_set(query: &Query) -> Result<ResultSetNode> {
    match query.body.as_ref() {
        SetExpr::Select(_) => Ok(ResultSetNode::Select(lower_query(query)?)),
        SetExpr::SetOperation { .. } => {
            let mut set = lower_set_opr(query)?;
            set.with = lower_with(query.with.as_ref())?;
            Ok(ResultSetNode::SetOpr(set))
        }
        SetExpr::Query(inner) => lower_result_set(inner),
        other => Err(Error::unsupported(format!("subquery body {other}"))),
    }
}

fn lower_select(
    select: &sqlparser::ast::Select,
    order_by: Option<&sqlparser::ast::OrderBy>,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: SelectLock,
    lock_wait: SelectLockWait,
    text: String,
) -> Result<SelectStmt> {
    let windows = lower_named_windows(&select.named_window)?;
    let mut fields = select
        .projection
        .iter()
        .map(lower_select_item)
        .collect::<Result<Vec<_>>>()?;
    for field in &mut fields {
        resolve_named_window(&mut field.expr, &windows)?;
    }
    let (from, joins, derived) = lower_from(&select.from)?;
    if !joins.is_empty() && !derived.is_empty() {
        return Err(Error::unsupported("subquery JOIN"));
    }
    let selection = select.selection.as_ref().map(lower_expr).transpose()?;
    let having = select.having.as_ref().map(lower_expr).transpose()?;
    let group_by = lower_group_by(&select.group_by)?;
    let order_by = lower_order_by(order_by)?;
    Ok(SelectStmt {
        fields,
        from,
        joins,
        selection,
        group_by,
        having,
        order_by,
        limit,
        offset,
        lock,
        lock_wait,
        with: with_from_derived(derived),
        text,
    })
}

/// Turn flattened FROM-subquery CTEs into a WITH clause (empty → None).
fn with_from_derived(ctes: Vec<CommonTableExpression>) -> Option<WithClause> {
    if ctes.is_empty() {
        None
    } else {
        Some(WithClause {
            is_recursive: false,
            ctes,
        })
    }
}

/// Append derived-table CTEs after an existing WITH list.
fn merge_with_ctes(
    with: Option<WithClause>,
    extra: Vec<CommonTableExpression>,
) -> Result<Option<WithClause>> {
    if extra.is_empty() {
        return Ok(with);
    }
    let mut with = with.unwrap_or(WithClause {
        is_recursive: false,
        ctes: Vec::new(),
    });
    for cte in extra {
        if with.find(&cte.name).is_some() {
            return Err(Error::unsupported("duplicate CTE name"));
        }
        with.ctes.push(cte);
    }
    Ok(Some(with))
}

/// Dummy `ON 1` for CROSS JOIN / comma join, which have no real join predicate.
fn true_expr() -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::I64(1),
    })
}

/// Lower FROM into the leftmost table plus a left-deep join chain.
/// Comma-separated table lists become CROSS JOIN steps.
/// Derived tables (`FROM (SELECT ...) alias`) flatten into CTEs.
fn lower_from(
    from: &[TableWithJoins],
) -> Result<(Option<TableName>, Vec<Join>, Vec<CommonTableExpression>)> {
    if from.is_empty() {
        return Ok((None, Vec::new(), Vec::new()));
    }
    let (first, mut joins, ctes) = lower_table_with_joins(&from[0])?;
    if from.len() > 1 && !ctes.is_empty() {
        return Err(Error::unsupported("subquery JOIN"));
    }
    let mut prev = joins
        .last()
        .map(|j| j.right.clone())
        .unwrap_or_else(|| first.clone());
    for extra in &from[1..] {
        let (right, extra_joins, extra_ctes) = lower_table_with_joins(extra)?;
        if !extra_ctes.is_empty() {
            return Err(Error::unsupported("subquery JOIN"));
        }
        joins.push(Join {
            left: prev,
            right: right.clone(),
            tp: JoinType::Cross,
            on: true_expr(),
        });
        joins.extend(extra_joins);
        prev = joins.last().map(|j| j.right.clone()).unwrap_or(right);
    }
    Ok((Some(first), joins, ctes))
}

/// Lower `t` or `t1 JOIN t2 ON ... JOIN t3 ON ...` into a left-deep `Join` list.
fn lower_table_with_joins(
    table: &TableWithJoins,
) -> Result<(TableName, Vec<Join>, Vec<CommonTableExpression>)> {
    let (left, ctes) = table_factor(&table.relation)?;
    if table.joins.is_empty() {
        return Ok((left, Vec::new(), ctes));
    }
    if !ctes.is_empty() {
        return Err(Error::unsupported("subquery JOIN"));
    }
    let mut out = Vec::new();
    let mut prev = left.clone();
    for join in &table.joins {
        let (right, right_ctes) = table_factor(&join.relation)?;
        if !right_ctes.is_empty() {
            return Err(Error::unsupported("subquery JOIN"));
        }
        let (tp, on) = match &join.join_operator {
            JoinOperator::Join(JoinConstraint::On(expr))
            | JoinOperator::Inner(JoinConstraint::On(expr)) => (JoinType::Inner, lower_expr(expr)?),
            JoinOperator::Left(JoinConstraint::On(expr))
            | JoinOperator::LeftOuter(JoinConstraint::On(expr)) => {
                (JoinType::LeftOuter, lower_expr(expr)?)
            }
            JoinOperator::Right(JoinConstraint::On(expr))
            | JoinOperator::RightOuter(JoinConstraint::On(expr)) => {
                (JoinType::RightOuter, lower_expr(expr)?)
            }
            JoinOperator::CrossJoin(JoinConstraint::None) => (JoinType::Cross, true_expr()),
            JoinOperator::CrossJoin(JoinConstraint::On(expr)) => {
                (JoinType::Inner, lower_expr(expr)?)
            }
            JoinOperator::Semi(JoinConstraint::On(expr))
            | JoinOperator::LeftSemi(JoinConstraint::On(expr)) => {
                (JoinType::Semi, lower_expr(expr)?)
            }
            JoinOperator::Anti(JoinConstraint::On(expr))
            | JoinOperator::LeftAnti(JoinConstraint::On(expr)) => {
                (JoinType::Anti, lower_expr(expr)?)
            }
            // Completeness wave: NATURAL / USING / right semi/anti / full outer.
            other => return Err(Error::unsupported(format!("JOIN {other:?}"))),
        };
        out.push(Join {
            left: prev,
            right: right.clone(),
            tp,
            on,
        });
        prev = right;
    }
    Ok((left, out, Vec::new()))
}

/// Two-table DML only: reject n-table JOIN chains on UPDATE / DELETE.
fn lower_dml_table(table: &TableWithJoins) -> Result<(TableName, Option<Join>)> {
    let (name, mut joins, ctes) = lower_table_with_joins(table)?;
    if !ctes.is_empty() {
        return Err(Error::unsupported("derived table in UPDATE/DELETE"));
    }
    match joins.len() {
        0 => Ok((name, None)),
        1 => {
            let join = joins.pop().expect("len == 1");
            if join.tp.is_semi_anti() {
                return Err(Error::unsupported("SEMI/ANTI JOIN in UPDATE/DELETE"));
            }
            Ok((name, Some(join)))
        }
        _ => Err(Error::unsupported("more than two tables in JOIN")),
    }
}

/// Resolve a FROM item. Derived tables flatten to synthetic CTEs named by alias.
fn table_factor(factor: &TableFactor) -> Result<(TableName, Vec<CommonTableExpression>)> {
    match factor {
        TableFactor::Table { name, .. } => Ok((object_table_name(name)?, Vec::new())),
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample,
        } => {
            if *lateral {
                return Err(Error::unsupported("LATERAL"));
            }
            if sample.is_some() {
                return Err(Error::unsupported("TABLESAMPLE"));
            }
            let alias = alias
                .as_ref()
                .ok_or_else(|| Error::unsupported("derived table without alias"))?;
            let mut query = lower_query(subquery)?;
            let mut ctes = query.with.take().map(|with| with.ctes).unwrap_or_default();
            let name = alias.name.value.clone();
            if ctes.iter().any(|cte| cte.name.eq_ignore_ascii_case(&name)) {
                return Err(Error::unsupported("duplicate CTE name"));
            }
            ctes.push(CommonTableExpression {
                name: name.clone(),
                query: Box::new(query),
                col_name_list: alias
                    .columns
                    .iter()
                    .map(|col| col.name.value.clone())
                    .collect(),
                recursive: None,
                set_rest: Vec::new(),
                materialize: CteMaterialize::Default,
            });
            Ok((TableName::new(None, name), ctes))
        }
        other => Err(Error::unsupported(format!("FROM {other}"))),
    }
}

fn lower_group_by(group_by: &GroupByExpr) -> Result<Vec<ExprNode>> {
    match group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            // completeness wave: ROLLUP / CUBE / GROUPING SETS.
            if !modifiers.is_empty() {
                return Err(Error::unsupported("GROUP BY WITH ROLLUP/CUBE"));
            }
            exprs.iter().map(lower_expr).collect()
        }
        GroupByExpr::All(_) => Err(Error::unsupported("GROUP BY ALL")),
    }
}

fn lower_order_by(order_by: Option<&sqlparser::ast::OrderBy>) -> Result<Vec<ByItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    match &order_by.kind {
        OrderByKind::Expressions(items) => items
            .iter()
            .map(|item| {
                Ok(ByItem {
                    expr: lower_expr(&item.expr)?,
                    desc: matches!(item.options.sort, Some(OrderBySort::Desc)),
                })
            })
            .collect(),
        other => Err(Error::unsupported(format!("ORDER BY {other:?}"))),
    }
}

fn lower_limit(limit: Option<&LimitClause>) -> Result<(Option<u64>, Option<u64>)> {
    match limit {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset { limit, offset, .. }) => {
            let limit = limit.as_ref().map(expr_as_u64).transpose()?;
            let offset = offset.as_ref().map(|o| expr_as_u64(&o.value)).transpose()?;
            Ok((limit, offset))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok((Some(expr_as_u64(limit)?), Some(expr_as_u64(offset)?)))
        }
    }
}

fn expr_as_u64(expr: &Expr) -> Result<u64> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => n
                .parse::<u64>()
                .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string())),
            other => Err(Error::unsupported(format!("limit literal {other}"))),
        },
        other => Err(Error::unsupported(format!("limit expression {other}"))),
    }
}

/// Lower UPDATE, including two-table JOIN. FROM / RETURNING are not in TiDB v8.5.8.
fn lower_update(update: &Update) -> Result<Statement> {
    // two-table UPDATE JOIN. TiDB v8.5.8 has no UPDATE FROM / RETURNING.
    if update.from.is_some() || update.returning.is_some() || update.output.is_some() {
        return Err(Error::unsupported("UPDATE FROM / RETURNING"));
    }
    let (table, join) = lower_dml_table(&update.table)?;
    let order_by = lower_order_by_exprs(&update.order_by)?;
    let limit = update.limit.as_ref().map(expr_as_u64).transpose()?;
    if join.is_some() && (!order_by.is_empty() || limit.is_some()) {
        return Err(Error::unsupported("ORDER BY / LIMIT on multi-table UPDATE"));
    }
    let list = update
        .assignments
        .iter()
        .map(|item| {
            let (table, column) = match &item.target {
                AssignmentTarget::ColumnName(name) => assignment_column(name)?,
                AssignmentTarget::Tuple(_) => {
                    return Err(Error::unsupported("tuple UPDATE assignment"));
                }
            };
            Ok(Assignment {
                table,
                column,
                expr: lower_expr(&item.value)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if list.is_empty() {
        return Err(Error::unsupported("UPDATE without SET"));
    }
    Ok(Statement::Update(UpdateStmt {
        table,
        join,
        list,
        selection: update.selection.as_ref().map(lower_expr).transpose()?,
        order_by,
        limit,
        with: None,
    }))
}

/// Lower DELETE, including `DELETE t1 FROM t1 JOIN t2` and `USING`. RETURNING stays rejected.
fn lower_delete(delete: &Delete) -> Result<Statement> {
    // two-table DELETE. TiDB v8.5.8 has no DELETE RETURNING.
    if delete.returning.is_some() || delete.output.is_some() {
        return Err(Error::unsupported("DELETE RETURNING"));
    }
    let from = match &delete.from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    };
    let ((table, join), tables) = if let Some(using) = &delete.using {
        if using.len() != 1 {
            return Err(Error::unsupported("comma joins"));
        }
        let rel = lower_dml_table(&using[0])?;
        let tables = if !delete.tables.is_empty() {
            delete
                .tables
                .iter()
                .map(object_table_name)
                .collect::<Result<Vec<_>>>()?
        } else {
            from.iter()
                .map(|twj| {
                    if !twj.joins.is_empty() {
                        return Err(Error::unsupported("JOIN in DELETE FROM list"));
                    }
                    let (name, ctes) = table_factor(&twj.relation)?;
                    if !ctes.is_empty() {
                        return Err(Error::unsupported("derived table in UPDATE/DELETE"));
                    }
                    Ok(name)
                })
                .collect::<Result<Vec<_>>>()?
        };
        (rel, tables)
    } else {
        if from.len() != 1 {
            return Err(Error::unsupported("multi-table DELETE"));
        }
        let rel = lower_dml_table(&from[0])?;
        let tables = delete
            .tables
            .iter()
            .map(object_table_name)
            .collect::<Result<Vec<_>>>()?;
        (rel, tables)
    };
    let order_by = lower_order_by_exprs(&delete.order_by)?;
    let limit = delete.limit.as_ref().map(expr_as_u64).transpose()?;
    if join.is_some() && (!order_by.is_empty() || limit.is_some()) {
        return Err(Error::unsupported("ORDER BY / LIMIT on multi-table DELETE"));
    }
    if join.is_some() && tables.is_empty() {
        return Err(Error::unsupported("multi-table DELETE without table list"));
    }
    Ok(Statement::Delete(DeleteStmt {
        table,
        tables,
        join,
        selection: delete.selection.as_ref().map(lower_expr).transpose()?,
        order_by,
        limit,
        with: None,
    }))
}

/// Split `col` / `t.col` / `db.t.col` into `(table, column)` for UPDATE SET.
fn assignment_column(name: &ObjectName) -> Result<(Option<String>, String)> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [column] => Ok((None, column.clone())),
        [table, column] => Ok((Some(table.clone()), column.clone())),
        [_, table, column] => Ok((Some(table.clone()), column.clone())),
        _ => Err(Error::unsupported(format!("UPDATE column {name}"))),
    }
}

fn lower_truncate(truncate: &sqlparser::ast::Truncate) -> Result<Statement> {
    let target = truncate
        .table_names
        .first()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing table name"))?;
    if truncate.table_names.len() != 1 {
        return Err(Error::unsupported("TRUNCATE multiple tables"));
    }
    if truncate.partitions.is_some() {
        return Err(Error::unsupported("TRUNCATE PARTITION"));
    }
    Ok(Statement::TruncateTable(TruncateTableStmt {
        table: object_table_name(&target.name)?,
    }))
}

/// Extract MySQL `LOAD DATA [LOCAL] INFILE ... INTO TABLE`.
fn extract_load_data(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "LOAD") {
            let after_load = skip_ws(bytes, i + "LOAD".len());
            if keyword_at(bytes, after_load, "DATA") {
                let mut j = skip_ws(bytes, after_load + "DATA".len());
                let local = if keyword_at(bytes, j, "LOCAL") {
                    j = skip_ws(bytes, j + "LOCAL".len());
                    true
                } else {
                    false
                };
                if !keyword_at(bytes, j, "INFILE") {
                    out.push(bytes[i] as char);
                    i += 1;
                    continue;
                }
                j = skip_ws(bytes, j + "INFILE".len());
                let (path, after_path) = read_sql_string(bytes, j)?;
                j = skip_ws(bytes, after_path);
                if keyword_at(bytes, j, "REPLACE") || keyword_at(bytes, j, "IGNORE") {
                    // MVB: accept and ignore REPLACE/IGNORE keyword before INTO.
                    let kw = if keyword_at(bytes, j, "REPLACE") {
                        "REPLACE".len()
                    } else {
                        "IGNORE".len()
                    };
                    j = skip_ws(bytes, j + kw);
                }
                if !keyword_at(bytes, j, "INTO") {
                    return Err(Error::new(ERR_PARSE_ERROR, "LOAD DATA missing INTO TABLE"));
                }
                j = skip_ws(bytes, j + "INTO".len());
                if !keyword_at(bytes, j, "TABLE") {
                    return Err(Error::new(ERR_PARSE_ERROR, "LOAD DATA missing INTO TABLE"));
                }
                j = skip_ws(bytes, j + "TABLE".len());
                let (schema, name, after_name) = read_qualified_name(bytes, j)?;
                j = skip_ws(bytes, after_name);
                let mut fields_terminated_by = "\t".to_string();
                let mut lines_terminated_by = "\n".to_string();
                let mut ignore_lines = 0u64;
                let mut columns = Vec::new();
                loop {
                    j = skip_ws(bytes, j);
                    if j >= bytes.len() || bytes[j] == b';' {
                        break;
                    }
                    if keyword_at(bytes, j, "FIELDS") || keyword_at(bytes, j, "COLUMNS") {
                        let kw = if keyword_at(bytes, j, "FIELDS") {
                            "FIELDS".len()
                        } else {
                            "COLUMNS".len()
                        };
                        j = skip_ws(bytes, j + kw);
                        if keyword_at(bytes, j, "TERMINATED") {
                            j = skip_ws(bytes, j + "TERMINATED".len());
                            if !keyword_at(bytes, j, "BY") {
                                return Err(Error::new(
                                    ERR_PARSE_ERROR,
                                    "LOAD DATA FIELDS TERMINATED BY",
                                ));
                            }
                            j = skip_ws(bytes, j + "BY".len());
                            let (term, next) = read_sql_string(bytes, j)?;
                            fields_terminated_by = unescape_sql_string(&term);
                            j = next;
                            continue;
                        }
                        return Err(Error::unsupported("LOAD DATA FIELDS option"));
                    }
                    if keyword_at(bytes, j, "LINES") {
                        j = skip_ws(bytes, j + "LINES".len());
                        if keyword_at(bytes, j, "TERMINATED") {
                            j = skip_ws(bytes, j + "TERMINATED".len());
                            if !keyword_at(bytes, j, "BY") {
                                return Err(Error::new(
                                    ERR_PARSE_ERROR,
                                    "LOAD DATA LINES TERMINATED BY",
                                ));
                            }
                            j = skip_ws(bytes, j + "BY".len());
                            let (term, next) = read_sql_string(bytes, j)?;
                            lines_terminated_by = unescape_sql_string(&term);
                            j = next;
                            continue;
                        }
                        return Err(Error::unsupported("LOAD DATA LINES option"));
                    }
                    if keyword_at(bytes, j, "IGNORE") {
                        j = skip_ws(bytes, j + "IGNORE".len());
                        let (n, next) = read_signed_number(bytes, j)?;
                        if n < 0 {
                            return Err(Error::new(ERR_PARSE_ERROR, "IGNORE lines must be >= 0"));
                        }
                        ignore_lines = n as u64;
                        j = skip_ws(bytes, next);
                        if keyword_at(bytes, j, "LINES") || keyword_at(bytes, j, "ROWS") {
                            let kw = if keyword_at(bytes, j, "LINES") {
                                "LINES".len()
                            } else {
                                "ROWS".len()
                            };
                            j = skip_ws(bytes, j + kw);
                        }
                        continue;
                    }
                    if bytes[j] == b'(' {
                        j += 1;
                        loop {
                            j = skip_ws(bytes, j);
                            if j < bytes.len() && bytes[j] == b')' {
                                j += 1;
                                break;
                            }
                            let (col, next) = read_ident(bytes, j);
                            if col.is_empty() {
                                return Err(Error::new(
                                    ERR_PARSE_ERROR,
                                    "LOAD DATA column list",
                                ));
                            }
                            columns.push(col);
                            j = skip_ws(bytes, next);
                            if j < bytes.len() && bytes[j] == b',' {
                                j += 1;
                                continue;
                            }
                            if j < bytes.len() && bytes[j] == b')' {
                                j += 1;
                                break;
                            }
                            return Err(Error::new(ERR_PARSE_ERROR, "LOAD DATA column list"));
                        }
                        continue;
                    }
                    break;
                }
                out.push_str("SET NAMES '__sealdb_load__'");
                ops.push(Some(Statement::LoadData(LoadDataStmt {
                    local,
                    path,
                    table: TableName::new(schema, name),
                    fields_terminated_by,
                    lines_terminated_by,
                    ignore_lines,
                    columns,
                })));
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok((out, ops))
}

fn read_sql_string(bytes: &[u8], start: usize) -> Result<(String, usize)> {
    let i = skip_ws(bytes, start);
    if i >= bytes.len() || (bytes[i] != b'\'' && bytes[i] != b'"') {
        return Err(Error::new(ERR_PARSE_ERROR, "expected string literal"));
    }
    let quote = bytes[i];
    let mut j = i + 1;
    let mut out = String::new();
    while j < bytes.len() {
        if bytes[j] == quote {
            if j + 1 < bytes.len() && bytes[j + 1] == quote {
                out.push(quote as char);
                j += 2;
                continue;
            }
            return Ok((out, j + 1));
        }
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 1;
            match bytes[j] {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'0' => out.push('\0'),
                other => out.push(other as char),
            }
            j += 1;
            continue;
        }
        out.push(bytes[j] as char);
        j += 1;
    }
    Err(Error::new(ERR_PARSE_ERROR, "unterminated string literal"))
}

fn unescape_sql_string(s: &str) -> String {
    // Already unescaped by read_sql_string; keep for TERMINATED BY values.
    s.to_string()
}

/// Extract CREATE/DROP/ALTER SEQUENCE (sqlparser MySQL coverage is incomplete).
fn extract_sequence_ddl(sql: &str) -> Result<(String, Vec<Option<Statement>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "CREATE") {
            let after_create = skip_ws(bytes, i + "CREATE".len());
            if keyword_at(bytes, after_create, "SEQUENCE") {
                let mut j = skip_ws(bytes, after_create + "SEQUENCE".len());
                let if_not_exists = if keyword_at(bytes, j, "IF") {
                    let after_if = skip_ws(bytes, j + "IF".len());
                    if keyword_at(bytes, after_if, "NOT") {
                        let after_not = skip_ws(bytes, after_if + "NOT".len());
                        if keyword_at(bytes, after_not, "EXISTS") {
                            j = skip_ws(bytes, after_not + "EXISTS".len());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                let (schema, name, after_name) = read_qualified_name(bytes, j)?;
                j = skip_ws(bytes, after_name);
                let (options, end) = parse_sequence_option_tail(bytes, j)?;
                out.push_str("SET NAMES '__sealdb_seq__'");
                ops.push(Some(Statement::CreateSequence(CreateSequenceStmt {
                    if_not_exists,
                    name: TableName::new(schema, name),
                    options,
                })));
                i = end;
                continue;
            }
        }
        if keyword_at(bytes, i, "DROP") {
            let after_drop = skip_ws(bytes, i + "DROP".len());
            if keyword_at(bytes, after_drop, "SEQUENCE") {
                let mut j = skip_ws(bytes, after_drop + "SEQUENCE".len());
                let if_exists = if keyword_at(bytes, j, "IF") {
                    let after_if = skip_ws(bytes, j + "IF".len());
                    if keyword_at(bytes, after_if, "EXISTS") {
                        j = skip_ws(bytes, after_if + "EXISTS".len());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                let mut sequences = Vec::new();
                loop {
                    let (schema, name, next) = read_qualified_name(bytes, j)?;
                    sequences.push(TableName::new(schema, name));
                    j = skip_ws(bytes, next);
                    if j < bytes.len() && bytes[j] == b',' {
                        j = skip_ws(bytes, j + 1);
                        continue;
                    }
                    break;
                }
                out.push_str("SET NAMES '__sealdb_seq__'");
                ops.push(Some(Statement::DropSequence(DropSequenceStmt {
                    if_exists,
                    sequences,
                })));
                i = j;
                continue;
            }
        }
        if keyword_at(bytes, i, "ALTER") {
            let after_alter = skip_ws(bytes, i + "ALTER".len());
            if keyword_at(bytes, after_alter, "SEQUENCE") {
                let mut j = skip_ws(bytes, after_alter + "SEQUENCE".len());
                let if_exists = if keyword_at(bytes, j, "IF") {
                    let after_if = skip_ws(bytes, j + "IF".len());
                    if keyword_at(bytes, after_if, "EXISTS") {
                        j = skip_ws(bytes, after_if + "EXISTS".len());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                let (schema, name, after_name) = read_qualified_name(bytes, j)?;
                j = skip_ws(bytes, after_name);
                let (options, end) = parse_sequence_option_tail(bytes, j)?;
                out.push_str("SET NAMES '__sealdb_seq__'");
                ops.push(Some(Statement::AlterSequence(AlterSequenceStmt {
                    if_exists,
                    name: TableName::new(schema, name),
                    options,
                })));
                i = end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok((out, ops))
}

fn read_qualified_name(
    bytes: &[u8],
    start: usize,
) -> Result<(Option<String>, String, usize)> {
    let (first, after_first) = read_ident(bytes, start);
    if first.is_empty() {
        return Err(Error::new(ERR_PARSE_ERROR, "missing sequence name"));
    }
    let after_first = skip_ws(bytes, after_first);
    if after_first < bytes.len() && bytes[after_first] == b'.' {
        let (second, after_second) = read_ident(bytes, skip_ws(bytes, after_first + 1));
        if second.is_empty() {
            return Err(Error::new(ERR_PARSE_ERROR, "missing sequence name"));
        }
        Ok((Some(first), second, after_second))
    } else {
        Ok((None, first, after_first))
    }
}

fn parse_sequence_option_tail(bytes: &[u8], mut i: usize) -> Result<(SequenceOptionSet, usize)> {
    let mut options = SequenceOptionSet::default();
    loop {
        i = skip_ws(bytes, i);
        if i >= bytes.len() || bytes[i] == b';' {
            break;
        }
        if keyword_at(bytes, i, "INCREMENT") {
            let mut j = skip_ws(bytes, i + "INCREMENT".len());
            if keyword_at(bytes, j, "BY") {
                j = skip_ws(bytes, j + "BY".len());
            }
            let (num, next) = read_signed_number(bytes, j)?;
            options.increment = Some(num);
            i = next;
            continue;
        }
        if keyword_at(bytes, i, "MINVALUE") {
            let j = skip_ws(bytes, i + "MINVALUE".len());
            let (num, next) = read_signed_number(bytes, j)?;
            options.min_value = Some(num);
            i = next;
            continue;
        }
        if keyword_at(bytes, i, "MAXVALUE") {
            let j = skip_ws(bytes, i + "MAXVALUE".len());
            let (num, next) = read_signed_number(bytes, j)?;
            options.max_value = Some(num);
            i = next;
            continue;
        }
        if keyword_at(bytes, i, "START") {
            let mut j = skip_ws(bytes, i + "START".len());
            if keyword_at(bytes, j, "WITH") {
                j = skip_ws(bytes, j + "WITH".len());
            }
            let (num, next) = read_signed_number(bytes, j)?;
            options.start = Some(num);
            i = next;
            continue;
        }
        if keyword_at(bytes, i, "CACHE") {
            let j = skip_ws(bytes, i + "CACHE".len());
            let (num, next) = read_signed_number(bytes, j)?;
            options.cache = Some(num);
            i = next;
            continue;
        }
        if keyword_at(bytes, i, "NOCACHE") || keyword_at(bytes, i, "NO") {
            if keyword_at(bytes, i, "NOCACHE") {
                options.no_cache = true;
                i = skip_ws(bytes, i + "NOCACHE".len());
                continue;
            }
            let after_no = skip_ws(bytes, i + "NO".len());
            if keyword_at(bytes, after_no, "MINVALUE") {
                options.no_min = true;
                i = skip_ws(bytes, after_no + "MINVALUE".len());
                continue;
            }
            if keyword_at(bytes, after_no, "MAXVALUE") {
                options.no_max = true;
                i = skip_ws(bytes, after_no + "MAXVALUE".len());
                continue;
            }
            if keyword_at(bytes, after_no, "CACHE") {
                options.no_cache = true;
                i = skip_ws(bytes, after_no + "CACHE".len());
                continue;
            }
            if keyword_at(bytes, after_no, "CYCLE") {
                options.cycle = Some(false);
                i = skip_ws(bytes, after_no + "CYCLE".len());
                continue;
            }
        }
        if keyword_at(bytes, i, "CYCLE") {
            options.cycle = Some(true);
            i = skip_ws(bytes, i + "CYCLE".len());
            continue;
        }
        if keyword_at(bytes, i, "NOCYCLE") {
            options.cycle = Some(false);
            i = skip_ws(bytes, i + "NOCYCLE".len());
            continue;
        }
        if keyword_at(bytes, i, "RESTART") {
            let mut j = skip_ws(bytes, i + "RESTART".len());
            if keyword_at(bytes, j, "WITH") {
                j = skip_ws(bytes, j + "WITH".len());
                let (num, next) = read_signed_number(bytes, j)?;
                options.start = Some(num);
                i = next;
                continue;
            }
            // RESTART without WITH → restart at original start; encode as start=None sentinel via cache=-1? Use start Some with special: leave start unset and set increment None; handle in DDL as restart_to_start flag.
            // Encode RESTART as no_cache=false + start = i64::MIN sentinel? Cleaner: use min_value Option and a dedicated field.
            // For MVB: RESTART alone sets start to current SequenceInfo.start via options.start = None and cycle=None and a comment in options — use cache = Some(0) as restart marker? Ugly.
            // Use options.start = Some(i64::MIN) as RESTART-without-value sentinel.
            options.start = Some(i64::MIN);
            i = j;
            continue;
        }
        break;
    }
    Ok((options, i))
}

fn read_signed_number(bytes: &[u8], start: usize) -> Result<(i64, usize)> {
    let mut i = skip_ws(bytes, start);
    let neg = if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
        true
    } else if i < bytes.len() && bytes[i] == b'+' {
        i += 1;
        false
    } else {
        false
    };
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return Err(Error::new(ERR_PARSE_ERROR, "expected sequence number"));
    }
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    let raw = std::str::from_utf8(&bytes[i..j]).map_err(|_| {
        Error::new(ERR_PARSE_ERROR, "invalid sequence number")
    })?;
    let mut value: i64 = raw
        .parse()
        .map_err(|_| Error::new(ERR_PARSE_ERROR, format!("invalid sequence number {raw}")))?;
    if neg {
        value = -value;
    }
    Ok((value, j))
}

fn lower_create_view(create: &CreateView) -> Result<Statement> {
    if create.materialized {
        return Err(Error::unsupported("CREATE MATERIALIZED VIEW"));
    }
    if create.temporary {
        return Err(Error::unsupported("CREATE TEMPORARY VIEW"));
    }
    if create.if_not_exists {
        return Err(Error::unsupported("CREATE VIEW IF NOT EXISTS"));
    }
    let mut select = lower_query(&create.query)?;
    // Keep restore text for ViewInfo.select_stmt persistence.
    let mut ctx = crate::format::RestoreCtx::new();
    select.restore(&mut ctx)?;
    select.text = ctx.into_string();
    let (algorithm, definer, security) = match &create.params {
        Some(params) => {
            let algorithm = params
                .algorithm
                .as_ref()
                .map(|a| match a {
                    CreateViewAlgorithm::Undefined => "UNDEFINED",
                    CreateViewAlgorithm::Merge => "MERGE",
                    CreateViewAlgorithm::TempTable => "TEMPTABLE",
                })
                .unwrap_or("UNDEFINED")
                .to_string();
            let definer = params
                .definer
                .as_ref()
                .map(|d| match d {
                    GranteeName::UserHost { user, host } => {
                        format!("`{}`@`{}`", user.value, host.value)
                    }
                    GranteeName::ObjectName(name) => name.to_string(),
                })
                .unwrap_or_else(|| "CURRENT_USER".to_string());
            let security = params
                .security
                .as_ref()
                .map(|s| match s {
                    CreateViewSecurity::Definer => "DEFINER",
                    CreateViewSecurity::Invoker => "INVOKER",
                })
                .unwrap_or("DEFINER")
                .to_string();
            (algorithm, definer, security)
        }
        None => (
            "UNDEFINED".to_string(),
            "CURRENT_USER".to_string(),
            "DEFINER".to_string(),
        ),
    };
    Ok(Statement::CreateView(CreateViewStmt {
        or_replace: create.or_replace,
        view_name: object_table_name(&create.name)?,
        cols: create
            .columns
            .iter()
            .map(|col| col.name.value.clone())
            .collect(),
        select: Box::new(select),
        algorithm,
        definer,
        security,
        check_option: "CASCADED".to_string(),
    }))
}

fn lower_create_table(create: &CreateTable) -> Result<Statement> {
    // LIKE / AS SELECT. TEMPORARY stays deferred ( gaps).
    if create.temporary {
        return Err(Error::unsupported("TEMPORARY CREATE TABLE"));
    }
    let refer_table = match &create.like {
        Some(CreateTableLikeKind::Plain(like) | CreateTableLikeKind::Parenthesized(like)) => {
            if like.defaults.is_some() {
                return Err(Error::unsupported(
                    "CREATE TABLE LIKE INCLUDING/EXCLUDING DEFAULTS",
                ));
            }
            Some(object_table_name(&like.name)?)
        }
        None => None,
    };
    let select = match create.query.as_deref() {
        Some(query) => Some(Box::new(lower_query(query)?)),
        None => None,
    };
    if refer_table.is_some() && select.is_some() {
        return Err(Error::unsupported("CREATE TABLE LIKE ... AS SELECT"));
    }
    if refer_table.is_some() && (!create.columns.is_empty() || !create.constraints.is_empty()) {
        return Err(Error::unsupported("CREATE TABLE LIKE with column list"));
    }
    let cols = create
        .columns
        .iter()
        .map(|col| {
            Ok(ColumnDef {
                name: col.name.value.clone(),
                type_name: col.data_type.to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if cols.is_empty() && refer_table.is_none() && select.is_none() {
        return Err(Error::unsupported("CREATE TABLE without columns"));
    }
    let indices = lower_table_indices(&create.constraints)?;
    Ok(Statement::CreateTable(CreateTableStmt {
        if_not_exists: create.if_not_exists,
        table: object_table_name(&create.name)?,
        cols,
        indices,
        partition: None,
        refer_table,
        select,
    }))
}

fn lower_values(values: &Values) -> Result<Vec<Vec<crate::ast::expressions::ExprNode>>> {
    values
        .rows
        .iter()
        .map(|row| row.iter().map(lower_expr).collect())
        .collect()
}

fn object_table_name(name: &ObjectName) -> Result<TableName> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [table] => Ok(TableName::new(None, table.clone())),
        [schema, table] => Ok(TableName::new(Some(schema.clone()), table.clone())),
        _ => Err(Error::unsupported(format!("object name {name}"))),
    }
}

fn object_name_last(name: &ObjectName) -> Result<String> {
    object_name_parts(name)?
        .pop()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "empty object name"))
}

/// Resolve SET target name + scope from sqlparser `ContextModifier` and/or
/// `@@GLOBAL.` / `@@SESSION.` object-name prefixes.
fn set_var_name_and_scope(
    name: &ObjectName,
    modifier: Option<ContextModifier>,
) -> Result<(String, SetVarScope)> {
    let mut parts = object_name_parts(name)?;
    if parts.is_empty() {
        return Err(Error::new(ERR_PARSE_ERROR, "empty SET variable name"));
    }
    let mut scope = match modifier {
        Some(ContextModifier::Global) => SetVarScope::Global,
        Some(ContextModifier::Session) | Some(ContextModifier::Local) | None => {
            SetVarScope::Session
        }
    };
    if parts.len() >= 2 {
        let head = parts[0].trim_start_matches('@').to_ascii_lowercase();
        if head == "global" {
            scope = SetVarScope::Global;
            parts.remove(0);
        } else if head == "session" || head == "local" {
            scope = SetVarScope::Session;
            parts.remove(0);
        }
    }
    // Keep leading `@` for user vars (`SET @a = 1`); `apply_set` strips
    // `@`/`@@` when matching system variable names.
    let raw = parts
        .pop()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "empty SET variable name"))?;
    Ok((raw, scope))
}

fn schema_name_last(name: &SchemaName) -> Result<String> {
    match name {
        SchemaName::Simple(name) => object_name_last(name),
        other => Err(Error::unsupported(format!("schema name {other}"))),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<String>> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.clone())
                .ok_or_else(|| Error::unsupported(format!("object name part {part}")))
        })
        .collect()
}

fn lower_select_item(item: &SelectItem) -> Result<SelectField> {
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: None,
        }),
        SelectItem::ExprWithAlias { expr, alias } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: Some(alias.value.clone()),
        }),
        SelectItem::ExprWithAliases { expr, aliases } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: aliases.first().map(|a| a.value.clone()),
        }),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => Ok(SelectField {
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
        }),
    }
}

fn lower_expr(expr: &Expr) -> Result<ExprNode> {
    match expr {
        Expr::Value(v) => Ok(ExprNode::Value(ValueExpr {
            datum: lower_value(&v.value)?,
        })),
        Expr::BinaryOp { left, op, right } => Ok(ExprNode::Binary(BinaryOpExpr {
            op: lower_binary_op(op)?,
            left: Box::new(lower_expr(left)?),
            right: Box::new(lower_expr(right)?),
        })),
        Expr::UnaryOp { op, expr } => Ok(ExprNode::Unary(UnaryOpExpr {
            op: lower_unary_op(op)?,
            expr: Box::new(lower_expr(expr)?),
        })),
        Expr::Nested(inner) => lower_expr(inner),
        Expr::Cast {
            expr, data_type, ..
        } => Ok(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(lower_expr(expr)?),
            target: data_type_name(data_type),
        })),
        Expr::Function(func) => lower_function(func),
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("null") => {
            Ok(ExprNode::Value(ValueExpr { datum: Datum::Null }))
        }
        Expr::Identifier(ident) => Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
            schema: None,
            table: None,
            name: ident.value.clone(),
        })),
        Expr::CompoundIdentifier(parts) => {
            let names: Vec<String> = parts.iter().map(|p| p.value.clone()).collect();
            let (schema, table, name) = match names.as_slice() {
                [name] => (None, None, name.clone()),
                [table, name] => (None, Some(table.clone()), name.clone()),
                [schema, table, name] => (Some(schema.clone()), Some(table.clone()), name.clone()),
                _ => return Err(Error::unsupported(format!("column name {expr}"))),
            };
            Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
                schema,
                table,
                name,
            }))
        }
        Expr::Exists { subquery, negated } => {
            let query = lower_result_set(subquery)?;
            if query.has_join() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::Exists(ExistsExpr {
                not: *negated,
                query: Box::new(query),
            }))
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let query = lower_result_set(subquery)?;
            if query.has_join() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::InSubquery(InSubqueryExpr {
                expr: Box::new(lower_expr(expr)?),
                not: *negated,
                query: Box::new(query),
            }))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => Ok(ExprNode::InList(InListExpr {
            expr: Box::new(lower_expr(expr)?),
            not: *negated,
            list: list.iter().map(lower_expr).collect::<Result<Vec<_>>>()?,
        })),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Ok(ExprNode::Between(BetweenExpr {
            expr: Box::new(lower_expr(expr)?),
            not: *negated,
            low: Box::new(lower_expr(low)?),
            high: Box::new(lower_expr(high)?),
        })),
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
            ..
        } => {
            if escape_char.is_some() {
                return Err(Error::unsupported("LIKE ESCAPE"));
            }
            Ok(ExprNode::Like(PatternLikeExpr {
                expr: Box::new(lower_expr(expr)?),
                not: *negated,
                pattern: Box::new(lower_expr(pattern)?),
                ci: false,
            }))
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            ..
        } => {
            if escape_char.is_some() {
                return Err(Error::unsupported("ILIKE ESCAPE"));
            }
            Ok(ExprNode::Like(PatternLikeExpr {
                expr: Box::new(lower_expr(expr)?),
                not: *negated,
                pattern: Box::new(lower_expr(pattern)?),
                ci: true,
            }))
        }
        Expr::IsNull(expr) => Ok(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(lower_expr(expr)?),
            not: false,
        })),
        Expr::IsNotNull(expr) => Ok(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(lower_expr(expr)?),
            not: true,
        })),
        Expr::Subquery(subquery) => {
            let query = lower_result_set(subquery)?;
            if query.has_join() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::ScalarSubquery(ScalarSubqueryExpr {
                query: Box::new(query),
            }))
        }
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => lower_quantified_cmp(left, compare_op, right, false),
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => lower_quantified_cmp(left, compare_op, right, true),
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("expression {other} is not available in this phase"),
        )),
    }
}

fn lower_function(func: &Function) -> Result<ExprNode> {
    if let Some(over) = &func.over {
        return lower_window_func(func, over);
    }
    let name = func.name.to_string();
    let distinct = match &func.args {
        FunctionArguments::List(list) => {
            matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct))
        }
        _ => false,
    };
    let args = match &func.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => lower_expr(expr),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                | FunctionArg::Unnamed(FunctionArgExpr::WildcardWithOptions(_)) => {
                    Ok(ExprNode::Value(ValueExpr {
                        datum: Datum::Bytes(b"*".to_vec()),
                    }))
                }
                _ => Err(Error::unsupported("function argument form")),
            })
            .collect::<Result<Vec<_>>>()?,
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => {
            return Err(Error::unsupported("function subquery argument"));
        }
    };
    let args = if name.eq_ignore_ascii_case("COUNT")
        && matches!(
            args.as_slice(),
            [ExprNode::Value(v)] if matches!(&v.datum, Datum::Bytes(b) if b == b"*")
        ) {
        Vec::new()
    } else {
        args
    };
    Ok(ExprNode::Func(FuncCallExpr {
        name,
        args,
        distinct,
    }))
}

fn lower_window_func(func: &Function, over: &WindowType) -> Result<ExprNode> {
    let ignore_nulls = window_ignore_nulls(func);
    let name = func.name.to_string();
    match name.to_ascii_uppercase().as_str() {
        "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "LAG"
        | "LEAD" | "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" | "NTILE" => {}
        other => return Err(Error::unsupported(format!("window function {other}"))),
    }
    let args = match &func.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => lower_expr(expr),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                | FunctionArg::Unnamed(FunctionArgExpr::WildcardWithOptions(_)) => {
                    Ok(ExprNode::Value(ValueExpr {
                        datum: Datum::Bytes(b"*".to_vec()),
                    }))
                }
                _ => Err(Error::unsupported("window function argument form")),
            })
            .collect::<Result<Vec<_>>>()?,
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => {
            return Err(Error::unsupported("window function subquery argument"));
        }
    };
    let args = if name.eq_ignore_ascii_case("COUNT")
        && matches!(
            args.as_slice(),
            [ExprNode::Value(v)] if matches!(&v.datum, Datum::Bytes(b) if b == b"*")
        ) {
        Vec::new()
    } else {
        args
    };
    let (window_ref, spec) = match over {
        WindowType::NamedWindow(ident) => (Some(ident.value.clone()), WindowSpec::default()),
        WindowType::WindowSpec(spec) => (
            spec.window_name.as_ref().map(|ident| ident.value.clone()),
            lower_over_spec(spec)?,
        ),
    };
    Ok(ExprNode::Window(WindowFuncExpr {
        name,
        args,
        spec,
        window_ref,
        ignore_nulls,
    }))
}

fn window_ignore_nulls(func: &Function) -> bool {
    let mut ignore = matches!(func.null_treatment, Some(NullTreatment::IgnoreNulls));
    if let FunctionArguments::List(list) = &func.args {
        for clause in &list.clauses {
            if let FunctionArgumentClause::IgnoreOrRespectNulls(treatment) = clause {
                ignore = matches!(treatment, NullTreatment::IgnoreNulls);
            }
        }
    }
    ignore
}

fn lower_over_spec(spec: &sqlparser::ast::WindowSpec) -> Result<WindowSpec> {
    Ok(WindowSpec {
        partition_by: spec
            .partition_by
            .iter()
            .map(lower_expr)
            .collect::<Result<Vec<_>>>()?,
        order_by: lower_order_by_exprs(&spec.order_by)?,
        frame: spec
            .window_frame
            .as_ref()
            .map(lower_window_frame)
            .transpose()?,
    })
}

/// Lower `WINDOW w AS (...)` definitions in SELECT order.
fn lower_named_windows(defs: &[NamedWindowDefinition]) -> Result<HashMap<String, WindowSpec>> {
    let mut windows = HashMap::new();
    for NamedWindowDefinition(name, expr) in defs {
        let spec = match expr {
            NamedWindowExpr::WindowSpec(spec) => lower_over_spec(spec)?,
            NamedWindowExpr::NamedWindow(other) => {
                windows.get(&other.value).cloned().ok_or_else(|| {
                    Error::unsupported(format!("unknown named window {}", other.value))
                })?
            }
        };
        if windows
            .keys()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&name.value))
        {
            return Err(Error::unsupported("duplicate named window"));
        }
        windows.insert(name.value.clone(), spec);
    }
    Ok(windows)
}

fn resolve_named_window(expr: &mut ExprNode, windows: &HashMap<String, WindowSpec>) -> Result<()> {
    let ExprNode::Window(window) = expr else {
        return Ok(());
    };
    let Some(name) = window.window_ref.take() else {
        return Ok(());
    };
    let Some(base) = windows
        .iter()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        .map(|(_, spec)| spec)
    else {
        return Err(Error::unsupported(format!("unknown named window {name}")));
    };
    window.spec = merge_window_spec(base, &window.spec)?;
    Ok(())
}

fn merge_window_spec(base: &WindowSpec, extra: &WindowSpec) -> Result<WindowSpec> {
    let mut out = base.clone();
    if !extra.partition_by.is_empty() {
        if !out.partition_by.is_empty() {
            return Err(Error::unsupported("cannot override PARTITION BY"));
        }
        out.partition_by = extra.partition_by.clone();
    }
    if !extra.order_by.is_empty() {
        if !out.order_by.is_empty() {
            return Err(Error::unsupported("cannot override ORDER BY"));
        }
        out.order_by = extra.order_by.clone();
    }
    if extra.frame.is_some() {
        if out.frame.is_some() {
            return Err(Error::unsupported("cannot override frame"));
        }
        out.frame = extra.frame.clone();
    }
    Ok(out)
}

/// Lower `ROWS|RANGE|GROUPS BETWEEN start AND end`. RANGE offsets stay
/// unsupported (value-based); GROUPS offsets count peer groups.
fn lower_window_frame(frame: &sqlparser::ast::WindowFrame) -> Result<WindowFrameSpec> {
    let units = match frame.units {
        SqlWindowFrameUnits::Rows => WindowFrameUnits::Rows,
        SqlWindowFrameUnits::Range => WindowFrameUnits::Range,
        SqlWindowFrameUnits::Groups => WindowFrameUnits::Groups,
    };
    let start = lower_frame_bound(&frame.start_bound)?;
    let end = match &frame.end_bound {
        Some(bound) => lower_frame_bound(bound)?,
        None => WindowFrameBound::CurrentRow,
    };
    if units == WindowFrameUnits::Range
        && matches!(
            (&start, &end),
            (
                WindowFrameBound::Preceding(_) | WindowFrameBound::Following(_),
                _
            ) | (
                _,
                WindowFrameBound::Preceding(_) | WindowFrameBound::Following(_)
            )
        )
    {
        return Err(Error::unsupported("RANGE offset frame"));
    }
    Ok(WindowFrameSpec { units, start, end })
}

fn lower_frame_bound(bound: &SqlWindowFrameBound) -> Result<WindowFrameBound> {
    match bound {
        SqlWindowFrameBound::CurrentRow => Ok(WindowFrameBound::CurrentRow),
        SqlWindowFrameBound::Preceding(None) => Ok(WindowFrameBound::UnboundedPreceding),
        SqlWindowFrameBound::Following(None) => Ok(WindowFrameBound::UnboundedFollowing),
        SqlWindowFrameBound::Preceding(Some(expr)) => {
            Ok(WindowFrameBound::Preceding(literal_frame_offset(expr)?))
        }
        SqlWindowFrameBound::Following(Some(expr)) => {
            Ok(WindowFrameBound::Following(literal_frame_offset(expr)?))
        }
    }
}

fn literal_frame_offset(expr: &Expr) -> Result<u64> {
    match lower_expr(expr)? {
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(n),
        }) if n >= 0 => Ok(n as u64),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(n),
        }) => Ok(n),
        other => Err(Error::unsupported(format!("frame offset {other:?}"))),
    }
}

fn lower_order_by_exprs(items: &[OrderByExpr]) -> Result<Vec<ByItem>> {
    items
        .iter()
        .map(|item| {
            Ok(ByItem {
                expr: lower_expr(&item.expr)?,
                desc: matches!(item.options.sort, Some(OrderBySort::Desc)),
            })
        })
        .collect()
}

fn lower_value(value: &Value) -> Result<Datum> {
    match value {
        Value::Number(n, _) => {
            if n.contains('.') {
                n.parse::<f64>()
                    .map(Datum::F64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            } else {
                n.parse::<i64>()
                    .map(Datum::I64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            }
        }
        Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => {
            Ok(Datum::Bytes(s.as_bytes().to_vec()))
        }
        Value::Boolean(b) => Ok(Datum::I64(i64::from(*b))),
        Value::Null => Ok(Datum::Null),
        other => Err(Error::unsupported(format!("literal {other}"))),
    }
}

/// Lower `left op ANY|ALL (subquery)`.
/// `= ANY`/`= SOME` become `IN`; `<> ALL` becomes `NOT IN`; other ops keep
/// `QuantifiedCmp` and run through NestedLoopApply.
fn lower_quantified_cmp(
    left: &Expr,
    compare_op: &BinaryOperator,
    right: &Expr,
    all: bool,
) -> Result<ExprNode> {
    let query = match right {
        Expr::Subquery(subquery) => lower_result_set(subquery)?,
        other => {
            return Err(Error::unsupported(format!(
                "quantified comparison of {other}"
            )));
        }
    };
    if query.has_join() {
        return Err(Error::unsupported("subquery JOIN"));
    }
    let expr = lower_expr(left)?;
    let op = lower_binary_op(compare_op)?;
    match (all, op) {
        (false, Op::Eq) => Ok(ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(expr),
            not: false,
            query: Box::new(query),
        })),
        (true, Op::Ne) => Ok(ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(expr),
            not: true,
            query: Box::new(query),
        })),
        (_, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) => {
            Ok(ExprNode::QuantifiedCmp(QuantifiedCmpExpr {
                expr: Box::new(expr),
                op,
                all,
                query: Box::new(query),
            }))
        }
        _ => Err(Error::unsupported(format!(
            "quantified operator {}",
            op.restore()
        ))),
    }
}

fn lower_binary_op(op: &BinaryOperator) -> Result<Op> {
    Ok(match op {
        BinaryOperator::Plus => Op::Plus,
        BinaryOperator::Minus => Op::Minus,
        BinaryOperator::Multiply => Op::Mul,
        BinaryOperator::Divide => Op::Div,
        BinaryOperator::Modulo => Op::Mod,
        BinaryOperator::Eq => Op::Eq,
        BinaryOperator::NotEq => Op::Ne,
        BinaryOperator::Lt => Op::Lt,
        BinaryOperator::LtEq => Op::Le,
        BinaryOperator::Gt => Op::Gt,
        BinaryOperator::GtEq => Op::Ge,
        BinaryOperator::And => Op::LogicAnd,
        BinaryOperator::Or => Op::LogicOr,
        _ => return Err(Error::unsupported(format!("operator {op}"))),
    })
}

fn lower_unary_op(op: &UnaryOperator) -> Result<Op> {
    Ok(match op {
        UnaryOperator::Plus => Op::Plus,
        UnaryOperator::Minus => Op::UnaryMinus,
        UnaryOperator::Not => Op::Not,
        _ => return Err(Error::unsupported(format!("unary operator {op}"))),
    })
}

fn lower_create_index(create: &sqlparser::ast::CreateIndex) -> Result<Statement> {
    // Completeness wave / not this feature: USING, partial, invisible.
    if create.using.is_some()
        || !create.include.is_empty()
        || create.predicate.is_some()
        || !create.with.is_empty()
        || create.concurrently
    {
        return Err(Error::unsupported("CREATE INDEX option"));
    }
    for opt in &create.alter_options {
        match opt {
            AlterTableOperation::Algorithm { .. } | AlterTableOperation::Lock { .. } => {}
            other => return Err(Error::unsupported(format!("CREATE INDEX option {other}"))),
        }
    }
    let columns = create
        .columns
        .iter()
        .map(index_column_name)
        .collect::<Result<Vec<_>>>()?;
    if columns.is_empty() {
        return Err(Error::unsupported("CREATE INDEX without columns"));
    }
    let name = match &create.name {
        Some(name) => object_name_last(name)?,
        None => format!("idx_{}", columns.join("_")),
    };
    Ok(Statement::CreateIndex(CreateIndexStmt {
        if_not_exists: create.if_not_exists,
        table: object_table_name(&create.table_name)?,
        name,
        columns,
        unique: create.unique,
    }))
}

fn lower_rename_tables(renames: &[SqlRenameTable]) -> Result<Statement> {
    if renames.len() != 1 {
        return Err(Error::unsupported("RENAME TABLE multiple pairs"));
    }
    Ok(Statement::AlterTable(AlterTableStmt {
        table: object_table_name(&renames[0].old_name)?,
        specs: vec![AlterTableSpec::RenameTable {
            new_name: object_table_name(&renames[0].new_name)?,
        }],
    }))
}

fn lower_column_position(pos: &Option<MySQLColumnPosition>) -> ColumnPosition {
    match pos {
        None => ColumnPosition::None,
        Some(MySQLColumnPosition::First) => ColumnPosition::First,
        Some(MySQLColumnPosition::After(ident)) => ColumnPosition::After(ident.value.clone()),
    }
}

fn lower_alter_table(alter: &sqlparser::ast::AlterTable) -> Result<Statement> {
    // Completeness wave / not this feature: generated columns, partitions
    // beyond ADD/DROP COLUMN|INDEX.
    if alter.if_exists || alter.only || alter.location.is_some() || alter.on_cluster.is_some() {
        return Err(Error::unsupported("ALTER TABLE option"));
    }
    if alter.operations.is_empty() {
        return Err(Error::unsupported("ALTER TABLE without operation"));
    }
    let specs = alter
        .operations
        .iter()
        .map(lower_alter_spec)
        .collect::<Result<Vec<_>>>()?;
    Ok(Statement::AlterTable(AlterTableStmt {
        table: object_table_name(&alter.name)?,
        specs,
    }))
}

fn lower_alter_spec(op: &AlterTableOperation) -> Result<AlterTableSpec> {
    match op {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => Ok(AlterTableSpec::AddColumn {
            if_not_exists: *if_not_exists,
            column: ColumnDef {
                name: column_def.name.value.clone(),
                type_name: column_def.data_type.to_string(),
            },
            position: lower_column_position(column_position),
        }),
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            ..
        } => {
            if column_names.len() != 1 {
                return Err(Error::unsupported("ALTER TABLE DROP multiple columns"));
            }
            Ok(AlterTableSpec::DropColumn {
                if_exists: *if_exists,
                name: column_names[0].value.clone(),
            })
        }
        AlterTableOperation::DropIndex { name } => Ok(AlterTableSpec::DropIndex {
            if_exists: false,
            name: name.value.clone(),
        }),
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            column_position,
        } => {
            if !options.is_empty() {
                return Err(Error::unsupported("CHANGE COLUMN option"));
            }
            Ok(AlterTableSpec::ChangeColumn {
                old_name: old_name.value.clone(),
                new_name: new_name.value.clone(),
                type_name: data_type.to_string(),
                position: lower_column_position(column_position),
            })
        }
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            column_position,
        } => {
            if !options.is_empty() {
                return Err(Error::unsupported("MODIFY COLUMN option"));
            }
            Ok(AlterTableSpec::ModifyColumn {
                name: col_name.value.clone(),
                type_name: data_type.to_string(),
                position: lower_column_position(column_position),
            })
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => {
            if old_column_name.value == RENAME_INDEX_MARKER {
                let (old_name, new_name) = split_rename_index_marker(&new_column_name.value)?;
                Ok(AlterTableSpec::RenameIndex { old_name, new_name })
            } else {
                Ok(AlterTableSpec::RenameColumn {
                    old_name: old_column_name.value.clone(),
                    new_name: new_column_name.value.clone(),
                })
            }
        }
        AlterTableOperation::Algorithm { algorithm, .. } => {
            Ok(AlterTableSpec::Algorithm(algorithm.to_string()))
        }
        AlterTableOperation::Lock { lock, .. } => Ok(AlterTableSpec::Lock(lock.to_string())),
        AlterTableOperation::RenameTable { table_name } => {
            let name = match table_name {
                RenameTableNameKind::To(name) | RenameTableNameKind::As(name) => name,
            };
            Ok(AlterTableSpec::RenameTable {
                new_name: object_table_name(name)?,
            })
        }
        AlterTableOperation::AddConstraint { constraint, .. } => match constraint {
            TableConstraint::Index(idx) => {
                let columns = idx
                    .columns
                    .iter()
                    .map(index_column_name)
                    .collect::<Result<Vec<_>>>()?;
                if columns.is_empty() {
                    return Err(Error::unsupported("INDEX without columns"));
                }
                if idx.index_type.is_some() {
                    return Err(Error::unsupported("INDEX USING type"));
                }
                let name = idx
                    .name
                    .as_ref()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_else(|| format!("idx_{}", columns.join("_")));
                Ok(AlterTableSpec::AddIndex(IndexDef {
                    name,
                    columns,
                    unique: false,
                    primary: false,
                }))
            }
            TableConstraint::Unique(idx) => Ok(AlterTableSpec::AddIndex(lower_unique_index(idx)?)),
            TableConstraint::PrimaryKey(idx) => {
                Ok(AlterTableSpec::AddIndex(lower_primary_key(idx)?))
            }
            other => Err(Error::unsupported(format!("ALTER TABLE ADD {other}"))),
        },
        other => Err(Error::unsupported(format!("ALTER TABLE {other}"))),
    }
}

fn lower_table_indices(constraints: &[TableConstraint]) -> Result<Vec<IndexDef>> {
    let mut indices = Vec::new();
    for constraint in constraints {
        match constraint {
            TableConstraint::Index(idx) => {
                let columns = idx
                    .columns
                    .iter()
                    .map(index_column_name)
                    .collect::<Result<Vec<_>>>()?;
                if columns.is_empty() {
                    return Err(Error::unsupported("INDEX without columns"));
                }
                // completeness wave: prefix length, functional, USING HASH.
                if idx.index_type.is_some() {
                    return Err(Error::unsupported("INDEX USING type"));
                }
                let name = idx
                    .name
                    .as_ref()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_else(|| format!("idx_{}", columns.join("_")));
                indices.push(IndexDef {
                    name,
                    columns,
                    unique: false,
                    primary: false,
                });
            }
            TableConstraint::Unique(idx) => indices.push(lower_unique_index(idx)?),
            TableConstraint::PrimaryKey(idx) => indices.push(lower_primary_key(idx)?),
            TableConstraint::ForeignKey(_) => {
                return Err(Error::unsupported("FOREIGN KEY"));
            }
            TableConstraint::Check(_) => return Err(Error::unsupported("CHECK constraint")),
            TableConstraint::FulltextOrSpatial(_) => {
                return Err(Error::unsupported("FULLTEXT / SPATIAL INDEX"));
            }
            other => {
                return Err(Error::unsupported(format!(
                    "CREATE TABLE constraint {other}"
                )));
            }
        }
    }
    Ok(indices)
}

fn lower_unique_index(idx: &sqlparser::ast::UniqueConstraint) -> Result<IndexDef> {
    if idx.index_type.is_some() || !idx.include.is_empty() {
        return Err(Error::unsupported("UNIQUE INDEX option"));
    }
    let columns = idx
        .columns
        .iter()
        .map(index_column_name)
        .collect::<Result<Vec<_>>>()?;
    if columns.is_empty() {
        return Err(Error::unsupported("UNIQUE INDEX without columns"));
    }
    let name = idx
        .index_name
        .as_ref()
        .or(idx.name.as_ref())
        .map(|ident| ident.value.clone())
        .unwrap_or_else(|| format!("idx_{}", columns.join("_")));
    Ok(IndexDef {
        name,
        columns,
        unique: true,
        primary: false,
    })
}

fn lower_primary_key(idx: &sqlparser::ast::PrimaryKeyConstraint) -> Result<IndexDef> {
    if idx.index_type.is_some() || !idx.include.is_empty() {
        return Err(Error::unsupported("PRIMARY KEY option"));
    }
    let columns = idx
        .columns
        .iter()
        .map(index_column_name)
        .collect::<Result<Vec<_>>>()?;
    if columns.is_empty() {
        return Err(Error::unsupported("PRIMARY KEY without columns"));
    }
    Ok(IndexDef {
        name: "PRIMARY".to_string(),
        columns,
        unique: true,
        primary: true,
    })
}

fn index_column_name(col: &sqlparser::ast::IndexColumn) -> Result<String> {
    match &col.column.expr {
        Expr::Identifier(ident) => Ok(ident.value.clone()),
        other => Err(Error::unsupported(format!("INDEX column {other}"))),
    }
}

fn data_type_name(tp: &DataType) -> String {
    match tp {
        DataType::Int(_) | DataType::Integer(_) | DataType::BigInt(_) | DataType::TinyInt(_) => {
            "SIGNED".to_string()
        }
        DataType::IntUnsigned(_)
        | DataType::IntegerUnsigned(_)
        | DataType::BigIntUnsigned(_)
        | DataType::TinyIntUnsigned(_)
        | DataType::Unsigned
        | DataType::UnsignedInteger => "UNSIGNED".to_string(),
        DataType::Double(_) | DataType::Float(_) | DataType::Real => "DOUBLE".to_string(),
        DataType::Decimal(_) | DataType::Numeric(_) => "DECIMAL".to_string(),
        DataType::Char(_) | DataType::Varchar(_) | DataType::Text | DataType::String(_) => {
            "CHAR".to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod unicode_literal_tests {
    use super::*;
    use crate::ast::expressions::ExprNode;
    use sealdb_types::Datum;

    #[test]
    fn preserves_utf8_string_literal() {
        let stmts = parse_sql("SELECT 'ä', 'ß'").unwrap();
        match &stmts[0] {
            Statement::Select(sel) => {
                let d0 = match &sel.fields[0].expr {
                    ExprNode::Value(v) => &v.datum,
                    other => panic!("expected value, got {other:?}"),
                };
                let d1 = match &sel.fields[1].expr {
                    ExprNode::Value(v) => &v.datum,
                    other => panic!("expected value, got {other:?}"),
                };
                assert_eq!(d0, &Datum::Bytes("ä".as_bytes().to_vec()));
                assert_eq!(d1, &Datum::Bytes("ß".as_bytes().to_vec()));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }
}

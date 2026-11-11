//! TiDB bootstrap DDL for the `mysql` system schema.
//!
//! TiDB creates the `mysql` database and its system tables while bootstrapping
//! a cluster (`session/bootstrap.go`): the grant tables (`user`,
//! `global_priv`, `global_grants`, `db`, …) are real InnoDB tables, so the
//! planner resolves them like any other table and their `i_user` indexes drive
//! `EXPLAIN` (e.g. `select * from mysql.user where user = 'xxx'` is an
//! `IndexLookUp`). SealDB mirrors that bootstrap so `mysql.*` queries plan the
//! same way instead of falling back to an unknown-table scan.

use crate::create_table::build_table_info;
use sealdb_errno::{Error, Result};
use sealdb_meta::{DBInfo, Mutator};
use sealdb_parser::ast::Statement;
use sealdb_parser::parser::Parser;

/// System schema name (lowercase, like TiDB's `mysql.SchemaName`).
pub const MYSQL_SCHEMA: &str = "mysql";
/// TiDB's default scratch schema.
pub const TEST_SCHEMA: &str = "test";

/// TiDB grant tables that `EXPLAIN` / privilege checks see. Each entry is one
/// `CREATE TABLE` with its `i_user` index inline; TiDB adds that index with a
/// follow-up `CREATE INDEX`, which lands as index id 2 here.
const SYSTEM_TABLES: &[&str] = &[
    // TiDB `mysql.user` (MySQL 5.7 columns + TiDB role/password columns).
    "CREATE TABLE user (\n\
        Host CHAR(255) NOT NULL DEFAULT '',\n\
        User CHAR(32) NOT NULL DEFAULT '',\n\
        authentication_string TEXT,\n\
        plugin CHAR(64) NOT NULL DEFAULT 'mysql_native_password',\n\
        Select_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Insert_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Update_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Delete_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Drop_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Process_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Grant_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        References_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Index_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Alter_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Show_db_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Super_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_tmp_table_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Lock_tables_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Execute_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_view_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Show_view_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_routine_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Alter_routine_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_user_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Event_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Trigger_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_tablespace_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        ssl_type ENUM('','ANY','X509','SPECIFIED') NOT NULL DEFAULT '',\n\
        ssl_cipher BLOB NOT NULL DEFAULT '',\n\
        x509_issuer BLOB NOT NULL DEFAULT '',\n\
        x509_subject BLOB NOT NULL DEFAULT '',\n\
        max_questions INT(11) UNSIGNED NOT NULL DEFAULT 0,\n\
        max_updates INT(11) UNSIGNED NOT NULL DEFAULT 0,\n\
        max_connections INT(11) UNSIGNED NOT NULL DEFAULT 0,\n\
        max_user_connections INT(11) UNSIGNED NOT NULL DEFAULT 0,\n\
        password_expired ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        password_last_changed TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n\
        password_lifetime SMALLINT(5) UNSIGNED DEFAULT NULL,\n\
        account_locked ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_role_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Drop_role_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Password_reuse_history SMALLINT(5) UNSIGNED DEFAULT NULL,\n\
        Password_reuse_time SMALLINT(5) UNSIGNED DEFAULT NULL,\n\
        Password_require_current ENUM('N','Y') DEFAULT NULL,\n\
        User_attributes JSON DEFAULT NULL,\n\
        PRIMARY KEY (Host, User),\n\
        KEY i_user (User)\n\
    )",
    // TiDB `mysql.global_priv`: the JSON grant snapshot for each account.
    "CREATE TABLE global_priv (\n\
        Host CHAR(255) NOT NULL DEFAULT '',\n\
        User CHAR(32) NOT NULL DEFAULT '',\n\
        Priv JSON NOT NULL,\n\
        PRIMARY KEY (Host, User),\n\
        KEY i_user (User)\n\
    )",
    // TiDB `mysql.global_grants`: dynamic privileges per account.
    "CREATE TABLE global_grants (\n\
        USER CHAR(32) NOT NULL DEFAULT '',\n\
        HOST CHAR(255) NOT NULL DEFAULT '',\n\
        PRIV CHAR(32) NOT NULL DEFAULT '',\n\
        WITH_GRANT_OPTION ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        PRIMARY KEY (USER, HOST, PRIV),\n\
        KEY i_user (USER)\n\
    )",
    // TiDB `mysql.db`: database-level grant rows.
    "CREATE TABLE db (\n\
        Host CHAR(255) NOT NULL DEFAULT '',\n\
        DB CHAR(64) NOT NULL DEFAULT '',\n\
        User CHAR(32) NOT NULL DEFAULT '',\n\
        Select_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Insert_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Update_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Delete_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Drop_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Grant_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        References_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Index_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Alter_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_tmp_table_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Lock_tables_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_view_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Show_view_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Create_routine_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Alter_routine_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Execute_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Event_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        Trigger_priv ENUM('N','Y') NOT NULL DEFAULT 'N',\n\
        PRIMARY KEY (Host, DB, User),\n\
        KEY i_user (User)\n\
    )",
    // TiDB `mysql.tables_priv`: table-level grant rows.
    "CREATE TABLE tables_priv (\n\
        Host CHAR(255) NOT NULL DEFAULT '',\n\
        DB CHAR(64) NOT NULL DEFAULT '',\n\
        User CHAR(32) NOT NULL DEFAULT '',\n\
        Table_name CHAR(64) NOT NULL DEFAULT '',\n\
        Grantor CHAR(255) NOT NULL DEFAULT '',\n\
        Timestamp TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n\
        Table_priv SET('Select','Insert','Update','Delete','Create','Drop','Grant','References','Index','Alter','Create View','Show view','Trigger') NOT NULL DEFAULT '',\n\
        Column_priv SET('Select','Insert','Update','References') NOT NULL DEFAULT '',\n\
        PRIMARY KEY (Host, DB, User, Table_name),\n\
        KEY i_user (User)\n\
    )",
    // TiDB `mysql.columns_priv`: column-level grant rows.
    "CREATE TABLE columns_priv (\n\
        Host CHAR(255) NOT NULL DEFAULT '',\n\
        DB CHAR(64) NOT NULL DEFAULT '',\n\
        User CHAR(32) NOT NULL DEFAULT '',\n\
        Table_name CHAR(64) NOT NULL DEFAULT '',\n\
        Column_name CHAR(64) NOT NULL DEFAULT '',\n\
        Timestamp TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n\
        Column_priv SET('Select','Insert','Update','References') NOT NULL DEFAULT '',\n\
        PRIMARY KEY (Host, DB, User, Table_name, Column_name),\n\
        KEY i_user (User)\n\
    )",
    // TiDB `mysql.expr_pushdown_blacklist`: expressions kept out of coprocessors.
    "CREATE TABLE expr_pushdown_blacklist (\n\
        name CHAR(100) NOT NULL,\n\
        store_type CHAR(100) NOT NULL DEFAULT 'tikv,tiflash,tidb',\n\
        reason VARCHAR(200)\n\
    )",
    // TiDB `mysql.opt_rule_blacklist`: disabled optimizer rules.
    "CREATE TABLE opt_rule_blacklist (\n\
        name CHAR(100) NOT NULL\n\
    )",
];

/// Create the `mysql` schema and its system tables when the cluster has no
/// `mysql` database yet (TiDB `bootstrap()`). Existing storage keeps its
/// schemas untouched, so this is a one-shot bootstrap.
///
/// TiDB also ships an empty `test` schema (`CREATE DATABASE IF NOT EXISTS
/// test`), which suites `use` before creating their tables.
///
/// Returns whether anything was created, so the caller can persist the meta.
pub fn bootstrap_system_tables(meta: &mut Mutator) -> Result<bool> {
    let mut created = false;
    let dbs = meta.list_databases()?;
    let has = |name: &str| dbs.iter().any(|db| db.name.l().eq_ignore_ascii_case(name));
    if !has(TEST_SCHEMA) {
        let db_id = meta.gen_global_id()?;
        let db = DBInfo::new(db_id, TEST_SCHEMA);
        meta.create_database(&db)?;
        created = true;
    }
    if has(MYSQL_SCHEMA) {
        if created {
            meta.gen_schema_version()?;
        }
        return Ok(created);
    }
    let db_id = meta.gen_global_id()?;
    let db = DBInfo::new(db_id, MYSQL_SCHEMA);
    meta.create_database(&db)?;
    let parser = Parser::new();
    for sql in SYSTEM_TABLES {
        let stmt = parser.parse_one_stmt(sql)?;
        let Statement::CreateTable(create) = stmt else {
            return Err(Error::unsupported("system table DDL is not CREATE TABLE"));
        };
        let table_id = meta.gen_global_id()?;
        let info = build_table_info(&create, table_id)?;
        meta.create_table_or_view(db_id, &info)?;
    }
    meta.gen_schema_version()?;
    Ok(true)
}

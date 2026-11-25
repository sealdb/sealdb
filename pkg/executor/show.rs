use crate::analyze::is_special_global_index;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_BAD_DB, ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_meta::{ColumnInfo, TableInfo};
use sealdb_parser::ast::{PrivilegeType, ShowStmt, ShowStmtKind, TableName};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct ShowExec {
    domain: Arc<Domain>,
    stmt: ShowStmt,
    current_db: String,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl ShowExec {
    pub fn new(domain: Arc<Domain>, stmt: ShowStmt, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }

    /// `SHOW COLUMNS|TABLES … WHERE <cond>`: TiDB builds the SHOW result rows
    /// and then filters them, comparing strings under the result's
    /// case-insensitive collation.
    fn apply_where_filter(&mut self) -> Result<()> {
        let Some(sealdb_parser::ast::ShowVarFilter::Where(expr)) = &self.stmt.filter else {
            return Ok(());
        };
        let mut schema = self.schema.clone();
        if self.stmt.kind == ShowStmtKind::Tables
            && let Some(first) = schema.first_mut()
            && let Some(pos) = first.find(" (")
        {
            first.truncate(pos);
        }
        let mut kept = Vec::with_capacity(self.rows.len());
        for row in self.rows.drain(..) {
            let pass = sealdb_expression::with_collation_ci(true, || {
                sealdb_expression::eval_filter(expr, &schema, &row)
            })?;
            if pass {
                kept.push(row);
            }
        }
        self.rows = kept;
        Ok(())
    }
}

impl Executor for ShowExec {
    fn open(&mut self) -> Result<()> {
        let is = self.domain.infoschema();
        match self.stmt.kind {
            ShowStmtKind::Databases => {
                self.schema = vec![show_column_header("Database", self.stmt.pattern.as_deref())];
                // Non-privileged users only see databases they hold a grant on
                // (TiDB `ShowDatabasesExec`); `INFORMATION_SCHEMA` is always
                // visible and listed first.
                let visible = sealdb_privilege::thread_user()
                    .and_then(|user| self.domain.privilege().visible_databases(&user, &[]));
                let mut names: Vec<String> = is
                    .all_schemas()
                    .into_iter()
                    .map(|db| db.name.o().to_string())
                    .collect();
                if !names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case("information_schema"))
                {
                    names.insert(0, "INFORMATION_SCHEMA".to_string());
                }
                self.rows = names
                    .into_iter()
                    .filter(|name| show_like_ok(name, self.stmt.pattern.as_deref()))
                    .filter(|name| match &visible {
                        None => true,
                        Some(dbs) => {
                            name.eq_ignore_ascii_case("information_schema")
                                || dbs.contains(&name.to_ascii_lowercase())
                        }
                    })
                    .map(|name| vec![Datum::Bytes(name.into_bytes())])
                    .collect();
            }
            ShowStmtKind::Tables => {
                let db = self
                    .stmt
                    .db_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.current_db.clone());
                if db.is_empty() {
                    return Err(Error::new(ERR_NO_DB, "No database selected"));
                }
                if is.schema_by_name(&db).is_none() {
                    return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{db}'")));
                }
                self.schema = if self.stmt.full {
                    vec![
                        show_column_header(
                            &format!("Tables_in_{db}"),
                            self.stmt.pattern.as_deref(),
                        ),
                        "Table_type".to_string(),
                    ]
                } else {
                    vec![show_column_header(
                        &format!("Tables_in_{db}"),
                        self.stmt.pattern.as_deref(),
                    )]
                };
                self.rows = is
                    .schema_tables(&db)
                    .into_iter()
                    .filter(|t| show_like_ok(t.name.o(), self.stmt.pattern.as_deref()))
                    .map(|t| {
                        let name = t.name.o().to_string();
                        if self.stmt.full {
                            let table_type = if t.is_view() { "VIEW" } else { "BASE TABLE" };
                            vec![
                                Datum::Bytes(name.into_bytes()),
                                Datum::Bytes(table_type.as_bytes().to_vec()),
                            ]
                        } else {
                            vec![Datum::Bytes(name.into_bytes())]
                        }
                    })
                    .collect();
                self.apply_where_filter()?;
            }
            ShowStmtKind::Grants => {
                // `SHOW GRANTS` / `FOR CURRENT_USER()` display the caller plus
                // their active roles; an explicit `FOR 'u'@'h'` ignores roles
                // (TiDB `ShowGrants` semantics).
                let (user, roles) = match self.stmt.user.clone() {
                    Some(u) if !u.username.is_empty() => (u, Vec::new()),
                    _ => (
                        sealdb_privilege::thread_user()
                            .unwrap_or_else(sealdb_parser::ast::UserIdentity::root),
                        sealdb_privilege::thread_active_roles(),
                    ),
                };
                // `USING role_list` overrides the active roles, but each role
                // must be granted to the displayed account (error 3530).
                let roles = if self.stmt.using_roles.is_empty() {
                    roles
                } else {
                    let mut using = Vec::with_capacity(self.stmt.using_roles.len());
                    for role in &self.stmt.using_roles {
                        let role = sealdb_parser::ast::UserIdentity::new(
                            role.username.clone(),
                            if role.hostname.is_empty() {
                                "%".to_string()
                            } else {
                                role.hostname.clone()
                            },
                        );
                        if !self.domain.privilege().role_granted_to(&user, &role) {
                            return Err(Error::new(
                                sealdb_errno::ERR_ROLE_NOT_GRANTED,
                                format!(
                                    "`{}`@`{}` is not granted to {}@{}",
                                    role.username, role.hostname, user.username, user.hostname
                                ),
                            ));
                        }
                        using.push(role);
                    }
                    using
                };
                // TiDB names the column `Grants for User` when no account is
                // given (it does not know the caller yet at plan time).
                self.schema = vec![match self.stmt.user.as_ref() {
                    None => "Grants for User".to_string(),
                    Some(_) => format!("Grants for {}", user.display()),
                }];
                self.rows = self
                    .domain
                    .privilege()
                    .show_grants(&user, &roles)
                    .into_iter()
                    .map(|g| vec![Datum::Bytes(g.into_bytes())])
                    .collect();
            }
            ShowStmtKind::CreateDatabase => {
                let db = self
                    .stmt
                    .db_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.current_db.clone());
                if db.is_empty() {
                    return Err(Error::new(ERR_NO_DB, "No database selected"));
                }
                if is.schema_by_name(&db).is_none() {
                    return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{db}'")));
                }
                let info = is.schema_by_name(&db).expect("checked above");
                self.schema = vec!["Database".to_string(), "Create Database".to_string()];
                let create = format!(
                    "CREATE DATABASE `{db}` DEFAULT CHARACTER SET {} COLLATE {}",
                    info.charset, info.collate
                );
                self.rows = vec![vec![
                    Datum::Bytes(db.as_bytes().to_vec()),
                    Datum::Bytes(create.into_bytes()),
                ]];
            }
            ShowStmtKind::CreateTable => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                if info.is_sequence() {
                    self.schema = vec!["Sequence".to_string(), "Create Sequence".to_string()];
                    self.rows = vec![vec![
                        Datum::Bytes(name.as_bytes().to_vec()),
                        Datum::Bytes(render_create_sequence(info).into_bytes()),
                    ]];
                } else if info.is_view() {
                    let view = info.view.as_ref().expect("view body");
                    self.schema = vec![
                        "View".to_string(),
                        "Create View".to_string(),
                        "character_set_client".to_string(),
                        "collation_connection".to_string(),
                    ];
                    self.rows = vec![vec![
                        Datum::Bytes(name.as_bytes().to_vec()),
                        Datum::Bytes(render_create_view(info, view).into_bytes()),
                        Datum::Bytes(b"utf8mb4".to_vec()),
                        Datum::Bytes(b"utf8mb4_general_ci".to_vec()),
                    ]];
                } else {
                    self.schema = vec!["Table".to_string(), "Create Table".to_string()];
                    // TiDB keeps the allocator high-water mark in the table
                    // meta; SHOW CREATE prints it as `AUTO_INCREMENT=<next>`.
                    let next_auto_id = if info.columns.iter().any(|c| c.auto_increment) {
                        Some(self.domain.peek_auto_id(info.id))
                    } else {
                        None
                    };
                    self.rows = vec![vec![
                        Datum::Bytes(name.as_bytes().to_vec()),
                        Datum::Bytes(render_create_table(info, next_auto_id).into_bytes()),
                    ]];
                }
            }
            ShowStmtKind::CreateView => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing view name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                let view = info.view.as_ref().ok_or_else(|| {
                    Error::new(
                        sealdb_errno::ERR_WRONG_OBJECT,
                        format!("'{schema}.{name}' is not VIEW"),
                    )
                })?;
                self.schema = vec![
                    "View".to_string(),
                    "Create View".to_string(),
                    "character_set_client".to_string(),
                    "collation_connection".to_string(),
                ];
                self.rows = vec![vec![
                    Datum::Bytes(name.as_bytes().to_vec()),
                    Datum::Bytes(render_create_view(info, view).into_bytes()),
                    Datum::Bytes(b"utf8mb4".to_vec()),
                    Datum::Bytes(b"utf8mb4_general_ci".to_vec()),
                ]];
            }
            ShowStmtKind::CreateSequence => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing sequence name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                // TiDB resolves `SHOW CREATE SEQUENCE` against the permanent
                // catalog (local temporary tables never shadow a sequence),
                // but takes the result column headers from the session catalog:
                // a temporary table with the same name reports
                // `Table` / `Create Table` even though the sequence DDL is
                // printed.
                let shadowed = is.temp_table_by_name(&name).is_some();
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                if !info.is_sequence() {
                    return Err(Error::new(
                        sealdb_errno::ERR_WRONG_OBJECT,
                        format!("'{schema}.{name}' is not SEQUENCE"),
                    ));
                }
                self.schema = if shadowed {
                    vec!["Table".to_string(), "Create Table".to_string()]
                } else {
                    vec!["Sequence".to_string(), "Create Sequence".to_string()]
                };
                self.rows = vec![vec![
                    Datum::Bytes(name.as_bytes().to_vec()),
                    Datum::Bytes(render_create_sequence(info).into_bytes()),
                ]];
            }
            ShowStmtKind::Columns => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                let full = self.stmt.full;
                self.schema = if full {
                    vec![
                        "Field".to_string(),
                        "Type".to_string(),
                        "Collation".to_string(),
                        "Null".to_string(),
                        "Key".to_string(),
                        "Default".to_string(),
                        "Extra".to_string(),
                        "Privileges".to_string(),
                        "Comment".to_string(),
                    ]
                } else {
                    vec![
                        "Field".to_string(),
                        "Type".to_string(),
                        "Null".to_string(),
                        "Key".to_string(),
                        "Default".to_string(),
                        "Extra".to_string(),
                    ]
                };
                let column = self.stmt.column.as_deref();
                let pattern = self.stmt.pattern.as_deref();
                // TiDB `fetchShowColumns`: a table-level
                // INSERT/SELECT/UPDATE/REFERENCES grant shows every column;
                // otherwise only columns covered by a column grant are listed,
                // and with none visible the statement fails with 1142 SELECT.
                let mgr = self.domain.privilege();
                let user = sealdb_privilege::thread_user();
                let roles = sealdb_privilege::thread_active_roles();
                let privs = PrivilegeType::INSERT
                    .union(PrivilegeType::SELECT)
                    .union(PrivilegeType::UPDATE)
                    .union(PrivilegeType::REFERENCES);
                let pass_tbl = match &user {
                    Some(user) => {
                        mgr.request_verification_with_roles(user, &roles, &schema, &name, privs)
                    }
                    None => true,
                };
                let mut pass_col = false;
                let mut rows = Vec::new();
                for col in info.columns.iter().filter(|col| !col.hidden) {
                    if !show_column_visible(col.name.o(), column, pattern) {
                        continue;
                    }
                    if !pass_tbl {
                        let allowed = match &user {
                            Some(user) => mgr.request_column_verification_with_roles(
                                user,
                                &roles,
                                &schema,
                                &name,
                                col.name.o(),
                                privs,
                            ),
                            None => true,
                        };
                        if !allowed {
                            continue;
                        }
                        pass_col = true;
                    }
                    rows.push(column_row(info, col, full));
                }
                if !pass_tbl && !pass_col {
                    return Err(sealdb_privilege::Manager::access_denied(
                        &schema,
                        &name,
                        PrivilegeType::SELECT,
                        user.as_ref().expect("checked above"),
                    ));
                }
                self.rows = rows;
                self.apply_where_filter()?;
            }
            ShowStmtKind::Index => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                self.schema = vec![
                    "Table".to_string(),
                    "Non_unique".to_string(),
                    "Key_name".to_string(),
                    "Seq_in_index".to_string(),
                    "Column_name".to_string(),
                    "Collation".to_string(),
                    "Cardinality".to_string(),
                    "Sub_part".to_string(),
                    "Packed".to_string(),
                    "Null".to_string(),
                    "Index_type".to_string(),
                    "Comment".to_string(),
                    "Index_comment".to_string(),
                ];
                self.rows = info
                    .indices
                    .iter()
                    .flat_map(|idx| {
                        idx.columns.iter().enumerate().map(|(seq, col)| {
                            vec![
                                Datum::Bytes(info.name.o().as_bytes().to_vec()),
                                Datum::Bytes(if idx.unique || idx.primary {
                                    b"0".to_vec()
                                } else {
                                    b"1".to_vec()
                                }),
                                Datum::Bytes(idx.name.o().as_bytes().to_vec()),
                                Datum::I64((seq + 1) as i64),
                                Datum::Bytes(col.name.o().as_bytes().to_vec()),
                                Datum::Bytes(b"A".to_vec()),
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Bytes(b"YES".to_vec()),
                                Datum::Bytes(b"BTREE".to_vec()),
                                Datum::Bytes(Vec::new()),
                                Datum::Bytes(Vec::new()),
                            ]
                        })
                    })
                    .collect();
            }
            ShowStmtKind::Charset => {
                let (_, rows) = sealdb_infoschema::tables::virtual_rows("character_sets", &is)?;
                self.schema = vec![
                    "Charset".to_string(),
                    "Description".to_string(),
                    "Default collation".to_string(),
                    "Maxlen".to_string(),
                ];
                self.rows = rows
                    .into_iter()
                    .map(|row| {
                        vec![
                            row[0].clone(),
                            row[2].clone(),
                            row[1].clone(),
                            row[3].clone(),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::Collation => {
                let (_, rows) = sealdb_infoschema::tables::virtual_rows("collations", &is)?;
                self.schema = vec![
                    "Collation".to_string(),
                    "Charset".to_string(),
                    "Id".to_string(),
                    "Default".to_string(),
                    "Compiled".to_string(),
                    "Sortlen".to_string(),
                ];
                self.rows = rows
                    .into_iter()
                    .map(|row| row.into_iter().take(6).collect())
                    .collect();
            }
            ShowStmtKind::DdlJobs => {
                self.schema = vec![
                    "JOB_ID".to_string(),
                    "DB_NAME".to_string(),
                    "TABLE_NAME".to_string(),
                    "JOB_TYPE".to_string(),
                    "STATE".to_string(),
                    "SCHEMA_STATE".to_string(),
                ];
                self.rows = self
                    .domain
                    .ddl_jobs()
                    .into_iter()
                    .map(|job| {
                        vec![
                            Datum::I64(job.id),
                            Datum::Bytes(job.schema_name.into_bytes()),
                            Datum::Bytes(job.table_name.into_bytes()),
                            Datum::Bytes(job.tp.into_bytes()),
                            Datum::Bytes(job.state.as_str().as_bytes().to_vec()),
                            Datum::Bytes(job.schema_state.as_str().as_bytes().to_vec()),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::Bindings => {
                self.schema = [
                    "Original_sql",
                    "Bind_sql",
                    "Default_db",
                    "Status",
                    "Create_time",
                    "Update_time",
                    "Charset",
                    "Collation",
                    "Source",
                    "Sql_digest",
                    "Plan_digest",
                ]
                .iter()
                .map(|column| column.to_string())
                .collect();
                self.rows = self
                    .domain
                    .bind_handle()
                    .all_bindings()
                    .into_iter()
                    .filter(|b| !b.global && b.status != "deleted")
                    .map(|b| {
                        vec![
                            Datum::Bytes(b.normalized_sql.into_bytes()),
                            Datum::Bytes(b.display_bind_sql.into_bytes()),
                            Datum::Bytes(b.db.into_bytes()),
                            Datum::Bytes(b.status.into_bytes()),
                            Datum::Bytes(b.create_time.into_bytes()),
                            Datum::Bytes(b.update_time.into_bytes()),
                            Datum::Bytes(b.charset.into_bytes()),
                            Datum::Bytes(b.collation.into_bytes()),
                            Datum::Bytes(b.source.into_bytes()),
                            Datum::Bytes(b.sql_digest.into_bytes()),
                            Datum::Bytes(b.plan_digest.into_bytes()),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::StatsLocked => {
                self.schema = vec![
                    "Db_name".to_string(),
                    "Table_name".to_string(),
                    "Partition_name".to_string(),
                    "Status".to_string(),
                ];
                let stats = self.domain.stats();
                let locked: std::collections::HashSet<i64> =
                    stats.locked_ids().into_iter().collect();
                // TiDB reports the locked table / partition ids in table-id
                // order; a partitioned table locked as a whole shows up as
                // `global` under dynamic partition pruning.
                let mut rows: Vec<(i64, String, String, String)> = Vec::new();
                for db in is.all_schemas() {
                    let db_name = db.name.o().to_string();
                    if db_name.eq_ignore_ascii_case("information_schema")
                        || db_name.eq_ignore_ascii_case("mysql")
                    {
                        continue;
                    }
                    for table in is.schema_tables(&db_name) {
                        let table_name = table.name.o().to_string();
                        match table.partition.as_ref() {
                            Some(partition) => {
                                if locked.contains(&table.id) {
                                    rows.push((
                                        table.id,
                                        db_name.clone(),
                                        table_name.clone(),
                                        "global".to_string(),
                                    ));
                                }
                                for definition in &partition.definitions {
                                    if locked.contains(&definition.id) {
                                        rows.push((
                                            definition.id,
                                            db_name.clone(),
                                            table_name.clone(),
                                            definition.name.o().to_string(),
                                        ));
                                    }
                                }
                            }
                            None => {
                                if locked.contains(&table.id) {
                                    rows.push((
                                        table.id,
                                        db_name.clone(),
                                        table_name,
                                        String::new(),
                                    ));
                                }
                            }
                        }
                    }
                }
                rows.sort_by_key(|(id, _, _, _)| *id);
                self.rows = rows
                    .into_iter()
                    .map(|(_, db, table, partition)| {
                        vec![
                            Datum::Bytes(db.into_bytes()),
                            Datum::Bytes(table.into_bytes()),
                            Datum::Bytes(partition.into_bytes()),
                            Datum::Bytes(b"locked".to_vec()),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::StatsTopN => {
                self.schema = vec![
                    "Db_name".to_string(),
                    "Table_name".to_string(),
                    "Partition_name".to_string(),
                    "Column_name".to_string(),
                    "Is_index".to_string(),
                    "Value".to_string(),
                    "Count".to_string(),
                ];
                let stats = self.domain.stats();
                let mut rows = Vec::new();
                for db in is.all_schemas() {
                    let db_name = db.name.o().to_string();
                    if db_name.eq_ignore_ascii_case("information_schema")
                        || db_name.eq_ignore_ascii_case("mysql")
                    {
                        continue;
                    }
                    for table in is.schema_tables(&db_name) {
                        let table_name = table.name.o().to_string();
                        let Some(table_stats) = stats.raw_table(&db_name, &table_name) else {
                            continue;
                        };
                        for (column_name, column) in &table_stats.columns {
                            let Some(topn) = column.topn.as_ref() else {
                                continue;
                            };
                            for (value, count) in &topn.items {
                                rows.push(vec![
                                    Datum::Bytes(db_name.as_bytes().to_vec()),
                                    Datum::Bytes(table_name.as_bytes().to_vec()),
                                    Datum::Bytes(Vec::new()),
                                    Datum::Bytes(column_name.as_bytes().to_vec()),
                                    Datum::I64(0),
                                    Datum::Bytes(value.as_bytes().to_vec()),
                                    Datum::I64(*count as i64),
                                ]);
                            }
                        }
                        for (index_name, index) in &table_stats.indices {
                            for (value, count) in &index.topn {
                                rows.push(vec![
                                    Datum::Bytes(db_name.as_bytes().to_vec()),
                                    Datum::Bytes(table_name.as_bytes().to_vec()),
                                    Datum::Bytes(Vec::new()),
                                    Datum::Bytes(index_name.as_bytes().to_vec()),
                                    Datum::I64(1),
                                    Datum::Bytes(value.clone()),
                                    Datum::I64(*count as i64),
                                ]);
                            }
                        }
                    }
                }
                self.rows = rows;
                self.apply_where_filter()?;
            }
            ShowStmtKind::StatsHistograms => {
                self.schema = STATS_HISTOGRAMS_COLUMNS
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect();
                let stats = self.domain.stats();
                let mut rows = Vec::new();
                for db in is.all_schemas() {
                    let db_name = db.name.o().to_string();
                    if db_name.eq_ignore_ascii_case("information_schema")
                        || db_name.eq_ignore_ascii_case("mysql")
                    {
                        continue;
                    }
                    for table in is.schema_tables(&db_name) {
                        let table_name = table.name.o().to_string();
                        let Some(table_stats) = stats.raw_table(&db_name, &table_name) else {
                            continue;
                        };
                        match table.partition.as_ref().filter(|part| part.enable) {
                            // A partitioned table reports its global statistics
                            // under the pseudo partition `global`, then one
                            // block per partition definition.
                            Some(part) => {
                                push_stats_histogram_rows(
                                    &mut rows,
                                    &db_name,
                                    &table_name,
                                    "global",
                                    &table,
                                    &table_stats.columns,
                                    &table_stats.indices,
                                    true,
                                );
                                for def in &part.definitions {
                                    let partition_name = def.name.o().to_string();
                                    let partition_stats = table_stats
                                        .partitions
                                        .iter()
                                        .find(|stats| stats.id == def.id);
                                    let empty_columns = Vec::new();
                                    let empty_indices = Vec::new();
                                    push_stats_histogram_rows(
                                        &mut rows,
                                        &db_name,
                                        &table_name,
                                        &partition_name,
                                        &table,
                                        partition_stats
                                            .map(|stats| &stats.columns)
                                            .unwrap_or(&empty_columns),
                                        partition_stats
                                            .map(|stats| &stats.indices)
                                            .unwrap_or(&empty_indices),
                                        // A special global index has no
                                        // per-partition statistics record.
                                        false,
                                    );
                                }
                            }
                            None => push_stats_histogram_rows(
                                &mut rows,
                                &db_name,
                                &table_name,
                                "",
                                &table,
                                &table_stats.columns,
                                &table_stats.indices,
                                true,
                            ),
                        }
                    }
                }
                self.rows = rows;
                self.apply_where_filter()?;
            }
            ShowStmtKind::Warnings => {
                self.schema = vec![
                    "Level".to_string(),
                    "Code".to_string(),
                    "Message".to_string(),
                ];
                self.rows = Vec::new();
            }
            // Variable values live in the session; `Session::execute_one`
            // materializes `SHOW VARIABLES` before planning. Keep the match
            // exhaustive with the same column layout.
            ShowStmtKind::Variables => {
                self.schema = vec!["Variable_name".to_string(), "Value".to_string()];
                self.rows = Vec::new();
            }
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn column_row(info: &TableInfo, col: &ColumnInfo, full: bool) -> Vec<Datum> {
    // MySQL NotNullFlag — `DESCRIBE` reports `NO` for columns that reject NULL.
    const NOT_NULL_FLAG: u32 = 1;
    let type_name = col.field_type.type_name();
    let mut row = vec![
        Datum::Bytes(col.name.o().as_bytes().to_vec()),
        Datum::Bytes(type_name.clone().into_bytes()),
    ];
    if full {
        let collation = if type_name.starts_with("varchar") || type_name.starts_with("char") {
            Datum::Bytes(b"utf8mb4_bin".to_vec())
        } else {
            Datum::Null
        };
        row.push(collation);
    }
    let default = match col.default_expr.as_deref() {
        Some(text) => Datum::Bytes(default_display(text).into_bytes()),
        None => Datum::Null,
    };
    // `PRI` for a PRIMARY KEY member, else `UNI` / `MUL` for the first column
    // of a unique / non-unique secondary index (MySQL `Field::Key`).
    let in_index =
        |idx: &sealdb_meta::IndexInfo| idx.columns.first().is_some_and(|c| c.offset == col.offset);
    let key = if info.indices.iter().any(|idx| idx.primary && in_index(idx)) {
        "PRI"
    } else if info.indices.iter().any(|idx| idx.unique && in_index(idx)) {
        "UNI"
    } else if info.indices.iter().any(|idx| !idx.primary && in_index(idx)) {
        "MUL"
    } else {
        ""
    };
    let null = if col.field_type.flag & NOT_NULL_FLAG != 0 {
        "NO"
    } else {
        "YES"
    };
    // MySQL `Field::Extra`: generated columns are flagged and may not be written.
    let extra = match col.generated_expr {
        Some(_) if col.generated_stored => "STORED GENERATED",
        Some(_) => "VIRTUAL GENERATED",
        None => "",
    };
    row.extend([
        Datum::Bytes(null.as_bytes().to_vec()),
        Datum::Bytes(key.as_bytes().to_vec()),
        default,
        Datum::Bytes(extra.as_bytes().to_vec()),
    ]);
    if full {
        row.push(Datum::Bytes(b"select,insert,update,references".to_vec()));
        row.push(Datum::Bytes(Vec::new()));
    }
    row
}

/// DESCRIBE `Default` cell: string literals lose their quotes.
fn default_display(text: &str) -> String {
    let t = text.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    t.to_string()
}

fn render_create_sequence(info: &TableInfo) -> String {
    let seq = info.sequence.as_ref().expect("sequence info");
    let mut out = format!(
        "CREATE SEQUENCE `{}` start with {} minvalue {} maxvalue {} increment by {} ",
        info.name.o(),
        seq.start,
        seq.min_value,
        seq.max_value,
        seq.increment
    );
    if seq.cache {
        out.push_str(&format!("cache {} ", seq.cache_value));
    } else {
        out.push_str("nocache ");
    }
    out.push_str(if seq.cycle { "cycle " } else { "nocycle " });
    out.push_str("ENGINE=InnoDB");
    if !seq.comment.is_empty() {
        out.push_str(&format!(" COMMENT='{}'", seq.comment.replace('\'', "''")));
    }
    out
}

fn render_create_table(info: &TableInfo, next_auto_id: Option<i64>) -> String {
    // MySQL NotNullFlag — omit DEFAULT NULL when set.
    const NOT_NULL_FLAG: u32 = 1;
    let mut cols: Vec<String> = info
        .columns
        .iter()
        .filter(|col| !col.hidden)
        .map(|col| {
            let mut line = format!("  `{}` {}", col.name.o(), col.field_type.type_name());
            // TiDB prints a column's charset/collation only when it differs from
            // the table default. An equal collation still prints when it is not
            // the charset's default collation (TiDB `GetDefaultCollation`).
            let ft = &col.field_type;
            if ft.is_character_type() && ft.charset != "binary" {
                if !ft.charset.is_empty() && !ft.charset.eq_ignore_ascii_case(&info.charset) {
                    line.push_str(&format!(" CHARACTER SET {}", ft.charset));
                    if !ft.collate.is_empty() {
                        line.push_str(&format!(" COLLATE {}", ft.collate));
                    }
                } else if !ft.collate.is_empty() {
                    let charset_default =
                        sealdb_parser::charset::Collation::default_for_charset(&ft.charset).name;
                    if !ft.collate.eq_ignore_ascii_case(&info.collate)
                        || !ft.collate.eq_ignore_ascii_case(&charset_default)
                    {
                        line.push_str(&format!(" COLLATE {}", ft.collate));
                    }
                }
            }
            // TiDB prints the generation expression in place of DEFAULT.
            if let Some(expr) = &col.generated_expr {
                line.push_str(&format!(
                    " GENERATED ALWAYS AS ({expr}) {}",
                    if col.generated_stored {
                        "STORED"
                    } else {
                        "VIRTUAL"
                    }
                ));
                return line;
            }
            if col.field_type.flag & NOT_NULL_FLAG != 0 {
                line.push_str(" NOT NULL");
            }
            if col.auto_increment {
                line.push_str(" AUTO_INCREMENT");
            }
            if let Some(default) = &col.default_expr {
                // A NOT NULL column has no implicit `DEFAULT NULL` to print.
                if !(default.eq_ignore_ascii_case("null")
                    && col.field_type.flag & NOT_NULL_FLAG != 0)
                {
                    line.push_str(&format!(" DEFAULT {default}"));
                }
            } else if col.field_type.flag & NOT_NULL_FLAG == 0 {
                // TiDB spells the implicit NULL of a timestamp column out only
                // when it also has no default (`timestamp NULL DEFAULT NULL`).
                if col.field_type.tp == sealdb_types::field_type::TYPE_TIMESTAMP {
                    line.push_str(" NULL");
                }
                line.push_str(" DEFAULT NULL");
            }
            if let Some(comment) = &col.comment {
                line.push_str(&format!(" COMMENT '{}'", comment.replace('\'', "''")));
            }
            line
        })
        .collect();
    // TiDB emits the PRIMARY KEY at its storage position: a clustered primary
    // key is index 0 and comes first, a NONCLUSTERED one is a secondary index
    // created after the declared keys and therefore prints last.
    let mut key_lines: Vec<String> = Vec::new();
    let mut primary_line: Option<String> = None;
    for idx in &info.indices {
        let key = if idx.primary {
            "PRIMARY KEY"
        } else if idx.unique {
            "UNIQUE KEY"
        } else {
            "KEY"
        };
        let parts = idx
            .columns
            .iter()
            .map(|c| {
                if let Some(col) = info
                    .columns
                    .iter()
                    .find(|col| col.hidden && col.name.o().eq_ignore_ascii_case(c.name.o()))
                {
                    // Expression key part: TiDB prints the generating expression
                    // wrapped in an extra paren pair.
                    return format!("({})", col.generated_expr.clone().unwrap_or_default());
                }
                // A prefix index prints its length right after the name
                // (`` `a`(10) ``), with no space before the paren. TiDB only
                // keeps the length while it still covers less than the whole
                // string column (`modify a int` / `char(9)` drop it).
                let prefix = info
                    .columns
                    .iter()
                    .find(|col| col.name.o().eq_ignore_ascii_case(c.name.o()))
                    .filter(|col| {
                        matches!(
                            col.field_type.tp,
                            sealdb_types::field_type::TYPE_STRING
                                | sealdb_types::field_type::TYPE_VARCHAR
                                | sealdb_types::field_type::TYPE_VAR_STRING
                        ) && c.length > 0
                            && col.field_type.flen > c.length
                    });
                match prefix {
                    Some(_) => format!("`{}`({})", c.name.o(), c.length),
                    None => format!("`{}`", c.name.o()),
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        if idx.primary {
            // TiDB prints the PRIMARY KEY without an index name and annotates
            // how the key is stored (`/*T![clustered_index] … */`).
            let stored = if info.pk_is_clustered() {
                "CLUSTERED"
            } else {
                "NONCLUSTERED"
            };
            primary_line = Some(format!(
                "  PRIMARY KEY ({parts}) /*T![clustered_index] {stored} */"
            ));
            continue;
        }
        let predicate = idx
            .where_clause
            .as_ref()
            .map(|text| format!(" WHERE {text}"))
            .unwrap_or_default();
        // TiDB keeps the GLOBAL option behind a version comment so MySQL
        // clients ignore it.
        let global = if idx.global {
            " /*T![global_index] GLOBAL */"
        } else {
            ""
        };
        key_lines.push(format!(
            "  {key} `{}` ({parts}){global}{predicate}",
            idx.name.o()
        ));
    }
    if info.pk_is_clustered() {
        if let Some(line) = primary_line.take() {
            cols.push(line);
        }
        cols.extend(key_lines);
    } else {
        cols.extend(key_lines);
        if let Some(line) = primary_line.take() {
            cols.push(line);
        }
    }
    // FOREIGN KEY constraints follow the keys (`CONSTRAINT `fk` FOREIGN KEY …`).
    for fk in &info.foreign_keys {
        let local = fk
            .cols
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(",");
        let referenced = fk
            .ref_cols
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(",");
        cols.push(format!(
            "  CONSTRAINT `{}` FOREIGN KEY ({local}) REFERENCES `{}` ({referenced})",
            fk.name, fk.ref_table
        ));
    }
    let mut out = format!(
        "CREATE {}TABLE `{}` (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET={} COLLATE={}",
        if info.temporary {
            if info.on_commit_delete_rows {
                "GLOBAL TEMPORARY "
            } else {
                "TEMPORARY "
            }
        } else {
            ""
        },
        info.name.o(),
        cols.join(",\n"),
        info.charset,
        info.collate
    );
    // TiDB prints `AUTO_INCREMENT=<next id>` once the allocator handed out at
    // least one value (a fresh table omits the option).
    if let Some(last) = next_auto_id
        && last > 0
    {
        out.push_str(&format!(" AUTO_INCREMENT={}", last + 1));
    }
    if let Some(cache) = info.auto_id_cache {
        out.push_str(&format!(" /*T![auto_id_cache] AUTO_ID_CACHE={cache} */"));
    }
    if info.temporary && info.on_commit_delete_rows {
        out.push_str(" ON COMMIT DELETE ROWS");
    }
    // TiDB echoes the `AFFINITY` option as a TiDB-only comment directly after
    // the table options and before `PARTITION BY`.
    if info.has_affinity() {
        out.push_str(&format!(" /*T![affinity] AFFINITY='{}' */", info.affinity));
    }
    // TiDB echoes a cached table as a trailing comment
    // (`… COLLATE=utf8mb4_bin /* CACHED ON */`).
    if info.cached_table {
        out.push_str(" /* CACHED ON */");
    }
    // TiDB echoes the shard options as a TiDB-only comment at the end.
    let mut shard_opts = String::new();
    if info.shard_row_id_bits > 0 {
        shard_opts.push_str(&format!("SHARD_ROW_ID_BITS={}", info.shard_row_id_bits));
    }
    if info.pre_split_regions > 0 {
        if !shard_opts.is_empty() {
            shard_opts.push(' ');
        }
        shard_opts.push_str(&format!("PRE_SPLIT_REGIONS={}", info.pre_split_regions));
    }
    if !shard_opts.is_empty() {
        out.push_str(&format!(" /*T! {shard_opts} */"));
    }
    if let Some(ttl) = &info.ttl {
        let enable = if ttl.enable { "ON" } else { "OFF" };
        out.push_str(&format!(
            " /*T![ttl] TTL={} */ /*T![ttl] TTL_ENABLE='{enable}' */ /*T![ttl] TTL_JOB_INTERVAL='{}' */",
            ttl.expr, ttl.job_interval
        ));
    }
    if let Some(policy) = &info.placement_policy {
        out.push_str(&format!(" /*T![placement] PLACEMENT POLICY=`{policy}` */"));
    }
    if let Some(part) = &info.partition {
        out.push('\n');
        out.push_str(&render_partition_clause(part));
    }
    // `ANSI_QUOTES`: TiDB quotes every identifier with `"` instead of a
    // backtick in `SHOW CREATE TABLE`.
    if sealdb_parser::ansi_quotes() {
        out = out.replace('`', "\"");
    }
    // TiDB keeps backticks around partition-expression function arguments even
    // under `ANSI_QUOTES` (`UNIX_TIMESTAMP(`c`)`); those were marked with a
    // sentinel so they survive the blanket identifier replacement.
    out = out.replace('\u{1}', "`");
    out
}

/// HASH/KEY/RANGE/LIST partition key text: a plain column is printed
/// backticked (`HASH (`a`)`), a function call renders its name upper-case with
/// backticked column arguments (`RANGE (YEAR(`hired`))`).
fn partition_key_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
    };
    if plain(trimmed) {
        return format!("`{trimmed}`");
    }
    // `KEY (c1, c2, …)`: a comma list of plain columns.
    let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
    if parts.len() > 1 && parts.iter().all(|p| plain(p)) {
        return parts
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(",");
    }
    if let Some((name, args)) = trimmed.split_once('(')
        && trimmed.ends_with(')')
        && plain(name)
    {
        let inner = &args[..args.len() - 1];
        let rendered = if inner.split(',').all(|a| plain(a.trim())) {
            inner
                .split(',')
                .map(|a| format!("\u{1}{}\u{1}", a.trim()))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            inner.to_string()
        };
        return format!("{}({rendered})", name.to_uppercase());
    }
    trimmed.to_string()
}

fn render_partition_clause(part: &sealdb_meta::PartitionInfo) -> String {
    use sealdb_meta::PartitionType;
    let key = if part.range_columns {
        let cols = part
            .columns
            .iter()
            .map(|c| format!("`{}`", c.o()))
            .collect::<Vec<_>>()
            .join(",");
        let keyword = if part.tp == PartitionType::List {
            "LIST COLUMNS"
        } else {
            "RANGE COLUMNS"
        };
        format!("{keyword}({cols})")
    } else {
        match part.tp {
            PartitionType::Range => format!("RANGE ({})", partition_key_expr(&part.expr)),
            PartitionType::Hash => format!("HASH ({})", partition_key_expr(&part.expr)),
            PartitionType::List => format!("LIST ({})", partition_key_expr(&part.expr)),
            PartitionType::Key => format!("KEY ({})", partition_key_expr(&part.expr)),
        }
    };
    if part.uniform && matches!(part.tp, PartitionType::Hash | PartitionType::Key) {
        return format!(
            "PARTITION BY {key} PARTITIONS {}",
            part.num.max(part.definitions.len() as u64)
        );
    }
    let mut defs = Vec::new();
    for def in &part.definitions {
        let mut piece = format!("PARTITION `{}`", def.name.o());
        if matches!(part.tp, PartitionType::Hash | PartitionType::Key)
            && def.less_than.is_empty()
            && def.in_values.is_empty()
            && !def.list_default
        {
            // HASH/KEY named partitions have no VALUES clause.
        } else if def.list_default
            && def.in_values.len() == 1
            && matches!(def.in_values[0], sealdb_meta::ListInEntry::Default)
        {
            piece.push_str(" DEFAULT");
        } else if !def.in_values.is_empty() {
            piece.push_str(&format!(
                " VALUES IN ({})",
                format_list_in_values(&def.in_values, part.range_columns)
            ));
        } else {
            let lt = def.less_than.to_vec().join(",");
            piece.push_str(&format!(" VALUES LESS THAN ({lt})"));
        }
        if !def.comment.is_empty() {
            piece.push_str(&format!(" COMMENT '{}'", def.comment.replace('\'', "''")));
        }
        if let Some(policy) = &def.placement_policy {
            piece.push_str(&format!(" /*T![placement] PLACEMENT POLICY=`{policy}` */"));
        }
        defs.push(piece);
    }
    format!("PARTITION BY {key}\n({})", defs.join(",\n "))
}

fn format_list_in_values(entries: &[sealdb_meta::ListInEntry], list_columns: bool) -> String {
    entries
        .iter()
        .map(|e| format_list_in_entry(e, list_columns))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_list_in_entry(entry: &sealdb_meta::ListInEntry, list_columns: bool) -> String {
    match entry {
        sealdb_meta::ListInEntry::Default => "DEFAULT".to_string(),
        sealdb_meta::ListInEntry::Row(row) => {
            let multi = list_columns && row.len() > 1;
            let inner = row
                .iter()
                .map(|lit| match lit {
                    sealdb_meta::ListLiteral::Null => "NULL".to_string(),
                    sealdb_meta::ListLiteral::Number(n) => n.clone(),
                    sealdb_meta::ListLiteral::String(s) => {
                        format!("'{}'", sealdb_meta::escape_mysql_string(s))
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            if multi { format!("({inner})") } else { inner }
        }
    }
}

fn render_create_view(info: &TableInfo, view: &sealdb_meta::ViewInfo) -> String {
    // TiDB always prints the full output column list (explicit rename list or
    // the names derived from the view body).
    let cols = if info.columns.is_empty() {
        String::new()
    } else {
        let list = info
            .columns
            .iter()
            .map(|c| format!("`{}`", c.name.o()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" ({list})")
    };
    format!(
        "CREATE ALGORITHM={} DEFINER={} SQL SECURITY {} VIEW `{}`{} AS {}",
        view.algorithm,
        view.definer,
        view.security,
        info.name.o(),
        cols,
        view.select_stmt
    )
}

fn resolve_table(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}

/// TiDB SHOW LIKE header: `Tables_in_db (pat)` / `Database (pat)`.
fn show_column_header(base: &str, pattern: Option<&str>) -> String {
    match pattern {
        Some(pat) => format!("{base} ({pat})"),
        None => base.to_string(),
    }
}

/// TiDB `DESC tbl col` filters on the column name exactly (case-insensitive);
/// `SHOW COLUMNS … LIKE` matches the LIKE pattern instead.
fn show_column_visible(name: &str, column: Option<&str>, pattern: Option<&str>) -> bool {
    match column {
        Some(column) => name.eq_ignore_ascii_case(column),
        None => show_like_ok(name, pattern),
    }
}

/// TiDB `SHOW STATS_HISTOGRAMS` columns (`executor/show_stats.go`).
const STATS_HISTOGRAMS_COLUMNS: [&str; 15] = [
    "Db_name",
    "Table_name",
    "Partition_name",
    "Column_name",
    "Is_index",
    "Update_time",
    "Distinct_count",
    "Null_count",
    "Avg_col_size",
    "Correlation",
    "Load_status",
    "Total_mem_usage",
    "Hist_mem_usage",
    "Topn_mem_usage",
    "Cms_mem_usage",
];

/// One statistics block: the columns first, then every index. TiDB keeps a
/// plain index's statistics on its key column, so a missing (or placeholder)
/// index record is reported from the leading column's statistics.
fn push_stats_histogram_rows(
    rows: &mut Vec<Vec<Datum>>,
    db: &str,
    table: &str,
    partition: &str,
    info: &TableInfo,
    columns: &[(String, sealdb_statistics::ColumnStats)],
    indices: &[(String, sealdb_statistics::IndexStats)],
    include_special: bool,
) {
    for (name, column) in columns {
        let topn_mem = column
            .topn
            .as_ref()
            .map(|topn| {
                let eval = info
                    .columns
                    .iter()
                    .find(|col| col.name.o().eq_ignore_ascii_case(name))
                    .map(|col| col.field_type.eval_type());
                32 + topn
                    .items
                    .iter()
                    .map(|(value, _)| 32 + datum_encoded_size(value, eval))
                    .sum::<i64>()
            })
            .unwrap_or(0);
        rows.push(stats_histogram_row(
            db,
            table,
            partition,
            name,
            0,
            column.ndv,
            column.null_count,
            column.correlation,
            topn_mem,
        ));
    }
    for index in &info.indices {
        if index.state != sealdb_meta::STATE_PUBLIC {
            continue;
        }
        let special = is_special_global_index(info, index);
        if special && !include_special {
            continue;
        }
        // A real index histogram (an expression index) carries its own encoded
        // keys; TiDB's `TopN` memory is 32 bytes per entry plus the slice.
        if let Some((_, stats)) = indices
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(index.name.o()))
            .filter(|(_, stats)| !stats.placeholder)
        {
            let topn_mem = 32
                + stats
                    .topn
                    .iter()
                    .map(|(key, _)| 32 + key.len() as i64)
                    .sum::<i64>();
            rows.push(stats_histogram_row(
                db,
                table,
                partition,
                index.name.o(),
                1,
                stats.ndv,
                stats.null_count,
                0.0,
                topn_mem,
            ));
            continue;
        }
        // Fall back to the leading key column's statistics, exactly like a
        // planner estimate for a plain index.
        let Some(column) = index
            .columns
            .iter()
            .filter_map(|part| {
                info.columns
                    .iter()
                    .find(|col| !col.hidden && col.name.o().eq_ignore_ascii_case(part.name.o()))
                    .map(|col| (col, part.name.o()))
            })
            .find_map(|(col, name)| {
                columns
                    .iter()
                    .find(|(column_name, _)| column_name.eq_ignore_ascii_case(name))
                    .map(|(_, stats)| (col, stats))
            })
        else {
            continue;
        };
        let (column_info, column_stats) = column;
        let eval = column_info.field_type.eval_type();
        let topn_mem = column_stats
            .topn
            .as_ref()
            .map(|topn| {
                32 + topn
                    .items
                    .iter()
                    .map(|(value, _)| 32 + datum_encoded_size(value, Some(eval)))
                    .sum::<i64>()
            })
            .unwrap_or(0);
        rows.push(stats_histogram_row(
            db,
            table,
            partition,
            index.name.o(),
            1,
            column_stats.ndv,
            column_stats.null_count,
            0.0,
            topn_mem,
        ));
    }
}

/// TiDB `codec.EncodeValue` byte length of one TopN value: an integer or float
/// is a flag byte plus its 8-byte encoding; a string is a flag byte, a length
/// byte and the collation sort key, padded to a multiple of 8 bytes.
fn datum_encoded_size(value: &str, eval: Option<sealdb_types::field_type::EvalType>) -> i64 {
    match eval {
        Some(sealdb_types::field_type::EvalType::String) => {
            let padded = (value.len() as i64 + 7) / 8 * 8;
            2 + padded
        }
        _ => 9,
    }
}

fn stats_histogram_row(
    db: &str,
    table: &str,
    partition: &str,
    name: &str,
    is_index: i64,
    distinct: u64,
    null_count: u64,
    correlation: f64,
    topn_mem: i64,
) -> Vec<Datum> {
    // Empty histograms and sketches: SealDB keeps every distinct value in the
    // TopN (version 2 without a CMSketch), so those two parts stay at zero.
    let hist_mem = 0;
    let cms_mem = 0;
    vec![
        Datum::Bytes(db.as_bytes().to_vec()),
        Datum::Bytes(table.as_bytes().to_vec()),
        Datum::Bytes(partition.as_bytes().to_vec()),
        Datum::Bytes(name.as_bytes().to_vec()),
        Datum::I64(is_index),
        // `update_time` comes from the statistics timestamp; SealDB reports
        // NULL (test goldens replace the column).
        Datum::Null,
        Datum::I64(distinct as i64),
        Datum::I64(null_count as i64),
        // `avg_col_size` needs per-column byte totals; goldens replace it.
        Datum::F64(0.0),
        Datum::F64(correlation),
        Datum::Bytes(b"allLoaded".to_vec()),
        Datum::I64(hist_mem + topn_mem + cms_mem),
        Datum::I64(hist_mem),
        Datum::I64(topn_mem),
        Datum::I64(cms_mem),
    ]
}

fn show_like_ok(name: &str, pattern: Option<&str>) -> bool {
    match pattern {
        None => true,
        Some(pat) => show_like_match(name.as_bytes(), pat.as_bytes()),
    }
}

/// MySQL LIKE (`%` / `_`), case-insensitive for identifier SHOW filters.
/// Shared with `SHOW VARIABLES LIKE` (evaluated by the session layer).
pub fn show_like_match(text: &[u8], pattern: &[u8]) -> bool {
    show_like_at(text, 0, pattern, 0)
}

fn show_like_at(text: &[u8], ti: usize, pattern: &[u8], pi: usize) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }
    match pattern[pi] {
        b'%' => {
            show_like_at(text, ti, pattern, pi + 1)
                || (ti < text.len() && show_like_at(text, ti + 1, pattern, pi))
        }
        b'_' => ti < text.len() && show_like_at(text, ti + 1, pattern, pi + 1),
        // MySQL pattern escapes: `\%`, `\_` and `\\` stand for the character
        // itself (the string-literal layer keeps them intact for LIKE).
        b'\\' if pi + 1 < pattern.len() => {
            let ch = pattern[pi + 1];
            ti < text.len()
                && text[ti].eq_ignore_ascii_case(&ch)
                && show_like_at(text, ti + 1, pattern, pi + 2)
        }
        ch => {
            ti < text.len()
                && text[ti].eq_ignore_ascii_case(&ch)
                && show_like_at(text, ti + 1, pattern, pi + 1)
        }
    }
}

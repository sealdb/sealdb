use sealdb_config::Config;
use sealdb_domain::{Domain, SchemaSnap};
use sealdb_errno::{Error, Result};
use sealdb_executor::{DDLExec, ExecutorBuilder, QueryResult};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::IsolationLevel;
use sealdb_meta::{ClusteredIndexMode, with_clustered_index_mode};
use sealdb_parser::Parser;
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{
    CommonTableExpression, CteMaterialize, InsertStmt, IsolationLevel as AstIsolation,
    PrivilegeType, SelectStmt, SetVarScope, Statement, UserIdentity, WithClause,
};
use sealdb_parser::charset::Collation;
use sealdb_planner::{PartitionPruneMode, PhysicalPlan, PlanBuilder, normalize_prep_cache_key};
use sealdb_sessiontxn::TxnManager;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Session {
    parser: Parser,
    planner: PlanBuilder,
    domain: Arc<Domain>,
    /// Domain-local id for MDL holders.
    session_id: u64,
    current_db: Mutex<String>,
    current_user: Mutex<UserIdentity>,
    txn: Mutex<TxnManager>,
    /// Schema before the first DDL in the current explicit txn.
    schema_snap: Mutex<Option<SchemaSnap>>,
    isolation: Mutex<IsolationLevel>,
    read_only: Mutex<bool>,
    prepared: Mutex<HashMap<String, String>>,
    cache_plans: Mutex<bool>,
    /// When set, Domain plan-cache lookup/insert uses this key instead of
    /// restored bound SQL (prepared `?` template).
    plan_cache_key: Mutex<Option<String>>,
    collation: Mutex<Collation>,
    sql_mode: Mutex<String>,
    /// TiDB `time_zone` (default `SYSTEM`).
    time_zone: Mutex<String>,
    /// Session user variables (`SET @a = …` / `SELECT @a`).
    user_vars: Mutex<HashMap<String, sealdb_types::Datum>>,
    /// TiDB `tidb_partition_prune_mode` (default dynamic).
    partition_prune_mode: Mutex<PartitionPruneMode>,
    /// TiDB `tidb_enable_clustered_index` (default ON). Read by CREATE TABLE to
    /// pick the row handle for an unannotated PRIMARY KEY.
    clustered_index: Mutex<ClusteredIndexMode>,
    /// Active roles for the session (`SET ROLE`).
    active_roles: Mutex<Vec<UserIdentity>>,
    /// TiDB `tidb_enable_auto_analyze` (default ON).
    enable_auto_analyze: Mutex<bool>,
    /// TiDB `tidb_auto_analyze_ratio` (default 0.5).
    auto_analyze_ratio: Mutex<f64>,
    /// TiDB `tidb_window_concurrency` (default 5).
    window_concurrency: Mutex<u64>,
    /// TiDB `autocommit` (default ON).
    autocommit: Mutex<bool>,
    /// Last statement warnings for `SHOW WARNINGS`.
    last_warnings: Mutex<Vec<(i32, String)>>,
}

impl Session {
    pub fn new() -> Self {
        Self::with_domain(Arc::new(Domain::new()))
    }

    pub fn from_config(cfg: &Config) -> Result<Self> {
        let storage = sealdb_store::open(cfg)?;
        Ok(Self::with_domain(Arc::new(Domain::with_storage(storage)?)))
    }

    pub fn with_domain(domain: Arc<Domain>) -> Self {
        let globals = domain.global_vars();
        let session_id = domain.alloc_session_id();
        Self {
            parser: Parser::new(),
            planner: PlanBuilder::new(),
            domain,
            session_id,
            current_db: Mutex::new(String::new()),
            current_user: Mutex::new(UserIdentity::root()),
            txn: Mutex::new(TxnManager::new()),
            schema_snap: Mutex::new(None),
            isolation: Mutex::new(IsolationLevel::RepeatableRead),
            read_only: Mutex::new(false),
            prepared: Mutex::new(HashMap::new()),
            cache_plans: Mutex::new(false),
            plan_cache_key: Mutex::new(None),
            collation: Mutex::new(Collation::default_for_charset(
                &globals.character_set_connection,
            )),
            sql_mode: Mutex::new(globals.sql_mode),
            time_zone: Mutex::new("SYSTEM".to_string()),
            user_vars: Mutex::new(HashMap::new()),
            partition_prune_mode: Mutex::new(
                PartitionPruneMode::parse(&globals.partition_prune_mode)
                    .unwrap_or(PartitionPruneMode::Dynamic),
            ),
            clustered_index: Mutex::new(ClusteredIndexMode::On),
            active_roles: Mutex::new(Vec::new()),
            enable_auto_analyze: Mutex::new(globals.enable_auto_analyze),
            auto_analyze_ratio: Mutex::new(0.5),
            window_concurrency: Mutex::new(sealdb_expression::DEFAULT_WINDOW_CONCURRENCY),
            autocommit: Mutex::new(true),
            last_warnings: Mutex::new(Vec::new()),
        }
    }

    pub fn with_grant_table() -> Self {
        Self::with_domain(Arc::new(Domain::with_grant_table()))
    }

    pub fn set_current_user(&self, user: UserIdentity) {
        *self.current_user.lock().expect("session current_user lock") = user;
    }

    pub fn current_user(&self) -> UserIdentity {
        self.current_user
            .lock()
            .expect("session current_user lock")
            .clone()
    }

    /// Privilege manager for MySQL handshake auth.
    pub fn privilege_manager(&self) -> Arc<sealdb_privilege::Manager> {
        self.domain.privilege()
    }

    pub fn execute_stmt(&self, sql: &str) -> Result<QueryResult> {
        let mut results = self.execute_all(sql)?;
        Ok(results.pop().unwrap_or(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            warnings: Vec::new(),
            affected_rows: 0,
            info: String::new(),
        }))
    }

    /// Execute SQL with the session plan cache enabled (text EXECUTE / COM_STMT_EXECUTE).
    pub fn execute_cached_stmt(&self, sql: &str) -> Result<QueryResult> {
        self.execute_cached_stmt_with_key(sql, None)
    }

    /// Like [`Self::execute_cached_stmt`], optionally overriding the Domain plan-cache
    /// key with the unbound prepared template (keeps `?`).
    pub fn execute_cached_stmt_with_key(
        &self,
        sql: &str,
        prep_cache_key: Option<&str>,
    ) -> Result<QueryResult> {
        *self.cache_plans.lock().expect("session cache flag lock") = true;
        if let Some(key) = prep_cache_key {
            *self
                .plan_cache_key
                .lock()
                .expect("session plan cache key lock") = Some(normalize_prep_cache_key(key));
        }
        let result = self.execute_stmt(sql);
        *self.cache_plans.lock().expect("session cache flag lock") = false;
        *self
            .plan_cache_key
            .lock()
            .expect("session plan cache key lock") = None;
        result
    }

    /// Execute every statement in `sql` and return each result ( multi-resultset).
    pub fn execute_all(&self, sql: &str) -> Result<Vec<QueryResult>> {
        let mode = self
            .sql_mode
            .lock()
            .expect("session sql_mode lock")
            .clone();
        reject_create_table_func_call(sql, &mode)?;
        // sqlparser always applies MySQL backslash escapes. Under
        // NO_BACKSLASH_ESCAPES, double `\` inside quotes so the unescaped
        // value matches TiDB (literal backslash).
        let rewritten;
        let sql = if sql_mode_has(&mode, "NO_BACKSLASH_ESCAPES") {
            rewritten = double_backslashes_in_sql_strings(sql);
            rewritten.as_str()
        } else {
            sql
        };
        let stmts = {
            // `HIGH_NOT_PRECEDENCE` binds `NOT` tighter than `BETWEEN` /
            // `IN` / `LIKE`; sqlparser always binds it looser, so the lowering
            // needs the session mode.
            sealdb_parser::set_high_not_precedence(sql_mode_has(&mode, "HIGH_NOT_PRECEDENCE"));
            let parsed = self.parser.parse_sql(sql);
            sealdb_parser::set_high_not_precedence(false);
            parsed?
        };
        if stmts.is_empty() {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_PARSE_ERROR,
                "empty SQL",
            ));
        }
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            out.push(self.execute_one(&stmt)?);
        }
        Ok(out)
    }

    fn execute_one(&self, stmt: &Statement) -> Result<QueryResult> {
        // Publish this session's `tidb_window_concurrency` for the planner and
        // executor (thread-local), so a suite that changes it cannot leak its
        // value into a later session on the same thread.
        sealdb_expression::set_window_concurrency(
            *self
                .window_concurrency
                .lock()
                .expect("session window_concurrency lock"),
        );
        let mut owned = None;
        let stmt = if let Statement::Show(show) = stmt
            && show.kind == sealdb_parser::ast::ShowStmtKind::Grants
            && show.user.is_none()
        {
            let mut show = show.clone();
            show.user = Some(self.current_user());
            owned.insert(Statement::Show(show))
        } else {
            stmt
        };
        self.check_privilege(stmt)?;
        if is_write_stmt(stmt) && self.in_read_only() {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION,
                "Cannot execute statement in a READ ONLY transaction.",
            ));
        }
        // Schema lease 8027 (disabled when lease is Duration::ZERO).
        self.domain.check_schema_lease()?;
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
            Statement::Set(set) => {
                let warnings = apply_set(self, set)?;
                // `SET` bypasses the shared plan path below, so record its
                // warnings here for a following `SHOW WARNINGS`.
                *self
                    .last_warnings
                    .lock()
                    .expect("session last_warnings lock") = warnings.clone();
                return Ok(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    warnings,
                    affected_rows: 0,
                    info: String::new(),
                });
            }
            Statement::Do(doit) => {
                for expr in &doit.exprs {
                    let _ = sealdb_expression::eval_expr(expr)?;
                }
                return Ok(empty_result());
            }
            Statement::SetPwd(set) => {
                let user = self.current_user();
                self.domain.privilege().set_password(set, &user)?;
                self.domain.persist_privileges()?;
                return Ok(empty_result());
            }
            Statement::Flush(flush) => {
                match flush.tp {
                    sealdb_parser::ast::FlushStmtType::Privileges => {
                        self.domain.reload_privileges()?;
                    }
                    sealdb_parser::ast::FlushStmtType::Tables
                    | sealdb_parser::ast::FlushStmtType::Other => {}
                }
                return Ok(empty_result());
            }
            Statement::Shutdown(_) => {
                self.domain.request_shutdown();
                return Ok(empty_result());
            }
            Statement::Kill(kill) => {
                let _ = self.domain.kill_connection(kill.connection_id);
                return Ok(empty_result());
            }
            Statement::LockTables(lock) => {
                let current_db = self
                    .current_db
                    .lock()
                    .expect("session current_db lock")
                    .clone();
                let locks: Vec<(String, bool)> = lock
                    .table_locks
                    .iter()
                    .map(|item| {
                        let key = mdl_table_key(&item.table, &current_db);
                        let write =
                            matches!(item.lock_type, sealdb_parser::ast::TableLockType::Write);
                        (key, write)
                    })
                    .collect();
                self.domain.lock_tables(self.session_id, &locks)?;
                return Ok(empty_result());
            }
            Statement::CleanupTableLock(cleanup) => {
                if cleanup.tables.is_empty() {
                    self.domain.unlock_tables(self.session_id);
                } else {
                    let current_db = self
                        .current_db
                        .lock()
                        .expect("session current_db lock")
                        .clone();
                    let keys: Vec<String> = cleanup
                        .tables
                        .iter()
                        .map(|t| mdl_table_key(t, &current_db))
                        .collect();
                    self.domain.cleanup_table_locks(&keys);
                }
                return Ok(empty_result());
            }
            Statement::OptimizeTable(optimize) => {
                let current_db = self
                    .current_db
                    .lock()
                    .expect("session current_db lock")
                    .clone();
                return Ok(table_status_result(
                    "optimize",
                    &optimize.tables,
                    &current_db,
                ));
            }
            Statement::RepairTable(repair) => {
                let current_db = self
                    .current_db
                    .lock()
                    .expect("session current_db lock")
                    .clone();
                return Ok(table_status_result("repair", &repair.tables, &current_db));
            }
            Statement::RenameUser(rename) => {
                self.domain.privilege().rename_user(rename)?;
                self.domain.persist_privileges()?;
                return Ok(empty_result());
            }
            Statement::LoadStats(stmt) => {
                self.load_stats_file(&stmt.path)?;
                return Ok(empty_result());
            }
            Statement::Binlog(_)
            | Statement::Change(_)
            | Statement::AlterInstance(_)
            | Statement::LockStats(_)
            | Statement::UnlockStats(_)
            | Statement::Trace(_)
            | Statement::PlanReplayer(_)
            | Statement::ImportInto(_)
            | Statement::SplitRegion(_)
            | Statement::DistributeTable(_)
            | Statement::CompactTable(_)
            | Statement::RecommendIndex(_)
            | Statement::CalibrateResource(_)
            | Statement::FlashBackTable(_)
            | Statement::FlashBackDatabase(_)
            | Statement::FlashBackToTimestamp(_)
            | Statement::RecoverTable(_)
            | Statement::DropPlacementPolicy(_)
            | Statement::CreateResourceGroup(_)
            | Statement::SetResourceGroup(_)
            | Statement::AddQueryWatch(_)
            | Statement::DropQueryWatch(_) => {
                return Ok(empty_result());
            }
            Statement::BRIE(brie) => {
                return crate::brie::execute_brie(self, &brie.raw_sql);
            }
            Statement::SetRole(set) => {
                let user = self.current_user();
                let roles = self
                    .domain
                    .privilege()
                    .resolve_active_roles(&user, &set.kind)?;
                *self.active_roles.lock().expect("session active roles lock") = roles;
                return Ok(empty_result());
            }
            Statement::SetTxn(set) => {
                if let Some(isolation) = set.isolation {
                    *self.isolation.lock().expect("session isolation lock") =
                        map_isolation(isolation);
                }
                if let Some(read_only) = set.read_only {
                    *self.read_only.lock().expect("session read_only lock") = read_only;
                }
                return Ok(empty_result());
            }
            Statement::Prepare(prepare) => {
                self.prepared
                    .lock()
                    .expect("session prepared lock")
                    .insert(prepare.name.to_ascii_lowercase(), prepare.sql.clone());
                return Ok(empty_result());
            }
            Statement::Deallocate(dealloc) => {
                self.prepared
                    .lock()
                    .expect("session prepared lock")
                    .remove(&dealloc.name.to_ascii_lowercase());
                return Ok(empty_result());
            }
            Statement::CreateBinding(create) => {
                let original = self.parser.parse_one_stmt(&create.original_sql)?;
                let key = sealdb_bindinfo::normalize_sql(&restore_sql(&original));
                let db = self
                    .current_db
                    .lock()
                    .expect("session current_db lock")
                    .clone();
                self.domain.bind_handle().create_binding(
                    sealdb_bindinfo::Binding::manual(
                        create.original_sql.clone(),
                        create.bind_sql.clone(),
                        db,
                    ),
                    &key,
                );
                return Ok(empty_result());
            }
            Statement::DropBinding(drop) => {
                let original = self.parser.parse_one_stmt(&drop.original_sql)?;
                let key = sealdb_bindinfo::normalize_sql(&restore_sql(&original));
                self.domain.bind_handle().drop_binding(&key);
                return Ok(empty_result());
            }
            Statement::Execute(exec) => {
                let sql = self
                    .prepared
                    .lock()
                    .expect("session prepared lock")
                    .get(&exec.name.to_ascii_lowercase())
                    .cloned()
                    .ok_or_else(|| {
                        sealdb_errno::Error::new(
                            sealdb_errno::ERR_UNKNOWN_STMT_HANDLER,
                            format!("Unknown prepared statement handler ({})", exec.name),
                        )
                    })?;
                let using = resolve_exec_params(self, &exec.using)?;
                let bound = bind_sql(&sql, &using)?;
                let inner = self.parser.parse_one_stmt(&bound)?;
                // MVB: still plan/execute the bound SQL, but cache under the
                // unbound `?` template so different USING literals share one entry.
                //
                // Subqueries are excluded: decorrelation rewrites (semi/anti
                // join, apply) bake the bound constants into the join keys, and
                // unlike PointGet/index lookups those literals are not re-read
                // from the current statement, so a cached plan would answer a
                // later EXECUTE with the first USING values.
                let cacheable = !sql.to_ascii_lowercase().contains("(select");
                *self.cache_plans.lock().expect("session cache flag lock") = cacheable;
                *self
                    .plan_cache_key
                    .lock()
                    .expect("session plan cache key lock") = Some(normalize_prep_cache_key(&sql));
                let result = self.execute_one(&inner);
                *self.cache_plans.lock().expect("session cache flag lock") = false;
                *self
                    .plan_cache_key
                    .lock()
                    .expect("session plan cache key lock") = None;
                return result;
            }
            Statement::Begin(begin) => {
                let isolation = begin
                    .isolation
                    .map(map_isolation)
                    .unwrap_or_else(|| *self.isolation.lock().expect("session isolation lock"));
                let read_only = begin
                    .read_only
                    .unwrap_or_else(|| *self.read_only.lock().expect("session read_only lock"));
                self.apply_pending_schema(true)?;
                let schema_ver = self.domain.infoschema().schema_meta_version();
                let mut txn = self.txn.lock().expect("session txn lock");
                if begin.pessimistic {
                    txn.enter_begin_pessimistic(
                        self.domain.as_ref(),
                        schema_ver,
                        isolation,
                        read_only,
                    )?;
                } else {
                    txn.enter_begin(self.domain.as_ref(), schema_ver, isolation, read_only)?;
                }
                return Ok(empty_result());
            }
            Statement::Commit(commit) => {
                let schema_ver = self.domain.infoschema().schema_meta_version();
                {
                    let txn = self.txn.lock().expect("session txn lock");
                    if txn.is_explicit() {
                        sealdb_domain::schema_checker::check(txn.schema_ver(), schema_ver)?;
                    }
                }
                self.apply_pending_schema(true)?;
                let schema_ver = self.domain.infoschema().schema_meta_version();
                let mut txn = self.txn.lock().expect("session txn lock");
                if commit.chain {
                    txn.chain(self.domain.as_ref(), schema_ver, true)?;
                } else {
                    txn.commit()?;
                }
                self.domain.mdl_unlock_session(self.session_id);
                return Ok(empty_result());
            }
            Statement::Rollback(rb) => {
                if let Some(name) = &rb.savepoint {
                    self.txn
                        .lock()
                        .expect("session txn lock")
                        .rollback_to(name)?;
                    return Ok(empty_result());
                }
                self.apply_pending_schema(false)?;
                let schema_ver = self.domain.infoschema().schema_meta_version();
                let mut txn = self.txn.lock().expect("session txn lock");
                if rb.chain {
                    txn.chain(self.domain.as_ref(), schema_ver, false)?;
                } else {
                    txn.rollback()?;
                }
                self.domain.mdl_unlock_session(self.session_id);
                return Ok(empty_result());
            }
            Statement::Savepoint(sp) => {
                let schema_ver = self.domain.infoschema().schema_meta_version();
                self.txn.lock().expect("session txn lock").savepoint(
                    self.domain.as_ref(),
                    schema_ver,
                    &sp.name,
                )?;
                return Ok(empty_result());
            }
            Statement::ReleaseSavepoint(sp) => {
                self.txn
                    .lock()
                    .expect("session txn lock")
                    .release_savepoint(&sp.name)?;
                return Ok(empty_result());
            }
            Statement::CreateDatabase(_)
            | Statement::DropDatabase(_)
            | Statement::AlterDatabase(_)
            | Statement::CreateTable(_)
            | Statement::CreateView(_)
            | Statement::CreateSequence(_)
            | Statement::DropSequence(_)
            | Statement::AlterSequence(_)
            | Statement::DropTable(_)
            | Statement::TruncateTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::AlterTable(_)
                if self.txn.lock().expect("session txn lock").is_explicit() =>
            {
                self.capture_schema_snap();
            }
            _ => {}
        }
        {
            let txn = self.txn.lock().expect("session txn lock");
            if txn.is_explicit() {
                sealdb_domain::schema_checker::check(
                    txn.schema_ver(),
                    self.domain.infoschema().schema_meta_version(),
                )?;
                if txn.isolation() == IsolationLevel::ReadCommitted {
                    txn.refresh_snapshot()?;
                }
            }
        }
        // TiDB: TRUNCATE TABLE is DDL and implicitly commits the open
        // transaction before clearing rows (it cannot be rolled back).
        if matches!(stmt, Statement::TruncateTable(_))
            && self.txn.lock().expect("session txn lock").is_explicit()
        {
            self.apply_pending_schema(true)?;
            self.txn.lock().expect("session txn lock").commit()?;
            self.domain.mdl_unlock_session(self.session_id);
        }
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        self.acquire_mdl(stmt, &current_db)?;
        // Autocommit statements release MDL when the statement ends (including
        // error paths). Explicit txns hold until COMMIT/ROLLBACK.
        let mut mdl_guard = MdlGuard {
            domain: &self.domain,
            session_id: self.session_id,
            active: !self.txn.lock().expect("session txn lock").is_explicit(),
        };
        let is = self.domain.infoschema();
        let ctas_insert = match stmt {
            Statement::CreateTable(create) => create.select.as_ref().and_then(|select| {
                let (schema, table) = create.table.qualified(&current_db);
                if create.if_not_exists && is.table_by_name(&schema, &table).is_some() {
                    return None;
                }
                Some(InsertStmt {
                    table: create.table.clone(),
                    ignore: false,
                    replace: false,
                    partitions: Vec::new(),
                    columns: Vec::new(),
                    rows: Vec::new(),
                    select: Some(select.clone()),
                    select_set: None,
                    with: None,
                    on_duplicate: Vec::new(),
                })
            }),
            _ => None,
        };
        // Apply CREATE BINDING rewrite before plan build (MVB: substitute bind SQL).
        let rebound = self.apply_sql_binding(stmt)?;
        let mut expanded = rebound.unwrap_or_else(|| stmt.clone());
        inherit_cte_scope(&mut expanded);
        expand_views(&mut expanded, &is, &current_db)?;
        // `ENUM`/`SET` numeric constants become member labels before planning so
        // index point gets encode the stored label (`a = 1` → `a = 'a'`).
        sealdb_planner::normalize_enum_constants(&mut expanded, Some(&is), &current_db);
        // String-vs-number comparisons get a `double` cast on the column before
        // planning (and execution) so point gets and residuals agree.
        sealdb_planner::normalize_comparison_literals(&mut expanded, Some(&is), &current_db);
        // `NATURAL JOIN` has no schema at parse time: resolve common columns
        // against the catalog before planning (TiDB buildNaturalJoin).
        if sealdb_planner::stmt_has_natural_join(&expanded) {
            expanded = sealdb_planner::expand_natural_joins(&expanded, Some(&is), &current_db)?;
        }
        if let Statement::LoadData(load) = &expanded {
            let insert = load_data_to_insert(load, &is, &current_db)?;
            mdl_guard.active = false;
            return self.execute_one(&Statement::Insert(insert));
        }
        if needs_user_var_subst(&expanded) {
            expanded = subst_user_vars_in_stmt(self, &expanded);
        }
        let stmt = &expanded;
        if let Some(result) = try_eval_nextval(self, stmt, &current_db)? {
            drop(mdl_guard);
            return Ok(result);
        }
        if let Statement::Show(show) = stmt
            && show.kind == sealdb_parser::ast::ShowStmtKind::Warnings
        {
            drop(mdl_guard);
            return Ok(show_warnings_result(self));
        }
        check_only_full_group_by(self, stmt)?;
        if let Some(result) = system_var_result(self, stmt) {
            return Ok(result);
        }
        let stats = self.domain.stats();
        let schema_ver = is.schema_meta_version();
        // Prepared EXECUTE may override with the unbound `?` template key.
        let cache_key = self
            .plan_cache_key
            .lock()
            .expect("session plan cache key lock")
            .clone()
            .unwrap_or_else(|| restore_sql(stmt));
        let use_cache = *self.cache_plans.lock().expect("session cache flag lock");
        let prune_mode = *self
            .partition_prune_mode
            .lock()
            .expect("session partition prune mode lock");
        // EXPLAIN / semi-join strings use `current_db_name()` (suite schema).
        let explicit_txn = self
            .txn
            .lock()
            .expect("session txn lock")
            .is_explicit()
            || !*self.autocommit.lock().expect("session autocommit lock");
        let plan = sealdb_expression::with_current_db(current_db.as_str(), || {
            sealdb_planner::with_explicit_txn(explicit_txn, || {
            if use_cache {
                let cache = self.domain.plan_cache();
                // Re-optimize every EXECUTE: the cached plan was built from the
                // first bound SQL, so literal values baked into ranges, pushed
                // cop filters, or pruned partitions would answer a later USING
                // list incorrectly. The cache still accounts a hit so shared
                // plan-cache statistics match TiDB.
                let cached = cache.get(&cache_key, schema_ver).is_some();
                let plan = self.planner.build_with_schema(
                    stmt,
                    Some(&is),
                    &current_db,
                    Some(stats.as_ref()),
                    prune_mode,
                )?;
                if !cached {
                    cache.put(cache_key, schema_ver, plan.clone());
                }
                Ok(plan)
            } else {
                self.planner.build_with_schema(
                    stmt,
                    Some(&is),
                    &current_db,
                    Some(stats.as_ref()),
                    prune_mode,
                )
            }
            })
        })?;
        let persist_ddl = !self.txn.lock().expect("session txn lock").is_explicit();
        let exec = if matches!(&plan, PhysicalPlan::DDL { .. }) {
            Box::new(DDLExec::new(
                Arc::clone(&self.domain),
                stmt.clone(),
                current_db.clone(),
                persist_ddl,
            ))
        } else {
            let txn = self
                .txn
                .lock()
                .expect("session txn lock")
                .ensure(self.domain.as_ref())?;
            ExecutorBuilder::build(&plan, stmt, Arc::clone(&self.domain), &current_db, txn)?
        };
        let coll = self
            .collation
            .lock()
            .expect("session collation lock")
            .clone();
        let sql_mode = self
            .sql_mode
            .lock()
            .expect("session sql_mode lock")
            .clone();
        let time_zone = self
            .time_zone
            .lock()
            .expect("session time_zone lock")
            .clone();
        let collate_mode = sealdb_expression::CollateMode::from_name(&coll.name);
        let div_mode = div_by_zero_mode(&sql_mode, stmt);
        let clustered_index = *self
            .clustered_index
            .lock()
            .expect("session clustered index lock");
        let result = with_clustered_index_mode(clustered_index, || {
            sealdb_expression::with_div_by_zero_mode(div_mode, || {
                sealdb_expression::with_sql_mode(sql_mode, || {
                    sealdb_expression::with_time_zone(time_zone, || {
                        sealdb_expression::with_current_db(current_db.as_str(), || {
                            sealdb_expression::with_collation(collate_mode, || {
                                sealdb_expression::clear_warnings();
                                // TiDB allocates a fresh memory tracker per query.
                                sealdb_expression::reset_query_mem_usage();
                                ExecutorBuilder::run(exec)
                            })
                        })
                    })
                })
            })
        });
        if result.is_ok() && matches!(&plan, PhysicalPlan::DDL { .. }) && !persist_ddl {
            self.txn
                .lock()
                .expect("session txn lock")
                .set_schema_ver(self.domain.infoschema().schema_meta_version());
        }
        self.txn
            .lock()
            .expect("session txn lock")
            .finish_autocommit(result.is_ok())?;
        // Explicit success path keeps MDL; autocommit Drop releases.
        if result.is_ok()
            && let Some(insert) = ctas_insert
        {
            // Keep MDL across CTAS follow-up INSERT; reinstate on nested call.
            mdl_guard.active = false;
            return self.execute_one(&Statement::Insert(insert));
        }
        if result.is_ok() {
            maybe_auto_analyze(self, stmt)?;
        }
        drop(mdl_guard);
        if let Ok(qr) = &result {
            *self
                .last_warnings
                .lock()
                .expect("session last_warnings lock") = qr.warnings.clone();
        }
        result
    }

    fn acquire_mdl(&self, stmt: &Statement, current_db: &str) -> Result<()> {
        let (keys, exclusive) = mdl_targets(stmt, current_db);
        // MySQL: a session holding `LOCK TABLES … READ` cannot write the table.
        if is_write_stmt(stmt) {
            for key in &keys {
                if self
                    .domain
                    .session_read_locks_table(self.session_id, key)
                {
                    let table = key.rsplit('.').next().unwrap_or(key);
                    return Err(sealdb_errno::Error::new(
                        sealdb_errno::ERR_TABLE_NOT_LOCKED_FOR_WRITE,
                        format!("Table '{table}' was locked with a READ lock and can't be updated"),
                    ));
                }
            }
        }
        for key in keys {
            if key.ends_with('.') || key.starts_with('.') {
                continue;
            }
            if exclusive {
                self.domain.mdl_lock_exclusive(&key, self.session_id)?;
            } else {
                self.domain.mdl_lock_shared(&key, self.session_id)?;
            }
        }
        Ok(())
    }

    fn capture_schema_snap(&self) {
        let mut snap = self.schema_snap.lock().expect("session schema snap lock");
        if snap.is_none() {
            *snap = Some(self.domain.schema_snap());
        }
    }

    /// Persist in-memory DDL on COMMIT, or restore the pre-DDL snapshot on
    /// ROLLBACK. CREATE INDEX / ALTER backfill KV is not undone.
    fn apply_pending_schema(&self, commit: bool) -> Result<()> {
        let mut snap = self.schema_snap.lock().expect("session schema snap lock");
        if commit {
            if snap.is_some() {
                self.domain.persist_schema()?;
                *snap = None;
            }
        } else if let Some(prev) = snap.take() {
            self.domain.restore_schema(prev);
        }
        Ok(())
    }

    pub fn infoschema(&self) -> InfoSchema {
        self.domain.infoschema()
    }

    pub fn stats(&self) -> Arc<sealdb_statistics::Handle> {
        self.domain.stats()
    }

    fn check_privilege(&self, stmt: &Statement) -> Result<()> {
        let mgr = self.domain.privilege();
        if mgr.skip_grant_table() {
            return Ok(());
        }
        let user = self.current_user();
        let roles = self
            .active_roles
            .lock()
            .expect("session active roles lock")
            .clone();
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        if let Statement::Explain(explain) = stmt {
            return self.check_privilege(&explain.stmt);
        }
        match stmt {
            Statement::Grant(_)
            | Statement::Revoke(_)
            | Statement::CreateRole(_)
            | Statement::DropRole(_)
            | Statement::CreateUser(_)
            | Statement::DropUser(_)
            | Statement::GrantRole(_)
            | Statement::RevokeRole(_)
            | Statement::SetPwd(_)
            | Statement::Flush(_)
            | Statement::Shutdown(_)
            | Statement::Kill(_)
            | Statement::RenameUser(_)
            | Statement::Binlog(_)
            | Statement::Change(_)
            | Statement::AlterInstance(_)
            | Statement::CleanupTableLock(_) => {
                if !mgr.request_verification_with_roles(&user, &roles, "", "", PrivilegeType::ALL) {
                    return Err(access_denied("", "", PrivilegeType::ALL, &user));
                }
            }
            Statement::LockTables(lock) => {
                for item in &lock.table_locks {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &item.table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::OptimizeTable(optimize) => {
                for table in &optimize.tables {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::RepairTable(repair) => {
                for table in &repair.tables {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::Show(show)
                if show.kind == sealdb_parser::ast::ShowStmtKind::Grants
                    && show.user.as_ref().is_some_and(|u| u != &user)
                    && !mgr.request_verification_with_roles(
                        &user,
                        &roles,
                        "",
                        "",
                        PrivilegeType::ALL,
                    ) =>
            {
                return Err(access_denied("", "", PrivilegeType::ALL, &user));
            }
            Statement::Select(select) => {
                if let Some(from) = &select.from {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        from,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
                for table in select.join_tables() {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::Insert(insert) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &insert.table,
                    &current_db,
                    PrivilegeType::INSERT,
                )?;
            }
            Statement::Update(update) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &update.table,
                    &current_db,
                    PrivilegeType::UPDATE,
                )?;
                if let Some(join) = &update.join {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &join.left,
                        &current_db,
                        PrivilegeType::UPDATE,
                    )?;
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &join.right,
                        &current_db,
                        PrivilegeType::UPDATE,
                    )?;
                }
            }
            Statement::Delete(delete) => {
                if delete.tables.is_empty() {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &delete.table,
                        &current_db,
                        PrivilegeType::DELETE,
                    )?;
                } else {
                    for table in &delete.tables {
                        require(
                            &mgr,
                            &user,
                            &roles,
                            table,
                            &current_db,
                            PrivilegeType::DELETE,
                        )?;
                    }
                }
                if let Some(join) = &delete.join {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &join.left,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                    require(
                        &mgr,
                        &user,
                        &roles,
                        &join.right,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::CreateDatabase(create) => {
                if !mgr.request_verification_with_roles(
                    &user,
                    &roles,
                    &create.name,
                    "",
                    PrivilegeType::CREATE,
                ) {
                    return Err(access_denied(
                        &create.name,
                        "",
                        PrivilegeType::CREATE,
                        &user,
                    ));
                }
            }
            Statement::AlterDatabase(alter) => {
                let name = if alter.name.is_empty() {
                    current_db.as_str()
                } else {
                    alter.name.as_str()
                };
                if !mgr.request_verification_with_roles(
                    &user,
                    &roles,
                    name,
                    "",
                    PrivilegeType::CREATE,
                ) {
                    return Err(access_denied(name, "", PrivilegeType::CREATE, &user));
                }
            }
            Statement::CreateTable(create) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &create.table,
                    &current_db,
                    PrivilegeType::CREATE,
                )?;
                if let Some(refer) = &create.refer_table {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        refer,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
                if let Some(select) = &create.select {
                    if let Some(from) = &select.from {
                        require(
                            &mgr,
                            &user,
                            &roles,
                            from,
                            &current_db,
                            PrivilegeType::SELECT,
                        )?;
                    }
                    for table in select.join_tables() {
                        require(
                            &mgr,
                            &user,
                            &roles,
                            table,
                            &current_db,
                            PrivilegeType::SELECT,
                        )?;
                    }
                }
            }
            Statement::CreateView(create) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &create.view_name,
                    &current_db,
                    PrivilegeType::CREATE,
                )?;
                if let Some(from) = &create.select.from {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        from,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
                for table in create.select.join_tables() {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            Statement::CreateIndex(create) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &create.table,
                    &current_db,
                    PrivilegeType::CREATE,
                )?;
            }
            Statement::AlterTable(alter) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &alter.table,
                    &current_db,
                    PrivilegeType::CREATE,
                )?;
            }
            Statement::DropDatabase(drop) => {
                if !mgr.request_verification_with_roles(
                    &user,
                    &roles,
                    &drop.name,
                    "",
                    PrivilegeType::DROP,
                ) {
                    return Err(access_denied(&drop.name, "", PrivilegeType::DROP, &user));
                }
            }
            Statement::DropTable(drop) => {
                for table in &drop.tables {
                    require(&mgr, &user, &roles, table, &current_db, PrivilegeType::DROP)?;
                }
            }
            Statement::TruncateTable(truncate) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &truncate.table,
                    &current_db,
                    PrivilegeType::DROP,
                )?;
            }
            Statement::DropIndex(drop) => {
                require(
                    &mgr,
                    &user,
                    &roles,
                    &drop.table,
                    &current_db,
                    PrivilegeType::DROP,
                )?;
            }
            Statement::AnalyzeTable(analyze) => {
                for table in analyze.all_tables() {
                    require(
                        &mgr,
                        &user,
                        &roles,
                        table,
                        &current_db,
                        PrivilegeType::SELECT,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `LOAD STATS '<file>'` (TiDB `handle.LoadStatsFromJSON`).
    ///
    /// The path is resolved relative to the process CWD first and then to the
    /// integration-test data directory, matching TiDB's `integrationtest`
    /// working directory (`s/…`).
    fn load_stats_file(&self, path: &str) -> Result<()> {
        let mut candidates = vec![std::path::PathBuf::from(path)];
        if !std::path::Path::new(path).is_absolute() {
            candidates.push(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/integrationtest")
                    .join(path),
            );
            candidates.push(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../tests/integrationtest")
                    .join(path),
            );
        }
        let file = candidates
            .iter()
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| {
                Error::new(
                    sealdb_errno::ERR_UNKNOWN_ERROR,
                    format!("LOAD STATS: file not found: {path}"),
                )
            })?;
        let raw = std::fs::read_to_string(file).map_err(|e| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("LOAD STATS: {e}"),
            )
        })?;
        let (database, table, stats) = sealdb_statistics::table_from_tidb_json(&raw)?;
        if database.is_empty() || table.is_empty() {
            return Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                "LOAD STATS: JSON has no database_name/table_name",
            ));
        }
        self.domain.stats().update(database, table, stats);
        self.domain.persist_stats()?;
        Ok(())
    }
}

fn require(
    mgr: &sealdb_privilege::Manager,
    user: &UserIdentity,
    roles: &[UserIdentity],
    table: &sealdb_parser::ast::TableName,
    current_db: &str,
    privs: PrivilegeType,
) -> Result<()> {
    let (db, name) = table.qualified(current_db);
    if !mgr.request_verification_with_roles(user, roles, &db, &name, privs) {
        return Err(access_denied(&db, &name, privs, user));
    }
    Ok(())
}

fn access_denied(
    db: &str,
    table: &str,
    wanted: PrivilegeType,
    user: &UserIdentity,
) -> sealdb_errno::Error {
    sealdb_privilege::Manager::access_denied(db, table, wanted, user)
}

fn empty_result() -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        warnings: Vec::new(),
        affected_rows: 0,
        info: String::new(),
    }
}

fn show_warnings_result(session: &Session) -> QueryResult {
    let warnings = session
        .last_warnings
        .lock()
        .expect("session last_warnings lock")
        .clone();
    QueryResult {
        columns: vec![
            "Level".to_string(),
            "Code".to_string(),
            "Message".to_string(),
        ],
        rows: warnings
            .into_iter()
            .map(|(code, msg)| {
                vec![
                    sealdb_types::Datum::Bytes(b"Warning".to_vec()),
                    sealdb_types::Datum::I64(code as i64),
                    sealdb_types::Datum::Bytes(msg.into_bytes()),
                ]
            })
            .collect(),
        warnings: Vec::new(),
        affected_rows: 0,
        info: String::new(),
    }
}

/// MySQL-style OPTIMIZE/REPAIR TABLE status rows: Table, Op, Msg_type, Msg_text.
fn table_status_result(op: &str, tables: &[TableName], current_db: &str) -> QueryResult {
    let columns = vec![
        "Table".to_string(),
        "Op".to_string(),
        "Msg_type".to_string(),
        "Msg_text".to_string(),
    ];
    let rows = tables
        .iter()
        .map(|table| {
            let (schema, name) = table.qualified(current_db);
            let display = if schema.is_empty() {
                name
            } else {
                format!("{schema}.{name}")
            };
            vec![
                sealdb_types::Datum::Bytes(display.into_bytes()),
                sealdb_types::Datum::Bytes(op.as_bytes().to_vec()),
                sealdb_types::Datum::Bytes(b"status".to_vec()),
                sealdb_types::Datum::Bytes(b"OK".to_vec()),
            ]
        })
        .collect();
    QueryResult {
        columns,
        rows,
        warnings: Vec::new(),
        affected_rows: 0,
        info: String::new(),
    }
}

/// MySQL/TiDB ONLY_FULL_GROUP_BY: when the mode is set and the query has
/// GROUP BY, every SELECT item must be an aggregate, a constant, or appear
/// in the GROUP BY list (exact restore match). CTE/derived single-source
/// columns grouped by another column of that CTE are allowed (TiDB cte suite
/// `select b from qn group by a`); base-table PK functional dependency is
/// still out of scope.
fn check_only_full_group_by(session: &Session, stmt: &Statement) -> Result<()> {
    let select = match stmt {
        Statement::Select(s) => s,
        Statement::Explain(e) => match e.stmt.as_ref() {
            Statement::Select(s) => s,
            _ => return Ok(()),
        },
        Statement::Insert(insert) => match insert.select.as_ref() {
            Some(s) => s,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    let mode = session
        .sql_mode
        .lock()
        .expect("session sql_mode lock")
        .clone();
    if !sql_mode_has(&mode, "ONLY_FULL_GROUP_BY") {
        return Ok(());
    }
    let current_db = session
        .current_db
        .lock()
        .expect("session current_db lock")
        .clone();
    check_select_only_full_group_by(select, &current_db)
}

/// One SELECT block plus every nested block reachable from it.
///
/// Derived `FROM (SELECT …)` tables are lowered into synthetic CTEs on the
/// same `SelectStmt`, so recursing through `with.ctes` also validates the
/// bodies of derived tables (TiDB reports 1055 for those too).
fn check_select_only_full_group_by(select: &SelectStmt, current_db: &str) -> Result<()> {
    if !select.group_by.is_empty() {
        let cte_loose = cte_group_by_allows_peer_columns(select);
        for (i, field) in select.fields.iter().enumerate() {
            if select_field_ok_for_group_by(&field.expr, &select.group_by) {
                continue;
            }
            if cte_loose && matches!(&field.expr, ExprNode::Column(_)) {
                continue;
            }
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_WRONG_FIELD_WITH_GROUP,
                nonaggregated_column_message(i + 1, &field.expr, select, current_db),
            ));
        }
    }
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            check_select_only_full_group_by(&cte.query, current_db)?;
            if let Some((_, arm)) = &cte.recursive {
                check_select_only_full_group_by(arm, current_db)?;
            }
            for (_, arm) in &cte.recursive_rest {
                check_select_only_full_group_by(arm, current_db)?;
            }
            for (_, arm) in &cte.set_rest {
                check_select_only_full_group_by(arm, current_db)?;
            }
        }
    }
    Ok(())
}

/// TiDB names the offending column as `{db}.{table}.{col}` when it can
/// resolve the owner (`e2.clock` for `SELECT clock FROM events e2 GROUP BY
/// e2.source`), and omits the name for expressions without a single column.
fn nonaggregated_column_message(
    position: usize,
    expr: &ExprNode,
    select: &SelectStmt,
    current_db: &str,
) -> String {
    let column = match expr {
        ExprNode::Column(col) => {
            let owner = col
                .table
                .clone()
                .or_else(|| select.from.as_ref().map(|t| t.name.clone()));
            owner.map(|owner| {
                let db = if current_db.is_empty() {
                    "select"
                } else {
                    current_db
                };
                format!("column '{db}.{owner}.{}'", col.name)
            })
        }
        _ => None,
    }
    .unwrap_or_else(|| "column".to_string());
    format!(
        "Expression #{position} of SELECT list is not in GROUP BY clause and contains nonaggregated {column} which is not functionally dependent on columns in GROUP BY clause; this is incompatible with sql_mode=only_full_group_by"
    )
}

/// TiDB: `WITH qn AS (SELECT a,b …) SELECT b FROM qn GROUP BY a` is accepted
/// under ONLY_FULL_GROUP_BY when FROM is that single CTE and GROUP BY is a
/// bare column from it.
fn cte_group_by_allows_peer_columns(select: &SelectStmt) -> bool {
    if !select.joins.is_empty() || select.from_item.is_some() {
        return false;
    }
    let Some(from) = &select.from else {
        return false;
    };
    let Some(with) = &select.with else {
        return false;
    };
    if with.find(from.resolve_name()).is_none() {
        return false;
    }
    !select.group_by.is_empty()
        && select
            .group_by
            .iter()
            .all(|g| matches!(g, ExprNode::Column(_)))
}

fn sql_mode_has(mode: &str, flag: &str) -> bool {
    mode.split(',')
        .map(|p| p.trim())
        .any(|p| p.eq_ignore_ascii_case(flag))
}

/// TiDB IGNORE_SPACE / bare function-name CREATE TABLE: `COUNT(a)` is a
/// syntax error; `COUNT (a)` is OK unless IGNORE_SPACE; quoted / qualified OK.
fn reject_create_table_func_call(sql: &str, sql_mode: &str) -> Result<()> {
    let ignore_space = sql_mode_has(sql_mode, "IGNORE_SPACE");
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !keyword_at_ci(bytes, i, b"CREATE") {
            i += 1;
            continue;
        }
        let mut j = skip_ws_bytes(bytes, i + 6);
        if !keyword_at_ci(bytes, j, b"TABLE") {
            i += 1;
            continue;
        }
        j = skip_ws_bytes(bytes, j + 5);
        if keyword_at_ci(bytes, j, b"IF") {
            j = skip_ws_bytes(bytes, j + 2);
            if keyword_at_ci(bytes, j, b"NOT") {
                j = skip_ws_bytes(bytes, j + 3);
                if keyword_at_ci(bytes, j, b"EXISTS") {
                    j = skip_ws_bytes(bytes, j + 6);
                }
            }
        }
        // Backtick-quoted name is always allowed.
        if j < bytes.len() && bytes[j] == b'`' {
            i = j + 1;
            continue;
        }
        // Qualified `db.name` — TiDB allows function names after the dot.
        let name_start = j;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if name_start == j {
            i += 1;
            continue;
        }
        let name = std::str::from_utf8(&bytes[name_start..j]).unwrap_or("");
        if j < bytes.len() && bytes[j] == b'.' {
            i = j + 1;
            continue;
        }
        if !is_create_table_func_name(name) {
            i = j;
            continue;
        }
        let after_name = j;
        let after_ws = skip_ws_bytes(bytes, after_name);
        if after_ws >= bytes.len() || bytes[after_ws] != b'(' {
            i = j;
            continue;
        }
        let had_space = after_ws > after_name;
        // No space → always error; space → error only with IGNORE_SPACE.
        if !had_space || ignore_space {
            let near = sql[name_start..].trim_end_matches(';').trim_end();
            // TiDB reports the 0-based offset of `(`.
            let col = after_ws;
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_PARSE_ERROR,
                format!(
                    // TiDB's mysql err message keeps a trailing space after the quote.
                    "You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 1 column {col} near \"{near};\" "
                ),
            ));
        }
        i = j;
    }
    Ok(())
}

fn is_create_table_func_name(name: &str) -> bool {
    // TiDB treats built-in function identifiers specially for CREATE TABLE.
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "BIT_AND" | "BIT_OR" | "BIT_XOR" | "NOW" | "SUM" | "AVG" | "MAX" | "MIN"
            | "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIME"
            | "LOCALTIMESTAMP" | "UTC_DATE" | "UTC_TIME" | "UTC_TIMESTAMP"
    ) || sealdb_expression::lookup(name).is_some()
}

fn keyword_at_ci(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    if i + kw.len() > bytes.len() {
        return false;
    }
    if !bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    let end = i + kw.len();
    end == bytes.len()
        || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
}

fn skip_ws_bytes(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Double each `\` inside `'...'` / `"..."` so sqlparser's unescape yields
/// literal backslashes (TiDB `NO_BACKSLASH_ESCAPES`).
fn double_backslashes_in_sql_strings(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            let quote = c;
            out.push(quote as char);
            i += 1;
            while i < bytes.len() {
                let b = bytes[i];
                if b == quote {
                    out.push(quote as char);
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        out.push(quote as char);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if b == b'\\' {
                    out.push('\\');
                    out.push('\\');
                    i += 1;
                    continue;
                }
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }
        if c == b'`' {
            out.push('`');
            i += 1;
            while i < bytes.len() {
                let ch = sql[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                if ch == '`' {
                    break;
                }
            }
            continue;
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn select_field_ok_for_group_by(expr: &ExprNode, group_by: &[ExprNode]) -> bool {
    if matches!(expr, ExprNode::Value(_)) {
        return true;
    }
    if let ExprNode::Func(func) = expr {
        let upper = func.name.to_ascii_uppercase();
        if matches!(upper.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") {
            return true;
        }
    }
    let restored = expr.restore_string();
    group_by
        .iter()
        .any(|g| g.restore_string().eq_ignore_ascii_case(&restored))
}

/// Record DML delta and run ANALYZE when modify/count exceeds the ratio
/// (TiDB auto-analyze refresher, synchronous for MemStorage tests).
fn maybe_auto_analyze(session: &Session, stmt: &Statement) -> Result<()> {
    let current_db = session
        .current_db
        .lock()
        .expect("session current_db lock")
        .clone();
    let targets: Vec<(String, String, u64)> = match stmt {
        Statement::Insert(insert) => {
            let (schema, name) = insert.table.qualified(&current_db);
            let delta = if insert.select.is_some() {
                1
            } else {
                insert.rows.len().max(1) as u64
            };
            vec![(schema, name, delta)]
        }
        Statement::Update(update) => {
            let (schema, name) = update.table.qualified(&current_db);
            vec![(schema, name, 1)]
        }
        Statement::Delete(delete) => {
            if delete.tables.is_empty() {
                let (schema, name) = delete.table.qualified(&current_db);
                vec![(schema, name, 1)]
            } else {
                delete
                    .tables
                    .iter()
                    .map(|t| {
                        let (schema, name) = t.qualified(&current_db);
                        (schema, name, 1u64)
                    })
                    .collect()
            }
        }
        _ => return Ok(()),
    };
    let enable = *session
        .enable_auto_analyze
        .lock()
        .expect("session auto analyze lock");
    let ratio = *session
        .auto_analyze_ratio
        .lock()
        .expect("session auto analyze ratio lock");
    let stats = session.domain.stats();
    let mut due = Vec::new();
    for (schema, name, delta) in targets {
        if schema.is_empty() {
            continue;
        }
        stats.add_modify(&schema, &name, delta);
        // TiDB analyzes partitioned tables asynchronously (partition jobs feed
        // the global stats separately), so an immediate EXPLAIN after INSERT
        // still reads the stats merged by the last explicit ANALYZE.
        let partitioned = session
            .domain
            .infoschema()
            .table_by_name(&schema, &name)
            .is_some_and(|info| info.partition.is_some());
        if enable && !partitioned && stats.needs_auto_analyze(&schema, &name, ratio) {
            due.push((schema, name));
        }
    }
    for (schema, name) in due {
        let analyze = Statement::AnalyzeTable(sealdb_parser::ast::AnalyzeTableStmt {
            table: sealdb_parser::ast::TableName::new(Some(schema), name),
            tables: Vec::new(),
            indexes: Vec::new(),
        });
        session.execute_one(&analyze)?;
    }
    Ok(())
}

fn is_user_var_name(name: &str) -> bool {
    name.starts_with('@') && !name.starts_with("@@")
}

fn user_var_key(name: &str) -> String {
    name.trim_start_matches('@').to_ascii_lowercase()
}

/// Evaluate an integer expression accepted as a `SET` numeric value, e.g.
/// `1 << 18` for `tidb_mem_quota_query`. Supports unary `-`, `+ - * / %`,
/// `<<`/`>>`, `|`, `&` and parentheses over decimal/hex literals.
fn parse_set_int_expr(value: &str) -> Option<i64> {
    let trimmed = value.trim().trim_matches(|c| c == '\'' || c == '"');
    let mut parser = SetIntParser {
        bytes: trimmed.as_bytes(),
        pos: 0,
    };
    let result = parser.parse_add()?;
    parser.skip_spaces();
    if parser.pos == parser.bytes.len() {
        Some(result)
    } else {
        // Plain quoted non-numeric values ("CANCEL") reach here.
        trimmed.parse::<i64>().ok()
    }
}

struct SetIntParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl SetIntParser<'_> {
    fn skip_spaces(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_spaces();
        let end = self.pos + token.len();
        if end <= self.bytes.len() && &self.bytes[self.pos..end] == token.as_bytes() {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn parse_add(&mut self) -> Option<i64> {
        let mut left = self.parse_mul()?;
        loop {
            if self.eat("+") {
                left = left.checked_add(self.parse_mul()?)?;
            } else if self.eat("-") {
                left = left.checked_sub(self.parse_mul()?)?;
            } else if self.eat("|") {
                left |= self.parse_mul()?;
            } else {
                return Some(left);
            }
        }
    }

    fn parse_mul(&mut self) -> Option<i64> {
        let mut left = self.parse_shift()?;
        loop {
            if self.eat("*") {
                left = left.checked_mul(self.parse_shift()?)?;
            } else if self.eat("/") {
                let rhs = self.parse_shift()?;
                if rhs == 0 {
                    return None;
                }
                left /= rhs;
            } else if self.eat("%") {
                let rhs = self.parse_shift()?;
                if rhs == 0 {
                    return None;
                }
                left %= rhs;
            } else if self.eat("&") {
                left &= self.parse_shift()?;
            } else {
                return Some(left);
            }
        }
    }

    fn parse_shift(&mut self) -> Option<i64> {
        let mut left = self.parse_unary()?;
        loop {
            if self.eat("<<") {
                let rhs = self.parse_unary()?;
                left = left.checked_shl(u32::try_from(rhs).ok()?)?;
            } else if self.eat(">>") {
                let rhs = self.parse_unary()?;
                left = left.checked_shr(u32::try_from(rhs).ok()?)?;
            } else {
                return Some(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Option<i64> {
        self.skip_spaces();
        if self.eat("-") {
            return self.parse_unary()?.checked_neg();
        }
        if self.eat("+") {
            return self.parse_unary();
        }
        if self.eat("(") {
            let value = self.parse_add()?;
            if !self.eat(")") {
                return None;
            }
            return Some(value);
        }
        self.parse_number()
    }

    fn parse_number(&mut self) -> Option<i64> {
        self.skip_spaces();
        let start = self.pos;
        if self.bytes[self.pos..].starts_with(b"0x") || self.bytes[self.pos..].starts_with(b"0X") {
            self.pos += 2;
            let digits = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            let text = std::str::from_utf8(&self.bytes[digits..self.pos]).ok()?;
            return i64::from_str_radix(text, 16).ok();
        }
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        let text: String = std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()?
            .chars()
            .filter(|c| *c != '_')
            .collect();
        text.parse().ok()
    }
}

fn parse_set_literal(value: &str) -> sealdb_types::Datum {    let v = value.trim();
    if v.eq_ignore_ascii_case("null") {
        return sealdb_types::Datum::Null;
    }
    if let Ok(i) = v.parse::<i64>() {
        return sealdb_types::Datum::I64(i);
    }
    if let Ok(f) = v.parse::<f64>() {
        return sealdb_types::Datum::F64(f);
    }
    let s = v.trim_matches(|c| c == '\'' || c == '"');
    sealdb_types::Datum::Bytes(s.as_bytes().to_vec())
}

fn resolve_exec_params(session: &Session, params: &[ExprNode]) -> Result<Vec<ExprNode>> {
    let vars = session.user_vars.lock().expect("session user vars lock");
    params
        .iter()
        .map(|expr| match expr {
            ExprNode::Column(col) if is_user_var_name(&col.name) => {
                let key = user_var_key(&col.name);
                let datum = vars.get(&key).cloned().unwrap_or(sealdb_types::Datum::Null);
                Ok(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr { datum },
                ))
            }
            other => Ok(other.clone()),
        })
        .collect()
}

fn needs_user_var_subst(stmt: &Statement) -> bool {
    let Statement::Select(select) = stmt else {
        return false;
    };
    select.fields.iter().any(|f| expr_has_session_var(&f.expr))
        || select
            .selection
            .as_ref()
            .is_some_and(|e| expr_has_session_var(e))
}

fn is_system_var_column(col: &sealdb_parser::ast::expressions::ColumnNameExpr) -> bool {
    col.name.starts_with("@@")
        || col
            .table
            .as_deref()
            .is_some_and(|t| t.starts_with('@'))
}

/// TiDB-style select-list name for expressions that mention session vars.
fn session_var_field_name(expr: &ExprNode) -> String {
    match expr {
        // TiDB names the column after the full reference, including the
        // `@@global.` / `@@session.` scope qualifier.
        ExprNode::Column(col) => match col.table.as_deref() {
            Some(table) if table.starts_with('@') => format!("{table}.{}", col.name),
            _ => col.name.clone(),
        },
        ExprNode::Binary(b) => {
            let left = session_var_field_name(&b.left);
            let right = match b.right.as_ref() {
                ExprNode::Value(v) => match &v.datum {
                    sealdb_types::Datum::Bytes(bytes) => {
                        format!("\"{}\"", String::from_utf8_lossy(bytes))
                    }
                    other => literal_field_name(other),
                },
                other => session_var_field_name(other),
            };
            format!("{left} {} {right}", b.op.restore())
        }
        other => other.restore_string(),
    }
}

fn expr_has_session_var(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Column(col) => is_user_var_name(&col.name) || is_system_var_column(col),
        ExprNode::Binary(b) => expr_has_session_var(&b.left) || expr_has_session_var(&b.right),
        ExprNode::Unary(u) => expr_has_session_var(&u.expr),
        ExprNode::Func(f) => f.args.iter().any(expr_has_session_var),
        ExprNode::Cast(c) => expr_has_session_var(&c.expr),
        ExprNode::InList(i) => {
            expr_has_session_var(&i.expr) || i.list.iter().any(expr_has_session_var)
        }
        ExprNode::Between(b) => {
            expr_has_session_var(&b.expr)
                || expr_has_session_var(&b.low)
                || expr_has_session_var(&b.high)
        }
        ExprNode::Like(l) => expr_has_session_var(&l.expr) || expr_has_session_var(&l.pattern),
        ExprNode::IsNull(i) => expr_has_session_var(&i.expr),
        _ => false,
    }
}

fn subst_user_vars_in_stmt(session: &Session, stmt: &Statement) -> Statement {
    let Statement::Select(select) = stmt else {
        return stmt.clone();
    };
    let mut select = select.clone();
    select.fields = select
        .fields
        .into_iter()
        .map(|mut field| {
            // Preserve TiDB display names (`@@autocommit`, `@b + "123"`) when
            // substituting vars to Values for evaluation.
            if field.as_name.as_ref().map_or(true, |s| s.is_empty())
                && expr_has_session_var(&field.expr)
            {
                field.as_name = Some(session_var_field_name(&field.expr));
            }
            field.expr = subst_user_vars_in_expr(session, &field.expr);
            field
        })
        .collect();
    if let Some(sel) = select.selection.take() {
        select.selection = Some(subst_user_vars_in_expr(session, &sel));
    }
    Statement::Select(select)
}

fn subst_user_vars_in_expr(session: &Session, expr: &ExprNode) -> ExprNode {
    match expr {
        ExprNode::Column(col) if is_user_var_name(&col.name) => {
            let key = user_var_key(&col.name);
            let datum = session
                .user_vars
                .lock()
                .expect("session user vars lock")
                .get(&key)
                .cloned()
                .unwrap_or(sealdb_types::Datum::Null);
            ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr { datum })
        }
        ExprNode::Column(col) if is_system_var_column(col) => {
            let global = col
                .table
                .as_deref()
                .map(|t| t.trim_start_matches('@').eq_ignore_ascii_case("global"))
                .unwrap_or(false);
            let name = col.name.trim_start_matches('@').to_ascii_lowercase();
            let value = if global {
                read_global_sysvar(session, &name)
            } else {
                read_session_sysvar(session, &name)
            };
            let datum = match value {
                Some(v) => sealdb_types::Datum::Bytes(v.into_bytes()),
                None => sealdb_types::Datum::Null,
            };
            ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr { datum })
        }
        ExprNode::Binary(b) => ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
            op: b.op,
            left: Box::new(subst_user_vars_in_expr(session, &b.left)),
            right: Box::new(subst_user_vars_in_expr(session, &b.right)),
            single_cnf_item: false,
        }),
        ExprNode::Unary(u) => ExprNode::Unary(sealdb_parser::ast::expressions::UnaryOpExpr {
            op: u.op,
            expr: Box::new(subst_user_vars_in_expr(session, &u.expr)),
        }),
        ExprNode::Func(f) => {
            let mut f = f.clone();
            f.args = f
                .args
                .iter()
                .map(|a| subst_user_vars_in_expr(session, a))
                .collect();
            ExprNode::Func(f)
        }
        ExprNode::Cast(c) => {
            let mut c = c.clone();
            c.expr = Box::new(subst_user_vars_in_expr(session, &c.expr));
            ExprNode::Cast(c)
        }
        ExprNode::InList(i) => {
            let mut i = i.clone();
            i.expr = Box::new(subst_user_vars_in_expr(session, &i.expr));
            i.list = i
                .list
                .iter()
                .map(|a| subst_user_vars_in_expr(session, a))
                .collect();
            ExprNode::InList(i)
        }
        ExprNode::Between(b) => {
            let mut b = b.clone();
            b.expr = Box::new(subst_user_vars_in_expr(session, &b.expr));
            b.low = Box::new(subst_user_vars_in_expr(session, &b.low));
            b.high = Box::new(subst_user_vars_in_expr(session, &b.high));
            ExprNode::Between(b)
        }
        ExprNode::Like(l) => {
            let mut l = l.clone();
            l.expr = Box::new(subst_user_vars_in_expr(session, &l.expr));
            l.pattern = Box::new(subst_user_vars_in_expr(session, &l.pattern));
            ExprNode::Like(l)
        }
        ExprNode::IsNull(i) => {
            let mut i = i.clone();
            i.expr = Box::new(subst_user_vars_in_expr(session, &i.expr));
            ExprNode::IsNull(i)
        }
        other => other.clone(),
    }
}

/// TiDB `errctx` level for `ERROR_FOR_DIVISION_BY_ZERO` per statement kind:
/// SELECT always warns; DML/DDL error only under strict mode with the flag set
/// (`ResetUpdateStmtCtx` / insert path).
fn div_by_zero_mode(
    sql_mode: &str,
    stmt: &sealdb_parser::ast::Statement,
) -> sealdb_expression::DivByZeroMode {
    let flag = |name: &str| {
        sql_mode
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case(name))
    };
    let strict = flag("STRICT_TRANS_TABLES") || flag("STRICT_ALL_TABLES");
    let dml_or_ddl = matches!(
        stmt,
        sealdb_parser::ast::Statement::Insert(_)
            | sealdb_parser::ast::Statement::Update(_)
            | sealdb_parser::ast::Statement::Delete(_)
            | sealdb_parser::ast::Statement::CreateTable(_)
            | sealdb_parser::ast::Statement::AlterTable(_)
            | sealdb_parser::ast::Statement::CreateIndex(_)
    );
    if !dml_or_ddl {
        return sealdb_expression::DivByZeroMode::Warn;
    }
    if !flag("ERROR_FOR_DIVISION_BY_ZERO") {
        sealdb_expression::DivByZeroMode::Ignore
    } else if strict {
        sealdb_expression::DivByZeroMode::Error
    } else {
        sealdb_expression::DivByZeroMode::Warn
    }
}

fn apply_set(
    session: &Session,
    stmt: &sealdb_parser::ast::SetStmt,
) -> Result<Vec<(i32, String)>> {
    let mut warnings = Vec::new();
    for a in &stmt.assignments {
        if is_user_var_name(&a.name) {
            if a.scope == SetVarScope::Global {
                return Err(sealdb_errno::Error::unsupported(
                    "SET GLOBAL user variables",
                ));
            }
            let key = user_var_key(&a.name);
            let datum = parse_set_literal(&a.value);
            session
                .user_vars
                .lock()
                .expect("session user vars lock")
                .insert(key, datum);
            continue;
        }
        let name = a.name.trim_start_matches('@').to_ascii_lowercase();
        let value = a.value.trim_matches(|c| c == '\'' || c == '"').to_string();
        if a.scope == SetVarScope::Global {
            let mut global_warnings = apply_set_global(session, &name, &value)?;
            warnings.append(&mut global_warnings);
            continue;
        }
        match name.as_str() {
            "names"
            | "character_set_client"
            | "character_set_connection"
            | "character_set_results" => {
                // SET NAMES <charset> resets collation_connection to the charset default
                // (`gbk`→`gbk_bin`, `latin1`→`latin1_bin`). An explicit COLLATE assignment
                // that follows in the same SET NAMES overrides this.
                if !sealdb_parser::charset::is_supported_charset(&value) {
                    return Err(sealdb_errno::Error::new(
                        sealdb_errno::ERR_UNKNOWN_CHARACTER_SET,
                        format!("Unknown character set: '{value}'"),
                    ));
                }
                *session.collation.lock().expect("session collation lock") =
                    Collation::default_for_charset(&value);
            }
            "collation_connection" | "collation_database" => {
                if !sealdb_parser::charset::is_supported_collation(&value) {
                    return Err(sealdb_errno::Error::new(
                        sealdb_errno::ERR_UNKNOWN_COLLATION,
                        format!("Unknown collation: '{value}'"),
                    ));
                }
                *session.collation.lock().expect("session collation lock") =
                    Collation::parse(&value);
            }
            "sql_mode" => {
                *session.sql_mode.lock().expect("session sql_mode lock") = value;
            }
            "tidb_partition_prune_mode" => {
                // `SET … = DEFAULT` resets to TiDB's default (dynamic).
                let raw = if value.eq_ignore_ascii_case("DEFAULT") {
                    "dynamic"
                } else {
                    value.as_str()
                };
                let mode = PartitionPruneMode::parse(raw).ok_or_else(|| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_WRONG_ARGUMENTS,
                        format!("Variable 'tidb_partition_prune_mode' can't be set to the value of '{value}'"),
                    )
                })?;
                let previous = *session
                    .partition_prune_mode
                    .lock()
                    .expect("session partition prune mode lock");
                *session
                    .partition_prune_mode
                    .lock()
                    .expect("session partition prune mode lock") = mode;
                // Prune mode changes plan shape; drop Domain-shared cached plans.
                session.domain.plan_cache().clear();
                // TiDB warns when a session overrides the prune mode: the
                // static alias is deprecated, dynamic asks for global scope and
                // for a re-ANALYZE of every partition table.
                if mode != previous {
                    match mode {
                        PartitionPruneMode::Static => warnings.push((
                            1681,
                            "static prune mode is deprecated and will be removed in the future release."
                                .to_string(),
                        )),
                        PartitionPruneMode::Dynamic => {
                            warnings.push((
                                1105,
                                "Please analyze all partition tables again for consistency between partition and global stats".to_string(),
                            ));
                            warnings.push((
                                1105,
                                "Please avoid setting partition prune mode to dynamic at session level and set partition prune mode to dynamic at global level".to_string(),
                            ));
                        }
                    }
                }
            }
            "tidb_enable_clustered_index" => {
                let mode = ClusteredIndexMode::parse(&value).ok_or_else(|| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_WRONG_ARGUMENTS,
                        format!(
                            "Variable 'tidb_enable_clustered_index' can't be set to the value of '{value}'"
                        ),
                    )
                })?;
                *session
                    .clustered_index
                    .lock()
                    .expect("session clustered index lock") = mode;
            }
            "tidb_enable_auto_analyze" => {
                let on = matches!(value.to_ascii_lowercase().as_str(), "1" | "on" | "true");
                *session
                    .enable_auto_analyze
                    .lock()
                    .expect("session auto analyze lock") = on;
            }
            "tidb_auto_analyze_ratio" => {
                let ratio: f64 = value.parse().map_err(|_| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_WRONG_ARGUMENTS,
                        format!(
                            "Variable 'tidb_auto_analyze_ratio' can't be set to the value of '{value}'"
                        ),
                    )
                })?;
                *session
                    .auto_analyze_ratio
                    .lock()
                    .expect("session auto analyze ratio lock") = ratio;
            }
            "autocommit" => {
                let on = if value.eq_ignore_ascii_case("DEFAULT") {
                    true
                } else {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "1" | "on" | "true"
                    )
                };
                *session.autocommit.lock().expect("session autocommit lock") = on;
            }
            "unique_checks" | "foreign_key_checks" => {}
            "cte_max_recursion_depth" => {
                // `SET … = DEFAULT` resets to TiDB's default (1000).
                let depth = if value.eq_ignore_ascii_case("DEFAULT") {
                    1000
                } else {
                    value.parse::<i64>().map_err(|_| {
                        sealdb_errno::Error::new(
                            sealdb_errno::ERR_WRONG_ARGUMENTS,
                            format!(
                                "Variable 'cte_max_recursion_depth' can't be set to the value of '{value}'"
                            ),
                        )
                    })?
                };
                sealdb_executor::cte::set_cte_max_recursion_depth(depth);
            }
            // Session time zone (DST / TIMESTAMP display). Value stored for later
            // conversion; offset literals like '+00:00' / named zones accepted.
            "time_zone" => {
                *session
                    .time_zone
                    .lock()
                    .expect("session time_zone lock") = value;
            }
            "tidb_window_concurrency" => {
                let n: u64 = value.parse().unwrap_or(sealdb_expression::DEFAULT_WINDOW_CONCURRENCY);
                *session
                    .window_concurrency
                    .lock()
                    .expect("session window_concurrency lock") = n.max(1);
            }
            "tidb_enable_window_function" => {}
            // Per-query memory quota (TiDB `tidb_mem_quota_query`): `0` = no
            // limit, `DEFAULT` = 1 GiB. Values are byte counts, possibly written
            // as an integer expression (`1 << 18`).
            "tidb_mem_quota_query" => {
                let quota = if value.eq_ignore_ascii_case("DEFAULT") {
                    sealdb_expression::memquota::DEFAULT_QUERY_MEM_QUOTA
                } else {
                    parse_set_int_expr(&value).ok_or_else(|| {
                        sealdb_errno::Error::new(
                            sealdb_errno::ERR_WRONG_ARGUMENTS,
                            format!(
                                "Variable 'tidb_mem_quota_query' can't be set to the value of '{value}'"
                            ),
                        )
                    })?
                };
                sealdb_expression::set_query_mem_quota(quota);
            }
            "tidb_mem_oom_action" => {
                let action = sealdb_expression::OomAction::parse(&value).ok_or_else(|| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_WRONG_ARGUMENTS,
                        format!(
                            "Variable 'tidb_mem_oom_action' can't be set to the value of '{value}'"
                        ),
                    )
                })?;
                sealdb_expression::set_query_mem_oom_action(action);
            }
            // Accepted for compatibility; SealDB always keeps intermediate data
            // in memory rather than spilling to local temporary storage.
            "tidb_enable_tmp_storage_on_oom" => {}
            // TiDB session vars used by integrationtest setup; accept as no-op
            // until the matching subsystem is wired (cost model, …).
            other if other.starts_with("tidb_") => {}
            other => {
                return Err(sealdb_errno::Error::unsupported(format!("SET {other}")));
            }
        }
    }
    Ok(warnings)
}

/// Domain-shared `SET GLOBAL` for the MVB variable set.
fn apply_set_global(session: &Session, name: &str, value: &str) -> Result<Vec<(i32, String)>> {
    let mut warnings = Vec::new();
    match name {
        "tidb_partition_prune_mode" => {
            // `SET GLOBAL … = DEFAULT` resets to TiDB's default (dynamic).
            let raw = if value.eq_ignore_ascii_case("DEFAULT") {
                "dynamic"
            } else {
                value
            };
            let mode = PartitionPruneMode::parse(raw).ok_or_else(|| {
                sealdb_errno::Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    format!(
                        "Variable 'tidb_partition_prune_mode' can't be set to the value of '{value}'"
                    ),
                )
            })?;
            session
                .domain
                .set_global_partition_prune_mode(mode.as_str().to_string());
            session.domain.plan_cache().clear();
            match mode {
                PartitionPruneMode::Static => warnings.push((
                    1681,
                    "static prune mode is deprecated and will be removed in the future release."
                        .to_string(),
                )),
                PartitionPruneMode::Dynamic => warnings.push((
                    1105,
                    "Please analyze all partition tables again for consistency between partition and global stats"
                        .to_string(),
                )),
            }
        }
        "sql_mode" => {
            session.domain.set_global_sql_mode(value.to_string());
        }
        "tidb_enable_auto_analyze" => {
            let on = matches!(value.to_ascii_lowercase().as_str(), "1" | "on" | "true");
            session.domain.set_global_enable_auto_analyze(on);
        }
        "names" | "character_set_client" | "character_set_connection" | "character_set_results" => {
            if !sealdb_parser::charset::is_supported_charset(value) {
                return Err(sealdb_errno::Error::new(
                    sealdb_errno::ERR_UNKNOWN_CHARACTER_SET,
                    format!("Unknown character set: '{value}'"),
                ));
            }
            session
                .domain
                .set_global_character_set_connection(value.to_string());
        }
        // System-wide memory OOM policy; SealDB tracks per session, so accept
        // the assignment without persisting a domain-wide default.
        "tidb_mem_oom_action" | "tidb_enable_tmp_storage_on_oom" => {}
        "tidb_mem_quota_query" => {}
        other => {
            return Err(sealdb_errno::Error::unsupported(format!(
                "SET GLOBAL {other}"
            )));
        }
    }
    Ok(warnings)
}

fn system_var_result(session: &Session, stmt: &Statement) -> Option<QueryResult> {
    let Statement::Select(select) = stmt else {
        return None;
    };
    if select.from.is_some() || !select.joins.is_empty() {
        return None;
    }
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for field in &select.fields {
        match &field.expr {
            ExprNode::Value(v) => {
                let name = field
                    .as_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        // TIMESTAMP/DATE/TIME literals: TiDB uses field.Text().
                        if matches!(v.datum, sealdb_types::Datum::Time(_)) {
                            if let Some(text) = field
                                .orig_text
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                return text.to_string();
                            }
                        }
                        literal_field_name(&v.datum)
                    });
                columns.push(name);
                row.push(v.datum.clone());
            }
            ExprNode::Column(col) => {
                columns.push(field.as_name.clone().unwrap_or_else(|| col.name.clone()));
                if is_user_var_name(&col.name) {
                    let key = user_var_key(&col.name);
                    let datum = session
                        .user_vars
                        .lock()
                        .expect("session user vars lock")
                        .get(&key)
                        .cloned()
                        .unwrap_or(sealdb_types::Datum::Null);
                    row.push(datum);
                    continue;
                }
                let global = col
                    .table
                    .as_deref()
                    .map(|t| t.trim_start_matches('@').eq_ignore_ascii_case("global"))
                    .unwrap_or(false);
                let name = col.name.trim_start_matches('@').to_ascii_lowercase();
                let value = if global {
                    read_global_sysvar(session, &name)?
                } else {
                    read_session_sysvar(session, &name)?
                };
                row.push(sealdb_types::Datum::Bytes(value.into_bytes()));
            }
            // Binary / func mixes (e.g. `@b + "123"`) fall through to planner.
            _ => return None,
        }
    }
    if columns.is_empty() {
        return None;
    }
    Some(QueryResult {
        columns,
        rows: vec![row],
        warnings: Vec::new(),
        affected_rows: 0,
        info: String::new(),
    })
}

fn literal_field_name(datum: &sealdb_types::Datum) -> String {
    match datum {
        sealdb_types::Datum::I64(n) => n.to_string(),
        sealdb_types::Datum::U64(n) => n.to_string(),
        sealdb_types::Datum::F64(n) => n.to_string(),
        sealdb_types::Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        sealdb_types::Datum::Bit(b) => String::from_utf8_lossy(b).into_owned(),
        sealdb_types::Datum::Null => "NULL".into(),
        sealdb_types::Datum::Time(t) => t.to_string(),
        sealdb_types::Datum::Decimal(d) => d.to_string(),
        sealdb_types::Datum::Enum(e) => String::from_utf8_lossy(&e.label).into_owned(),
    }
}

fn read_session_sysvar(session: &Session, name: &str) -> Option<String> {
    Some(match name {
        "sql_mode" => session
            .sql_mode
            .lock()
            .expect("session sql_mode lock")
            .clone(),
        "time_zone" => session
            .time_zone
            .lock()
            .expect("session time_zone lock")
            .clone(),
        "collation_connection" => session
            .collation
            .lock()
            .expect("session collation lock")
            .name
            .clone(),
        "character_set_client"
        | "character_set_connection"
        | "character_set_results"
        | "character_set_database" => session
            .collation
            .lock()
            .expect("session collation lock")
            .charset
            .clone(),
        "tidb_partition_prune_mode" => session
            .partition_prune_mode
            .lock()
            .expect("session partition prune mode lock")
            .as_str()
            .to_string(),
        "tidb_enable_clustered_index" => session
            .clustered_index
            .lock()
            .expect("session clustered index lock")
            .as_str()
            .to_string(),
        "tidb_enable_auto_analyze" => {
            if *session
                .enable_auto_analyze
                .lock()
                .expect("session auto analyze lock")
            {
                "ON".into()
            } else {
                "OFF".into()
            }
        }
        "tidb_auto_analyze_ratio" => session
            .auto_analyze_ratio
            .lock()
            .expect("session auto analyze ratio lock")
            .to_string(),
        "autocommit" => {
            if *session.autocommit.lock().expect("session autocommit lock") {
                "1".into()
            } else {
                "0".into()
            }
        }
        "cte_max_recursion_depth" => sealdb_executor::cte::cte_max_recursion_depth().to_string(),
        "tidb_window_concurrency" => session
            .window_concurrency
            .lock()
            .expect("session window_concurrency lock")
            .to_string(),
        "tidb_enable_window_function" => "1".into(),
        // Common TiDB session vars referenced by integrationtest; defaults.
        other if other.starts_with("tidb_") => "0".into(),
        _ => return None,
    })
}

fn read_global_sysvar(session: &Session, name: &str) -> Option<String> {
    let globals = session.domain.global_vars();
    Some(match name {
        "sql_mode" => globals.sql_mode,
        "tidb_enable_auto_analyze" => {
            if globals.enable_auto_analyze {
                "ON".into()
            } else {
                "OFF".into()
            }
        }
        "character_set_client"
        | "character_set_connection"
        | "character_set_results"
        | "character_set_database"
        | "character_set_server" => globals.character_set_connection,
        "tidb_partition_prune_mode" => globals.partition_prune_mode.clone(),
        _ => return None,
    })
}

fn restore_sql(stmt: &Statement) -> String {
    let mut ctx = sealdb_parser::format::RestoreCtx::new();
    let _ = sealdb_parser::ast::Node::restore(stmt, &mut ctx);
    ctx.into_string()
}

fn bind_sql(sql: &str, params: &[ExprNode]) -> Result<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut next = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let quote = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'?' {
            let expr = params.get(next).ok_or_else(|| {
                sealdb_errno::Error::new(
                    sealdb_errno::ERR_WRONG_ARGUMENTS,
                    "Incorrect arguments to EXECUTE",
                )
            })?;
            next += 1;
            let mut ctx = sealdb_parser::format::RestoreCtx::new();
            expr.restore(&mut ctx)?;
            out.push_str(&ctx.into_string());
            i += 1;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    if next != params.len() {
        return Err(sealdb_errno::Error::new(
            sealdb_errno::ERR_WRONG_ARGUMENTS,
            "Incorrect arguments to EXECUTE",
        ));
    }
    Ok(out)
}

/// Releases Domain MDL for autocommit statements on all exit paths.
struct MdlGuard<'a> {
    domain: &'a Domain,
    session_id: u64,
    active: bool,
}

impl Drop for MdlGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.domain.mdl_unlock_session(self.session_id);
        }
    }
}

fn map_isolation(level: AstIsolation) -> IsolationLevel {
    match level {
        AstIsolation::ReadCommitted => IsolationLevel::ReadCommitted,
        AstIsolation::RepeatableRead => IsolationLevel::RepeatableRead,
        // MVB: SERIALIZABLE accepted; RR snapshot semantics.
        AstIsolation::Serializable => IsolationLevel::Serializable,
    }
}

fn table_key(schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        schema.to_ascii_lowercase(),
        table.to_ascii_lowercase()
    )
}

fn mdl_table_key(name: &TableName, current_db: &str) -> String {
    let (schema, table) = name.qualified(current_db);
    table_key(&schema, &table)
}

/// Tables that need shared (DML) or exclusive (DDL) metadata locks.
fn mdl_targets(stmt: &Statement, current_db: &str) -> (Vec<String>, bool) {
    match stmt {
        Statement::Insert(insert) => (vec![mdl_table_key(&insert.table, current_db)], false),
        Statement::Update(update) => {
            let mut keys = vec![mdl_table_key(&update.table, current_db)];
            if let Some(join) = &update.join {
                keys.push(mdl_table_key(&join.left, current_db));
                keys.push(mdl_table_key(&join.right, current_db));
            }
            (keys, false)
        }
        Statement::Delete(delete) => {
            let mut keys = if delete.tables.is_empty() {
                vec![mdl_table_key(&delete.table, current_db)]
            } else {
                delete
                    .tables
                    .iter()
                    .map(|t| mdl_table_key(t, current_db))
                    .collect()
            };
            if let Some(join) = &delete.join {
                keys.push(mdl_table_key(&join.left, current_db));
                keys.push(mdl_table_key(&join.right, current_db));
            }
            (keys, false)
        }
        Statement::Select(select) => {
            let mut keys = Vec::new();
            if let Some(from) = &select.from {
                keys.push(mdl_table_key(from, current_db));
            }
            for t in select.join_tables() {
                keys.push(mdl_table_key(t, current_db));
            }
            (keys, false)
        }
        Statement::CreateTable(create) => (vec![mdl_table_key(&create.table, current_db)], true),
        Statement::CreateView(create) => (vec![mdl_table_key(&create.view_name, current_db)], true),
        Statement::CreateSequence(create) => (vec![mdl_table_key(&create.name, current_db)], true),
        Statement::DropSequence(drop) => (
            drop.sequences
                .iter()
                .map(|t| mdl_table_key(t, current_db))
                .collect(),
            true,
        ),
        Statement::AlterSequence(alter) => (vec![mdl_table_key(&alter.name, current_db)], true),
        Statement::DropTable(drop) => (
            drop.tables
                .iter()
                .map(|t| mdl_table_key(t, current_db))
                .collect(),
            true,
        ),
        Statement::TruncateTable(truncate) => {
            (vec![mdl_table_key(&truncate.table, current_db)], true)
        }
        Statement::AlterTable(alter) => (vec![mdl_table_key(&alter.table, current_db)], true),
        Statement::CreateIndex(create) => (vec![mdl_table_key(&create.table, current_db)], true),
        Statement::DropIndex(drop) => (vec![mdl_table_key(&drop.table, current_db)], true),
        Statement::LoadData(load) => (vec![mdl_table_key(&load.table, current_db)], false),
        _ => (Vec::new(), false),
    }
}

fn is_write_stmt(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::CreateDatabase(_)
            | Statement::DropDatabase(_)
            | Statement::AlterDatabase(_)
            | Statement::CreateTable(_)
            | Statement::CreateView(_)
            | Statement::CreateSequence(_)
            | Statement::DropSequence(_)
            | Statement::AlterSequence(_)
            | Statement::DropTable(_)
            | Statement::TruncateTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::AlterTable(_)
            | Statement::LoadData(_)
            | Statement::Grant(_)
            | Statement::Revoke(_)
            | Statement::CreateRole(_)
            | Statement::DropRole(_)
            | Statement::CreateUser(_)
            | Statement::DropUser(_)
            | Statement::GrantRole(_)
            | Statement::RevokeRole(_)
            | Statement::SetPwd(_)
            | Statement::RenameUser(_)
            | Statement::LockTables(_)
            | Statement::CleanupTableLock(_)
    )
}

/// TiDB resolves a statement-level `WITH` inside every subquery of that
/// statement (`… WHERE EXISTS (SELECT * FROM cte1 …)` where `cte1` is the
/// outer CTE). Planning reads only `select.with`, so push the visible scope
/// into nested SELECTs. Inner CTEs keep precedence for shadowed names.
fn inherit_cte_scope(stmt: &mut Statement) {
    match stmt {
        Statement::Select(select) => {
            sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(select);
        }
        Statement::SetOpr(set) => {
            let scope = set.with.clone();
            if let Some(outer) = scope.as_ref() {
                sealdb_parser::ast::cte_scope::merge_cte_scope(&mut set.first, outer);
            }
            sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(&mut set.first);
            for (_, select) in &mut set.rest {
                if let Some(outer) = scope.as_ref() {
                    sealdb_parser::ast::cte_scope::merge_cte_scope(select, outer);
                }
                sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(select);
            }
        }
        Statement::Explain(explain) => inherit_cte_scope(&mut explain.stmt),
        Statement::Insert(insert) => {
            if let Some(select) = insert.select.as_mut() {
                if let Some(outer) = insert.with.as_ref() {
                    sealdb_parser::ast::cte_scope::merge_cte_scope(select, outer);
                }
                sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(select);
            }
        }
        Statement::Update(update) => {
            let scope = update.with.clone();
            if let Some(sel) = update.selection.as_mut() {
                inherit_cte_in_expr(sel, scope.as_ref());
            }
            if let Some(join) = update.join.as_mut() {
                inherit_cte_in_expr(&mut join.on, scope.as_ref());
            }
        }
        Statement::Delete(delete) => {
            let scope = delete.with.clone();
            if let Some(sel) = delete.selection.as_mut() {
                inherit_cte_in_expr(sel, scope.as_ref());
            }
            if let Some(join) = delete.join.as_mut() {
                inherit_cte_in_expr(&mut join.on, scope.as_ref());
            }
        }
        _ => {}
    }
}

fn inherit_cte_in_expr(expr: &mut ExprNode, scope: Option<&WithClause>) {
    let Some(scope) = scope else {
        return;
    };
    match expr {
        ExprNode::InSubquery(s) => {
            inherit_cte_in_expr(&mut s.expr, Some(scope));
            inherit_cte_in_result_set(&mut s.query, scope);
        }
        ExprNode::ScalarSubquery(s) => inherit_cte_in_result_set(&mut s.query, scope),
        ExprNode::Exists(e) => inherit_cte_in_result_set(&mut e.query, scope),
        ExprNode::QuantifiedCmp(q) => {
            inherit_cte_in_expr(&mut q.expr, Some(scope));
            inherit_cte_in_result_set(&mut q.query, scope);
        }
        ExprNode::Binary(b) => {
            inherit_cte_in_expr(&mut b.left, Some(scope));
            inherit_cte_in_expr(&mut b.right, Some(scope));
        }
        ExprNode::Unary(u) => inherit_cte_in_expr(&mut u.expr, Some(scope)),
        ExprNode::Func(f) => {
            for arg in &mut f.args {
                inherit_cte_in_expr(arg, Some(scope));
            }
        }
        ExprNode::Cast(c) => inherit_cte_in_expr(&mut c.expr, Some(scope)),
        ExprNode::InList(i) => {
            inherit_cte_in_expr(&mut i.expr, Some(scope));
            for item in &mut i.list {
                inherit_cte_in_expr(item, Some(scope));
            }
        }
        ExprNode::Between(b) => {
            inherit_cte_in_expr(&mut b.expr, Some(scope));
            inherit_cte_in_expr(&mut b.low, Some(scope));
            inherit_cte_in_expr(&mut b.high, Some(scope));
        }
        ExprNode::Like(l) => {
            inherit_cte_in_expr(&mut l.expr, Some(scope));
            inherit_cte_in_expr(&mut l.pattern, Some(scope));
        }
        ExprNode::IsNull(i) => inherit_cte_in_expr(&mut i.expr, Some(scope)),
        ExprNode::Case(c) => {
            if let Some(operand) = &mut c.operand {
                inherit_cte_in_expr(operand, Some(scope));
            }
            for (when, then) in &mut c.when_then {
                inherit_cte_in_expr(when, Some(scope));
                inherit_cte_in_expr(then, Some(scope));
            }
            if let Some(branch) = &mut c.else_expr {
                inherit_cte_in_expr(branch, Some(scope));
            }
        }
        ExprNode::Window(w) => {
            for arg in &mut w.args {
                inherit_cte_in_expr(arg, Some(scope));
            }
        }
        ExprNode::Row(r) => {
            for expr in &mut r.exprs {
                inherit_cte_in_expr(expr, Some(scope));
            }
        }
        ExprNode::Column(_) | ExprNode::Value(_) => {}
    }
}

fn inherit_cte_in_result_set(query: &mut sealdb_parser::ast::ResultSetNode, scope: &WithClause) {
    match query {
        sealdb_parser::ast::ResultSetNode::Select(select) => {
            sealdb_parser::ast::cte_scope::merge_cte_scope(select, scope);
            sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(select);
        }
        sealdb_parser::ast::ResultSetNode::SetOpr(set) => {
            let merged = match set.with.take() {
                Some(mut inner) => {
                    let mut outer = scope.clone();
                    outer.ctes.extend(inner.ctes.drain(..));
                    outer.is_recursive |= inner.is_recursive;
                    outer
                }
                None => scope.clone(),
            };
            set.with = Some(merged);
            let merged = set.with.clone().unwrap_or_else(|| scope.clone());
            sealdb_parser::ast::cte_scope::merge_cte_scope(&mut set.first, &merged);
            sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(&mut set.first);
            for (_, select) in &mut set.rest {
                sealdb_parser::ast::cte_scope::merge_cte_scope(select, &merged);
                sealdb_parser::ast::cte_scope::inherit_cte_scope_in_select(select);
            }
        }
    }
}

/// Expand VIEW references into CTEs so planner/executor reuse derived-table paths.
fn expand_views(stmt: &mut Statement, is: &InfoSchema, current_db: &str) -> Result<()> {
    match stmt {
        Statement::Select(select) => expand_views_in_select(select, is, current_db)?,
        Statement::Insert(insert) => {
            if let Some(select) = insert.select.as_mut() {
                expand_views_in_select(select, is, current_db)?;
            }
        }
        Statement::Update(update) => {
            if update.join.is_none() {
                rewrite_updatable_view_target(
                    &mut update.table,
                    &mut update.selection,
                    &mut update.check_option_where,
                    &mut update.check_option_view,
                    is,
                    current_db,
                )?;
            }
        }
        Statement::Delete(delete) => {
            if delete.join.is_none() && delete.tables.is_empty() {
                let mut unused_check = None;
                let mut unused_view = None;
                rewrite_updatable_view_target(
                    &mut delete.table,
                    &mut delete.selection,
                    &mut unused_check,
                    &mut unused_view,
                    is,
                    current_db,
                )?;
            }
        }
        Statement::SetOpr(set) => {
            expand_views_in_select(&mut set.first, is, current_db)?;
            for (_, select) in &mut set.rest {
                expand_views_in_select(select, is, current_db)?;
            }
        }
        Statement::Explain(explain) => expand_views(&mut explain.stmt, is, current_db)?,
        Statement::CreateView(create) => {
            expand_views_in_select(&mut create.select, is, current_db)?;
        }
        Statement::CreateTable(create) => {
            if let Some(select) = create.select.as_mut() {
                expand_views_in_select(select, is, current_db)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Simple updatable VIEW: single-table SELECT (no JOIN/GROUP BY). Rewrite the
/// DML target to the base table and AND the view WHERE into the statement.
/// When the view has `WITH CHECK OPTION`, stash the predicate for post-update
/// validation (`check_option_where`).
fn rewrite_updatable_view_target(
    table: &mut TableName,
    selection: &mut Option<ExprNode>,
    check_option_where: &mut Option<ExprNode>,
    check_option_view: &mut Option<String>,
    is: &InfoSchema,
    current_db: &str,
) -> Result<()> {
    use sealdb_parser::ast::expressions::BinaryOpExpr;
    use sealdb_parser::opcode::Op;

    let (schema, name) = table.qualified(current_db);
    if schema.is_empty() {
        return Ok(());
    }
    let Some(info) = is.table_by_name(&schema, &name) else {
        return Ok(());
    };
    let Some(view) = &info.view else {
        return Ok(());
    };
    if view.algorithm.eq_ignore_ascii_case("TEMPTABLE") {
        return Err(sealdb_errno::Error::unsupported(format!(
            "VIEW `{schema}`.`{name}` ALGORITHM=TEMPTABLE is not updatable"
        )));
    }
    let parsed = Parser::new().parse_one_stmt(&view.select_stmt)?;
    let Statement::Select(view_select) = parsed else {
        return Err(sealdb_errno::Error::unsupported(
            "VIEW definition must be SELECT",
        ));
    };
    if !view_select.joins.is_empty()
        || view_select.from.is_none()
        || !view_select.group_by.is_empty()
        || view_select.having.is_some()
        || view_select.with.is_some()
    {
        return Err(sealdb_errno::Error::unsupported(format!(
            "VIEW `{schema}`.`{name}` is not updatable"
        )));
    }
    let base = view_select.from.expect("checked above");
    let enforce_check =
        !view.check_option.is_empty() && !view.check_option.eq_ignore_ascii_case("NONE");
    if let Some(view_where) = view_select.selection {
        if enforce_check {
            *check_option_where = Some(view_where.clone());
            *check_option_view = Some(format!("{schema}.{name}"));
        }
        *selection = Some(match selection.take() {
            Some(user_where) => ExprNode::Binary(BinaryOpExpr {
                op: Op::LogicAnd,
                left: Box::new(view_where),
                right: Box::new(user_where),
                single_cnf_item: false,
            }),
            None => view_where,
        });
    }
    *table = base;
    Ok(())
}

fn expand_views_in_select(
    select: &mut SelectStmt,
    is: &InfoSchema,
    current_db: &str,
) -> Result<()> {
    if let Some(with) = select.with.as_mut() {
        for cte in &mut with.ctes {
            expand_views_in_select(&mut cte.query, is, current_db)?;
            for (_, arm) in &mut cte.set_rest {
                expand_views_in_select(arm, is, current_db)?;
            }
            if let Some((_, rec)) = cte.recursive.as_mut() {
                expand_views_in_select(rec, is, current_db)?;
            }
        }
    }
    let refs: Vec<TableName> = select.join_tables().into_iter().cloned().collect();
    for table in refs {
        inject_view_cte(select, &table, is, current_db)?;
    }
    Ok(())
}

fn inject_view_cte(
    select: &mut SelectStmt,
    table: &TableName,
    is: &InfoSchema,
    current_db: &str,
) -> Result<()> {
    let (schema, name) = table.qualified(current_db);
    if schema.is_empty() {
        return Ok(());
    }
    if named_cte_exists(select.with.as_ref(), &name) {
        return Ok(());
    }
    let Some(info) = is.table_by_name(&schema, &name) else {
        return Ok(());
    };
    let Some(view) = &info.view else {
        return Ok(());
    };
    let parsed = Parser::new().parse_one_stmt(&view.select_stmt)?;
    let Statement::Select(mut view_select) = parsed else {
        return Err(sealdb_errno::Error::unsupported(
            "VIEW definition must be SELECT",
        ));
    };
    // Unqualified base tables in the view body resolve in the view's schema
    // (`USE cte1; SELECT * FROM cte.v1` still reads cte.t2, not cte1.t2).
    qualify_view_body_tables(&mut view_select, &schema);
    expand_views_in_select(&mut view_select, is, &schema)?;
    // TiDB MERGE / UNDEFINED: inline simple single-table views.
    let view_col_names: Vec<String> = view.cols.iter().map(|c| c.o().to_string()).collect();
    if !view.algorithm.eq_ignore_ascii_case("TEMPTABLE")
        && try_merge_simple_view(select, &name, &view_select, &view_col_names)?
    {
        return Ok(());
    }
    let materialize = if view.algorithm.eq_ignore_ascii_case("TEMPTABLE") {
        CteMaterialize::Materialized
    } else if view.algorithm.eq_ignore_ascii_case("MERGE") {
        CteMaterialize::NotMaterialized
    } else {
        CteMaterialize::Default
    };
    // TiDB names unnamed select-list exprs `name_exp_N` for view/derived output.
    // Leave empty for `SELECT *` / plain columns so CTE inherits expanded seed names.
    let is_star_field = |f: &sealdb_parser::ast::SelectField| {
        matches!(
            &f.expr,
            sealdb_parser::ast::expressions::ExprNode::Value(
                sealdb_parser::ast::expressions::ValueExpr {
                    datum: sealdb_types::Datum::Bytes(b)
                }
            ) if b == b"*"
        )
    };
    let has_star = view_select.fields.iter().any(is_star_field);
    let needs_name_exp = !has_star
        && view_select.fields.iter().any(|f| {
            f.as_name.as_ref().map(|s| s.is_empty()).unwrap_or(true)
                && !matches!(
                    &f.expr,
                    sealdb_parser::ast::expressions::ExprNode::Column(_)
                )
        });
    let col_name_list = if !view_col_names.is_empty() {
        view_col_names
    } else if needs_name_exp {
        view_select
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if let Some(name) = &f.as_name
                    && !name.is_empty()
                {
                    return name.clone();
                }
                match &f.expr {
                    sealdb_parser::ast::expressions::ExprNode::Column(c) => c.name.clone(),
                    _ => format!("name_exp_{}", i + 1),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut ctes = select.with.take().map(|w| w.ctes).unwrap_or_default();
    ctes.push(CommonTableExpression {
        name: name.clone(),
        query: Box::new(view_select),
        col_name_list,
        recursive: None,
        recursive_rest: Vec::new(),
        set_rest: Vec::new(),
        materialize,
        limit: None,
        offset: None,
    });
    select.with = Some(sealdb_parser::ast::WithClause {
        is_recursive: false,
        ctes,
    });
    // CTE names are unqualified; strip schema so planner hits the injected CTE.
    strip_table_schema(select, &name);
    Ok(())
}

/// Attach `schema` to unqualified table refs that are not CTE names in scope.
fn qualify_view_body_tables(select: &mut SelectStmt, schema: &str) {
    if schema.is_empty() {
        return;
    }
    let mut cte_names = Vec::new();
    if let Some(with) = select.with.as_mut() {
        for cte in &mut with.ctes {
            // Nested WITH bodies see only preceding sibling CTE names.
            qualify_select_tables(cte.query.as_mut(), schema, &cte_names);
            for (_, arm) in &mut cte.set_rest {
                qualify_select_tables(arm, schema, &cte_names);
            }
            if let Some((_, rec)) = cte.recursive.as_mut() {
                // Recursive arm may self-ref this CTE.
                let mut with_self = cte_names.clone();
                with_self.push(cte.name.clone());
                qualify_select_tables(rec, schema, &with_self);
            }
            cte_names.push(cte.name.clone());
        }
    }
    qualify_select_tables(select, schema, &cte_names);
}

fn qualify_select_tables(select: &mut SelectStmt, schema: &str, cte_names: &[String]) {
    let qualify = |table: &mut TableName| {
        if table.schema.is_some() {
            return;
        }
        if table.name.eq_ignore_ascii_case("dual") {
            return;
        }
        if cte_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(table.resolve_name()))
        {
            return;
        }
        table.schema = Some(schema.to_string());
    };
    if let Some(from) = select.from.as_mut() {
        qualify(from);
    }
    for join in &mut select.joins {
        qualify(&mut join.left);
        qualify(&mut join.right);
    }
    for field in &mut select.fields {
        qualify_tables_in_expr(&mut field.expr, schema, cte_names);
    }
    if let Some(sel) = select.selection.as_mut() {
        qualify_tables_in_expr(sel, schema, cte_names);
    }
}

fn qualify_tables_in_expr(
    expr: &mut sealdb_parser::ast::expressions::ExprNode,
    schema: &str,
    cte_names: &[String],
) {
    use sealdb_parser::ast::expressions::ExprNode;
    match expr {
        ExprNode::ScalarSubquery(s) => qualify_tables_in_result_set(&mut s.query, schema, cte_names),
        ExprNode::Exists(e) => qualify_tables_in_result_set(&mut e.query, schema, cte_names),
        ExprNode::InSubquery(s) => {
            qualify_tables_in_expr(&mut s.expr, schema, cte_names);
            qualify_tables_in_result_set(&mut s.query, schema, cte_names);
        }
        ExprNode::QuantifiedCmp(q) => {
            qualify_tables_in_expr(&mut q.expr, schema, cte_names);
            qualify_tables_in_result_set(&mut q.query, schema, cte_names);
        }
        ExprNode::Binary(b) => {
            qualify_tables_in_expr(&mut b.left, schema, cte_names);
            qualify_tables_in_expr(&mut b.right, schema, cte_names);
        }
        ExprNode::Unary(u) => qualify_tables_in_expr(&mut u.expr, schema, cte_names),
        ExprNode::Func(f) => {
            for arg in &mut f.args {
                qualify_tables_in_expr(arg, schema, cte_names);
            }
        }
        ExprNode::Cast(c) => qualify_tables_in_expr(&mut c.expr, schema, cte_names),
        ExprNode::InList(i) => {
            qualify_tables_in_expr(&mut i.expr, schema, cte_names);
            for item in &mut i.list {
                qualify_tables_in_expr(item, schema, cte_names);
            }
        }
        ExprNode::Between(b) => {
            qualify_tables_in_expr(&mut b.expr, schema, cte_names);
            qualify_tables_in_expr(&mut b.low, schema, cte_names);
            qualify_tables_in_expr(&mut b.high, schema, cte_names);
        }
        ExprNode::Like(l) => {
            qualify_tables_in_expr(&mut l.expr, schema, cte_names);
            qualify_tables_in_expr(&mut l.pattern, schema, cte_names);
        }
        ExprNode::IsNull(i) => qualify_tables_in_expr(&mut i.expr, schema, cte_names),
        ExprNode::Case(c) => {
            if let Some(operand) = &mut c.operand {
                qualify_tables_in_expr(operand, schema, cte_names);
            }
            for (when, then) in &mut c.when_then {
                qualify_tables_in_expr(when, schema, cte_names);
                qualify_tables_in_expr(then, schema, cte_names);
            }
            if let Some(branch) = &mut c.else_expr {
                qualify_tables_in_expr(branch, schema, cte_names);
            }
        }
        ExprNode::Window(w) => {
            for arg in &mut w.args {
                qualify_tables_in_expr(arg, schema, cte_names);
            }
        }
        ExprNode::Row(r) => {
            for expr in &mut r.exprs {
                qualify_tables_in_expr(expr, schema, cte_names);
            }
        }
        ExprNode::Column(_) | ExprNode::Value(_) => {}
    }
}

fn qualify_tables_in_result_set(
    query: &mut sealdb_parser::ast::ResultSetNode,
    schema: &str,
    cte_names: &[String],
) {
    match query {
        sealdb_parser::ast::ResultSetNode::Select(select) => {
            // Nested WITH on a scalar subquery: qualify bodies, then the select.
            let mut nested = cte_names.to_vec();
            if let Some(with) = select.with.as_mut() {
                for cte in &mut with.ctes {
                    qualify_select_tables(cte.query.as_mut(), schema, &nested);
                    for (_, arm) in &mut cte.set_rest {
                        qualify_select_tables(arm, schema, &nested);
                    }
                    if let Some((_, rec)) = cte.recursive.as_mut() {
                        let mut with_self = nested.clone();
                        with_self.push(cte.name.clone());
                        qualify_select_tables(rec, schema, &with_self);
                    }
                    nested.push(cte.name.clone());
                }
            }
            qualify_select_tables(select, schema, &nested);
        }
        sealdb_parser::ast::ResultSetNode::SetOpr(set) => {
            qualify_select_tables(&mut set.first, schema, cte_names);
            for (_, arm) in &mut set.rest {
                qualify_select_tables(arm, schema, cte_names);
            }
        }
    }
}

/// Merge a simple VIEW into the referencing SELECT (TiDB `algorithm=MERGE`).
fn try_merge_simple_view(
    select: &mut SelectStmt,
    view_name: &str,
    view_select: &SelectStmt,
    view_cols: &[String],
) -> Result<bool> {
    use sealdb_parser::ast::expressions::BinaryOpExpr;
    use sealdb_parser::opcode::Op;
    use std::collections::HashMap;

    if !view_select.joins.is_empty()
        || view_select.from.is_none()
        || !view_select.group_by.is_empty()
        || view_select.having.is_some()
        || view_select.with.is_some()
        || view_select.distinct
        || view_select.limit.is_some()
        || !view_select.order_by.is_empty()
        // Scalar/WITH subqueries in the select list need CTE materialization so
        // `SELECT * FROM view` does not collapse to the base table's columns
        // (cte suite: name_exp_1 / 30 vs bare `a` / 1).
        || view_select.fields.iter().any(|f| f.expr.has_subquery())
    {
        return Ok(false);
    }
    let base = view_select.from.clone().expect("checked above");
    let col_names: Vec<String> = if view_cols.is_empty() {
        view_select
            .fields
            .iter()
            .map(|f| {
                f.as_name
                    .clone()
                    .or_else(|| f.orig_text.clone())
                    .unwrap_or_else(|| match &f.expr {
                        sealdb_parser::ast::expressions::ExprNode::Column(c) => c.name.clone(),
                        _ => String::new(),
                    })
            })
            .collect()
    } else {
        view_cols.to_vec()
    };
    if col_names.len() != view_select.fields.len() || col_names.iter().any(|n| n.is_empty()) {
        return Ok(false);
    }
    let mapping: HashMap<String, sealdb_parser::ast::expressions::ExprNode> = col_names
        .iter()
        .zip(view_select.fields.iter())
        .map(|(n, f)| (n.to_ascii_lowercase(), f.expr.clone()))
        .collect();

    let replace_table = |t: &mut TableName| {
        if t.name.eq_ignore_ascii_case(view_name)
            || t.resolve_name().eq_ignore_ascii_case(view_name)
        {
            // Column refs are rewritten to view exprs; expose the base table
            // name so EXPLAIN / DistSQL use `table:t1` (TiDB MERGE).
            *t = base.clone();
            true
        } else {
            false
        }
    };
    let mut replaced = false;
    if let Some(from) = select.from.as_mut() {
        replaced |= replace_table(from);
    }
    for join in &mut select.joins {
        replaced |= replace_table(&mut join.left);
        replaced |= replace_table(&mut join.right);
    }
    if !replaced {
        return Ok(false);
    }

    for field in &mut select.fields {
        rewrite_merged_view_cols(&mut field.expr, view_name, &mapping);
    }
    if let Some(sel) = select.selection.as_mut() {
        rewrite_merged_view_cols(sel, view_name, &mapping);
    }
    for join in &mut select.joins {
        rewrite_merged_view_cols(&mut join.on, view_name, &mapping);
    }
    if let Some(view_where) = &view_select.selection {
        select.selection = Some(match select.selection.take() {
            Some(user) => sealdb_parser::ast::expressions::ExprNode::Binary(BinaryOpExpr {
                op: Op::LogicAnd,
                left: Box::new(view_where.clone()),
                right: Box::new(user),
                single_cnf_item: false,
            }),
            None => view_where.clone(),
        });
    }
    Ok(true)
}

fn rewrite_merged_view_cols(
    expr: &mut sealdb_parser::ast::expressions::ExprNode,
    view_name: &str,
    mapping: &std::collections::HashMap<String, sealdb_parser::ast::expressions::ExprNode>,
) {
    use sealdb_parser::ast::expressions::ExprNode;
    match expr {
        ExprNode::Column(c) => {
            let from_view = c
                .table
                .as_deref()
                .is_none_or(|t| t.eq_ignore_ascii_case(view_name));
            if from_view
                && let Some(mapped) = mapping.get(&c.name.to_ascii_lowercase())
            {
                *expr = mapped.clone();
            }
        }
        ExprNode::Binary(b) => {
            rewrite_merged_view_cols(&mut b.left, view_name, mapping);
            rewrite_merged_view_cols(&mut b.right, view_name, mapping);
        }
        ExprNode::Unary(u) => rewrite_merged_view_cols(&mut u.expr, view_name, mapping),
        ExprNode::Func(f) => {
            for arg in &mut f.args {
                rewrite_merged_view_cols(arg, view_name, mapping);
            }
        }
        ExprNode::Cast(c) => rewrite_merged_view_cols(&mut c.expr, view_name, mapping),
        ExprNode::InList(i) => {
            rewrite_merged_view_cols(&mut i.expr, view_name, mapping);
            for e in &mut i.list {
                rewrite_merged_view_cols(e, view_name, mapping);
            }
        }
        ExprNode::Between(b) => {
            rewrite_merged_view_cols(&mut b.expr, view_name, mapping);
            rewrite_merged_view_cols(&mut b.low, view_name, mapping);
            rewrite_merged_view_cols(&mut b.high, view_name, mapping);
        }
        ExprNode::Like(l) => {
            rewrite_merged_view_cols(&mut l.expr, view_name, mapping);
            rewrite_merged_view_cols(&mut l.pattern, view_name, mapping);
        }
        ExprNode::IsNull(i) => rewrite_merged_view_cols(&mut i.expr, view_name, mapping),
        ExprNode::Case(c) => {
            if let Some(operand) = &mut c.operand {
                rewrite_merged_view_cols(operand, view_name, mapping);
            }
            for (when, then) in &mut c.when_then {
                rewrite_merged_view_cols(when, view_name, mapping);
                rewrite_merged_view_cols(then, view_name, mapping);
            }
            if let Some(branch) = &mut c.else_expr {
                rewrite_merged_view_cols(branch, view_name, mapping);
            }
        }
        ExprNode::Window(w) => {
            for arg in &mut w.args {
                rewrite_merged_view_cols(arg, view_name, mapping);
            }
            for p in &mut w.spec.partition_by {
                rewrite_merged_view_cols(p, view_name, mapping);
            }
            for o in &mut w.spec.order_by {
                rewrite_merged_view_cols(&mut o.expr, view_name, mapping);
            }
        }
        ExprNode::Row(r) => {
            for expr in &mut r.exprs {
                rewrite_merged_view_cols(expr, view_name, mapping);
            }
        }
        ExprNode::Value(_)
        | ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_) => {}
    }
}

fn strip_table_schema(select: &mut SelectStmt, name: &str) {
    if let Some(from) = select.from.as_mut()
        && from.name.eq_ignore_ascii_case(name)
    {
        from.schema = None;
    }
    for join in &mut select.joins {
        if join.left.name.eq_ignore_ascii_case(name) {
            join.left.schema = None;
        }
        if join.right.name.eq_ignore_ascii_case(name) {
            join.right.schema = None;
        }
    }
}

fn named_cte_exists(with: Option<&sealdb_parser::ast::WithClause>, name: &str) -> bool {
    with.is_some_and(|w| w.find(name).is_some())
}

fn load_data_to_insert(
    load: &sealdb_parser::ast::LoadDataStmt,
    is: &InfoSchema,
    current_db: &str,
) -> Result<InsertStmt> {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    let (schema, table) = load.table.qualified(current_db);
    if schema.is_empty() {
        return Err(sealdb_errno::Error::new(
            sealdb_errno::ERR_NO_DB,
            "No database selected",
        ));
    }
    let info = is.table_by_name(&schema, &table).ok_or_else(|| {
        sealdb_errno::Error::new(
            sealdb_errno::ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    if info.is_view() || info.is_sequence() {
        return Err(sealdb_errno::Error::new(
            sealdb_errno::ERR_WRONG_OBJECT,
            format!("'{schema}.{table}' is not BASE TABLE"),
        ));
    }
    let content = std::fs::read_to_string(&load.path).map_err(|e| {
        sealdb_errno::Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("failed to read '{}': {e}", load.path),
        )
    })?;
    let line_sep = &load.lines_terminated_by;
    let field_sep = &load.fields_terminated_by;
    let mut lines: Vec<&str> = if line_sep.is_empty() {
        vec![content.as_str()]
    } else {
        content.split(line_sep).collect()
    };
    // Drop trailing empty line from final terminator.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let skip = load.ignore_lines as usize;
    let lines = if skip >= lines.len() {
        Vec::new()
    } else {
        lines[skip..].to_vec()
    };
    let columns = if load.columns.is_empty() {
        info.columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect()
    } else {
        load.columns.clone()
    };
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = if field_sep.is_empty() {
            vec![line]
        } else {
            line.split(field_sep).collect()
        };
        if fields.len() != columns.len() {
            return Err(sealdb_errno::Error::new(
                sealdb_errno::ERR_WRONG_VALUE_COUNT,
                format!(
                    "Column count doesn't match value count at row {}",
                    rows.len() + 1
                ),
            ));
        }
        let mut row = Vec::with_capacity(fields.len());
        for (i, field) in fields.iter().enumerate() {
            let col = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(&columns[i]))
                .ok_or_else(|| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_BAD_FIELD,
                        format!("Unknown column '{}' in 'field list'", columns[i]),
                    )
                })?;
            let datum = parse_load_field(field, col.field_type.eval_type())?;
            row.push(ExprNode::Value(ValueExpr { datum }));
        }
        rows.push(row);
    }
    Ok(InsertStmt {
        table: load.table.clone(),
        ignore: false,
        replace: false,
        partitions: Vec::new(),
        columns,
        rows,
        select: None,
        select_set: None,
        with: None,
        on_duplicate: Vec::new(),
    })
}

fn parse_load_field(raw: &str, eval_type: sealdb_types::EvalType) -> Result<sealdb_types::Datum> {
    use sealdb_types::{Datum, EvalType};
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("NULL") || trimmed.is_empty() {
        return Ok(Datum::Null);
    }
    match eval_type {
        EvalType::Int => trimmed.parse::<i64>().map(Datum::I64).map_err(|_| {
            sealdb_errno::Error::new(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                format!("Incorrect integer value: '{trimmed}'"),
            )
        }),
        EvalType::Real => trimmed.parse::<f64>().map(Datum::F64).map_err(|_| {
            sealdb_errno::Error::new(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                format!("Incorrect double value: '{trimmed}'"),
            )
        }),
        _ => Ok(Datum::Bytes(trimmed.as_bytes().to_vec())),
    }
}

/// MVB: `SELECT NEXTVAL(seq)` / `SELECT NEXTVAL(db.seq)` without FROM.
fn try_eval_nextval(
    session: &Session,
    stmt: &Statement,
    current_db: &str,
) -> Result<Option<QueryResult>> {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    let Statement::Select(select) = stmt else {
        return Ok(None);
    };
    if select.from.is_some()
        || !select.joins.is_empty()
        || select.fields.len() != 1
        || select.selection.is_some()
        || select.with.is_some()
    {
        return Ok(None);
    }
    let ExprNode::Func(func) = &select.fields[0].expr else {
        return Ok(None);
    };
    if !func.name.eq_ignore_ascii_case("NEXTVAL") || func.args.len() != 1 {
        return Ok(None);
    }
    let (schema, name) = match &func.args[0] {
        ExprNode::Column(col) => {
            // `db.seq` → schema+name; `seq` → current_db+name; `db.t.col` unsupported.
            if let Some(schema) = &col.schema {
                (schema.clone(), col.name.clone())
            } else if let Some(table) = &col.table {
                (table.clone(), col.name.clone())
            } else {
                (current_db.to_string(), col.name.clone())
            }
        }
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => {
            let raw = String::from_utf8_lossy(b);
            if let Some((s, n)) = raw.split_once('.') {
                (s.to_string(), n.to_string())
            } else {
                (current_db.to_string(), raw.to_string())
            }
        }
        _ => return Ok(None),
    };
    if schema.is_empty() {
        return Err(sealdb_errno::Error::new(
            sealdb_errno::ERR_NO_DB,
            "No database selected",
        ));
    }
    let value = session.domain.next_sequence_value(&schema, &name)?;
    let col_name = select.fields[0]
        .as_name
        .clone()
        .unwrap_or_else(|| "NEXTVAL(...)".to_string());
    Ok(Some(QueryResult {
        columns: vec![col_name],
        rows: vec![vec![Datum::I64(value)]],
        warnings: Vec::new(),
        affected_rows: 0,
        info: String::new(),
    }))
}

impl Session {
    /// Shared Domain (tests / SHUTDOWN flag).
    pub fn domain(&self) -> Arc<Domain> {
        Arc::clone(&self.domain)
    }

    /// Domain-local session / connection id (MDL + KILL + LOCK TABLES).
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// If a Domain binding matches this statement's restored SQL, re-parse the bind SQL.
    fn apply_sql_binding(&self, stmt: &Statement) -> Result<Option<Statement>> {
        match stmt {
            Statement::Select(_) => {
                let restored = restore_sql(stmt);
                if let Some(binding) = self.domain.bind_handle().match_binding(&restored) {
                    return Ok(Some(self.parser.parse_one_stmt(&binding.bind_sql)?));
                }
            }
            Statement::Explain(explain) => {
                if let Some(inner) = self.apply_sql_binding(&explain.stmt)? {
                    let mut explain = explain.clone();
                    explain.stmt = Box::new(inner);
                    return Ok(Some(Statement::Explain(explain)));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn in_read_only(&self) -> bool {
        let (explicit, txn_ro) = {
            let txn = self.txn.lock().expect("session txn lock");
            (txn.is_explicit(), txn.is_read_only())
        };
        if explicit {
            txn_ro
        } else {
            *self.read_only.lock().expect("session read_only lock")
        }
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
    use sealdb_errno::{
        ERR_DATA_OUT_OF_RANGE, ERR_DB_CREATE_EXISTS, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS,
    };
    use sealdb_types::Datum;

    #[test]
    fn test_timestamp_literal_header() {
        let session = Session::new();
        let ok = session
            .execute_stmt("SELECT TIMESTAMP '2024-01-01 14:00:00'")
            .unwrap();
        assert_eq!(ok.columns[0], "TIMESTAMP '2024-01-01 14:00:00'", "{:?}", ok.columns);
        assert_eq!(ok.rows[0][0].display(), "2024-01-01 14:00:00");
    }

    #[test]
    fn test_date_add_timestampadd_strings() {
        let session = Session::new();
        session.execute_stmt("SET time_zone='Europe/Paris'").unwrap();
        let ok = session
            .execute_stmt("SELECT DATE_ADD('2025-03-30 02:30:00', INTERVAL 1 HOUR)")
            .unwrap();
        assert_eq!(ok.rows[0][0].display(), "2025-03-30 03:30:00");
        let ok = session
            .execute_stmt("SELECT TIMESTAMPADD(HOUR, 1, '2025-03-30 02:30:00')")
            .unwrap();
        assert_eq!(ok.rows[0][0].display(), "2025-03-30 03:30:00");
        let ok = session
            .execute_stmt("SELECT TIMESTAMPDIFF(HOUR, '2025-03-30 01:59:59', '2025-03-30 02:30:00')")
            .unwrap();
        assert_eq!(ok.rows, vec![vec![Datum::I64(0)]]);
    }

    #[test]
    fn test_timestampadd_column_dst_8179() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE IF NOT EXISTS dst").unwrap();
        session.execute_stmt("USE dst").unwrap();
        session.execute_stmt("SET time_zone='UTC'").unwrap();
        session.execute_stmt("DROP TABLE IF EXISTS t").unwrap();
        session.execute_stmt("CREATE TABLE t (id int primary key, ts TIMESTAMP)").unwrap();
        session.execute_stmt("INSERT INTO t VALUES (1, '2025-03-30 00:59:59')").unwrap();
        session.execute_stmt("SET time_zone='Europe/Paris'").unwrap();
        let err = session
            .execute_stmt("SELECT TIMESTAMPADD(HOUR, 1, ts) FROM t")
            .expect_err("dst");
        assert_eq!(err.code, 8179, "{err}");
    }

    #[test]
    fn test_timestamp_index_nullsafe_dst_warnings() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE IF NOT EXISTS tzidx").unwrap();
        session.execute_stmt("USE tzidx").unwrap();
        session.execute_stmt("SET time_zone='UTC'").unwrap();
        session.execute_stmt("DROP TABLE IF EXISTS t").unwrap();
        session
            .execute_stmt("CREATE TABLE t (id int primary key, ts TIMESTAMP)")
            .unwrap();
        session
            .execute_stmt("INSERT INTO t VALUES (1, '2025-03-30 00:59:59')")
            .unwrap();
        session
            .execute_stmt("INSERT INTO t VALUES (2, '2025-03-30 01:00:00')")
            .unwrap();
        session.execute_stmt("ALTER TABLE t ADD INDEX idx_ts (ts)").unwrap();
        session.execute_stmt("SET time_zone='Europe/Paris'").unwrap();
        let eq = session
            .execute_stmt(
                "SELECT /*+ USE_INDEX(t, idx_ts) */ * FROM t WHERE ts = '2025-03-30 02:00:00'",
            )
            .unwrap();
        assert_eq!(eq.warnings.len(), 4, "eq {:?}", eq.warnings);
        let ok = session
            .execute_stmt(
                "SELECT /*+ USE_INDEX(t, idx_ts) */ * FROM t WHERE ts <=> '2025-03-30 02:00:00'",
            )
            .unwrap();
        assert_eq!(ok.rows.len(), 0, "{ok:?}");
        assert_eq!(ok.warnings.len(), 4, "nullsafe {:?}", ok.warnings);
    }

    #[test]
    fn test_timestamp_column_tz_display() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE IF NOT EXISTS tz").unwrap();
        session.execute_stmt("USE tz").unwrap();
        session.execute_stmt("SET time_zone='UTC'").unwrap();
        session.execute_stmt("DROP TABLE IF EXISTS t").unwrap();
        session
            .execute_stmt("CREATE TABLE t (id int primary key, ts TIMESTAMP)")
            .unwrap();
        session
            .execute_stmt("INSERT INTO t VALUES (1, '2025-03-30 00:59:59')")
            .unwrap();
        session
            .execute_stmt("INSERT INTO t VALUES (2, '2025-03-30 01:00:00')")
            .unwrap();
        session.execute_stmt("SET time_zone='Europe/Paris'").unwrap();
        let ok = session
            .execute_stmt("SELECT *, UNIX_TIMESTAMP(ts) FROM t")
            .unwrap();
        let rows = ok.display_rows();
        assert_eq!(rows[0][1], "2025-03-30 01:59:59", "{rows:?}");
        assert_eq!(rows[1][1], "2025-03-30 03:00:00", "{rows:?}");
        assert_eq!(rows[0][2], "1743296399");
        assert_eq!(rows[1][2], "1743296400");
    }

    #[test]
    fn test_no_backslash_escapes() {
        let session = Session::new();
        let def = session.execute_stmt("SELECT '\\\\'").unwrap();
        assert_eq!(def.rows, vec![vec![Datum::Bytes(b"\\".to_vec())]]);
        session
            .execute_stmt("SET sql_mode='NO_BACKSLASH_ESCAPES'")
            .unwrap();
        // Source `\\` must stay two backslashes under NO_BACKSLASH_ESCAPES.
        let nb = session.execute_stmt("SELECT '\\\\'").unwrap();
        assert_eq!(nb.rows, vec![vec![Datum::Bytes(b"\\\\".to_vec())]]);
    }

    #[test]
    fn test_bigint_sub_overflow_1690() {
        let session = Session::new();
        session
            .execute_stmt("SET sql_mode='NO_UNSIGNED_SUBTRACTION'")
            .unwrap();
        let ok = session
            .execute_stmt("SELECT CAST(0 as UNSIGNED) - 1")
            .unwrap();
        assert_eq!(ok.rows, vec![vec![Datum::I64(-1)]]);
        let err = session
            .execute_stmt(
                "SELECT CAST(-1 as UNSIGNED) - cast(9223372036854775807 as unsigned)",
            )
            .unwrap_err();
        assert_eq!(err.code, ERR_DATA_OUT_OF_RANGE, "{err}");
        assert!(
            err.message.contains("18446744073709551615"),
            "{}",
            err.message
        );
    }

    #[test]
    fn test_select_one() {
        let session = Session::new();
        let result = session.execute_stmt("SELECT 1").unwrap();
        assert_eq!(result.rows, vec![vec![Datum::I64(1)]]);
    }

    #[test]
    fn test_from_config_defaults_to_memory() {
        let session = Session::from_config(&Config::default()).unwrap();
        let result = session.execute_stmt("SELECT 1").unwrap();
        assert_eq!(result.rows, vec![vec![Datum::I64(1)]]);
    }

    fn tikv_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_tikv_sql_roundtrip_if_available() {
        let _guard = tikv_test_lock();
        let cfg = Config {
            store: sealdb_config::StoreKind::Tikv,
            ..Config::default()
        };
        let session = match Session::from_config(&cfg) {
            Ok(session) => session,
            Err(err) => {
                eprintln!("skip live tikv sql: {err}");
                return;
            }
        };
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS tikv_smoke; CREATE DATABASE tikv_smoke; CREATE TABLE tikv_smoke.t (id INT, name VARCHAR(20)); INSERT INTO tikv_smoke.t VALUES (1, 'a');",
            )
            .unwrap();
        let rows = session.execute_stmt("SELECT * FROM tikv_smoke.t").unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
    }


    #[test]
    fn test_exists_with_recursive_cte_correlated() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte;                  CREATE TABLE t1(a INT); INSERT INTO t1 VALUES (1),(2);",
            )
            .unwrap();
        let filtered = session
            .execute_stmt(
                "WITH RECURSIVE qn AS (SELECT 1 AS b UNION ALL SELECT b+1 FROM qn WHERE b=0)                  SELECT * FROM qn WHERE b=1",
            )
            .unwrap_or_else(|e| panic!("filtered recursive failed: {e}"));
        assert_eq!(filtered.display_rows(), vec![vec!["1".to_string()]]);
        let rows = session
            .execute_stmt(
                "SELECT * FROM t1 dt WHERE EXISTS(                    WITH RECURSIVE qn AS (SELECT a*0 AS b UNION ALL SELECT b+1 FROM qn WHERE b=0)                    SELECT * FROM qn WHERE b=a)",
            )
            .unwrap_or_else(|e| panic!("EXISTS recursive CTE failed: {e}"));
        assert_eq!(rows.display_rows(), vec![vec!["1".to_string()]]);
    }


    #[test]
    fn test_unnamed_key_index_name_and_cte_binding() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte; \
                 CREATE TABLE t(a INT, b INT, KEY (b))",
            )
            .unwrap();
        // TiDB: unnamed `KEY (b)` → index name `b` (not `idx_b`).
        let idx = session.execute_stmt("SHOW INDEX FROM t").unwrap();
        assert_eq!(idx.display_rows()[0][2], "b", "{:?}", idx.display_rows());
        let direct = session
            .execute_stmt("DESC SELECT * FROM t USE INDEX(b)")
            .unwrap();
        let direct_text = direct
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            direct_text.contains("IndexLookUp") || direct_text.contains("IndexFullScan"),
            "{direct_text}"
        );
        session
            .execute_stmt(
                "CREATE SESSION BINDING FOR WITH cte AS (SELECT * FROM t) SELECT * FROM cte \
                 USING WITH cte AS (SELECT * FROM t USE INDEX(b)) SELECT * FROM cte",
            )
            .unwrap();
        let after = session
            .execute_stmt("DESC WITH cte AS (SELECT * FROM t) SELECT * FROM cte")
            .unwrap();
        let after_text = after
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            after_text.contains("IndexLookUp") || after_text.contains("IndexFullScan"),
            "{after_text}"
        );
    }

    #[test]
    fn test_exists_cte_decorrelates_to_semi_join() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte;                  CREATE TABLE t1(c1 INT, c2 INT); INSERT INTO t1 VALUES (2,1),(2,2);                  CREATE TABLE t2(c1 INT, c2 INT); INSERT INTO t2 VALUES (1,1),(3,2);",
            )
            .unwrap();
        let plan = session
            .execute_stmt(
                "EXPLAIN FORMAT='brief' SELECT * FROM t1 WHERE EXISTS(                  WITH cte1 AS (SELECT c1 FROM t2 WHERE t2.c2 = t1.c2) SELECT c1 FROM cte1)",
            )
            .unwrap_or_else(|e| panic!("{e}"));
        let text = plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("HashJoin"), "{text}");
        assert!(text.contains("semi join"), "{text}");
        assert!(!text.contains("Apply"), "{text}");
        session
            .execute_stmt("INSERT INTO t1 VALUES (2, 3)")
            .unwrap();
        let rows = session
            .execute_stmt(
                "SELECT * FROM t1 WHERE EXISTS(WITH cte1 AS (SELECT c1 FROM t2 WHERE t2.c2 = t1.c2) SELECT c1 FROM cte1) ORDER BY c1, c2",
            )
            .unwrap_or_else(|e| panic!("{e}"));
        let got: Vec<String> = rows
            .display_rows()
            .into_iter()
            .map(|r| r.join(","))
            .collect();
        assert_eq!(got, vec!["2,1".to_string(), "2,2".to_string()], "{got:?}");
    }

    #[test]
    fn test_recursive_cte_limit_under_all() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte;                  CREATE TABLE t1(c1 INT, c2 INT); INSERT INTO t1 VALUES (2,1),(2,2),(2,3);                  CREATE TABLE t2(c1 INT, c2 INT); INSERT INTO t2 VALUES (1,1),(3,2);",
            )
            .unwrap();
        let rows = session
            .execute_stmt(
                "SELECT * FROM t1 WHERE c1 > ALL(WITH RECURSIVE cte1 AS (                    SELECT c1 FROM t2 WHERE t2.c2 = t1.c2                    UNION ALL SELECT c1+1 AS c1 FROM cte1 LIMIT 1) SELECT c1 FROM cte1) ORDER BY c1, c2",
            )
            .unwrap_or_else(|e| panic!("{e}"));
        let got: Vec<String> = rows
            .display_rows()
            .into_iter()
            .map(|r| r.join(","))
            .collect();
        assert_eq!(got, vec!["2,1".to_string(), "2,3".to_string()], "{got:?}");
    }

    #[test]
    fn test_merge_join_hint_with_cte_inline() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte; \
                 CREATE TABLE t1(c1 INT); INSERT INTO t1 VALUES (1),(2),(1),(2);",
            )
            .unwrap();
        let plan = session
            .execute_stmt(
                "EXPLAIN FORMAT='brief' WITH cte1 AS (SELECT c1 FROM t1) \
                 SELECT /*+ MERGE_JOIN(dt1, dt2) */ * FROM t1 dt1 \
                 INNER JOIN cte1 dt2 ON dt2.c1 = dt1.c1 ORDER BY 1, 2",
            )
            .unwrap_or_else(|e| panic!("{e}"));
        let text = plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("MergeJoin"), "{text}");
        assert!(!text.contains("CTE"), "{text}");
        assert!(
            text.contains("left key:cte.t1.c1") && text.contains("right key:cte.t1.c1"),
            "{text}"
        );
        assert!(text.contains("Sort(Build)"), "{text}");
        assert!(text.contains("Sort(Probe)"), "{text}");
        assert!(text.contains("not(isnull(cte.t1.c1))"), "{text}");
        assert!(!text.lines().next().unwrap_or("").starts_with("Sort|"), "{text}");
        let rows = session
            .execute_stmt(
                "WITH cte1 AS (SELECT c1 FROM t1) \
                 SELECT /*+ MERGE_JOIN(dt1, dt2) */ * FROM t1 dt1 \
                 INNER JOIN cte1 dt2 ON dt2.c1 = dt1.c1 ORDER BY 1, 2",
            )
            .unwrap_or_else(|e| panic!("exec: {e}"));
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string()],
                vec!["1".to_string(), "1".to_string()],
                vec!["1".to_string(), "1".to_string()],
                vec!["1".to_string(), "1".to_string()],
                vec!["2".to_string(), "2".to_string()],
                vec!["2".to_string(), "2".to_string()],
                vec!["2".to_string(), "2".to_string()],
                vec!["2".to_string(), "2".to_string()],
            ]
        );
    }

    #[test]
    fn test_scalar_with_chained_cte_and_alias() {
        let session = Session::new();
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS cte; CREATE DATABASE cte; USE cte; \
                 CREATE TABLE t1 (a INT); INSERT INTO t1 VALUES (1);",
            )
            .unwrap();
        let scalar = session
            .execute_stmt(
                "SELECT (WITH qn AS (SELECT 10*a AS a FROM t1), \
                 qn2 AS (SELECT 3*a AS b FROM qn) SELECT * FROM qn2 LIMIT 1) FROM t1",
            )
            .unwrap_or_else(|e| panic!("scalar WITH chain: {e}"));
        assert_eq!(scalar.display_rows(), vec![vec!["30".to_string()]]);
        assert!(
            scalar.columns[0].len() <= 256,
            "TiDB MaxAliasIdentifierLen: {}",
            scalar.columns[0].len()
        );
        session
            .execute_stmt("DROP TABLE IF EXISTS t1; CREATE TABLE t1 (a INT, b INT)")
            .unwrap();
        let grouped = session
            .execute_stmt("WITH qn AS (SELECT a, b FROM t1) SELECT b FROM qn GROUP BY a")
            .unwrap_or_else(|e| panic!("CTE GROUP BY: {e}"));
        assert_eq!(grouped.columns, vec!["b".to_string()]);
        assert!(grouped.display_rows().is_empty());
        session
            .execute_stmt(
                "DROP TABLE IF EXISTS tbl_1; \
                 CREATE TABLE tbl_1 (col_2 CHAR(65), col_3 INT NOT NULL)",
            )
            .unwrap();
        let nested = session
            .execute_stmt(
                "WITH RECURSIVE cte_8932 (col_34891, col_34892) AS ( \
                   WITH RECURSIVE cte_8932 (col_34893, col_34894, col_34895) AS ( \
                     WITH tbl_1 (col_34896, col_34897, col_34898, col_34899) AS ( \
                       SELECT 1, '2', 3, col_3 FROM tbl_1 \
                     ) \
                     SELECT cte_as_8958.col_34896, cte_as_8958.col_34898, cte_as_8958.col_34899 \
                       FROM tbl_1 AS cte_as_8958 \
                     UNION DISTINCT \
                     SELECT col_34893 + 1, CONCAT(col_34894, 1), col_34895 + 1 \
                       FROM cte_8932 WHERE col_34893 < 5 \
                   ) \
                   SELECT cte_as_8959.col_34893, cte_as_8959.col_34895 \
                     FROM cte_8932 AS cte_as_8959 \
                 ) \
                 SELECT * FROM cte_8932 AS cte_as_8960 \
                 ORDER BY cte_as_8960.col_34891, cte_as_8960.col_34892",
            )
            .unwrap_or_else(|e| panic!("nested aliased CTE: {e}"));
        assert_eq!(
            nested.columns,
            vec!["col_34891".to_string(), "col_34892".to_string()]
        );
        assert!(nested.display_rows().is_empty());
    }

    #[test]
    fn test_schema_persists_across_domain() {
        let storage: Arc<dyn sealdb_kv::Storage> = Arc::new(sealdb_kv::MemStorage::new());
        let first = Session::with_domain(Arc::new(
            Domain::with_storage(Arc::clone(&storage)).unwrap(),
        ));
        first
            .execute_stmt(
                "CREATE DATABASE persist_db; CREATE TABLE persist_db.t (id INT, name VARCHAR(20)); INSERT INTO persist_db.t VALUES (1, 'a')",
            )
            .unwrap();
        drop(first);
        let second = Session::with_domain(Arc::new(Domain::with_storage(storage).unwrap()));
        let rows = second
            .execute_stmt("SELECT name FROM persist_db.t")
            .unwrap();
        assert_eq!(rows.columns, vec!["name"]);
        assert_eq!(rows.display_rows(), vec![vec!["a".to_string()]]);
    }

    #[test]
    fn test_tikv_schema_reload_if_available() {
        let _guard = tikv_test_lock();
        let cfg = Config {
            store: sealdb_config::StoreKind::Tikv,
            ..Config::default()
        };
        let first = match Session::from_config(&cfg) {
            Ok(session) => session,
            Err(err) => {
                eprintln!("skip live tikv schema reload: {err}");
                return;
            }
        };
        first
            .execute_stmt(
                "DROP DATABASE IF EXISTS persist_reload; CREATE DATABASE persist_reload; CREATE TABLE persist_reload.t (id INT, name VARCHAR(20)); INSERT INTO persist_reload.t VALUES (7, 'z')",
            )
            .unwrap();
        drop(first);
        let second = Session::from_config(&cfg).unwrap();
        let rows = second
            .execute_stmt("SELECT * FROM persist_reload.t")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["7".to_string(), "z".to_string()]]
        );
    }

    #[test]
    fn test_tikv_pessimistic_for_update_if_available() {
        let _guard = tikv_test_lock();
        let cfg = Config {
            store: sealdb_config::StoreKind::Tikv,
            ..Config::default()
        };
        let session = match Session::from_config(&cfg) {
            Ok(session) => session,
            Err(err) => {
                eprintln!("skip live tikv pessimistic: {err}");
                return;
            }
        };
        session
            .execute_stmt(
                "DROP DATABASE IF EXISTS tikv_pessimistic; CREATE DATABASE tikv_pessimistic; CREATE TABLE tikv_pessimistic.t (id INT, name VARCHAR(20)); INSERT INTO tikv_pessimistic.t VALUES (1, 'a')",
            )
            .unwrap();
        session
            .execute_stmt(
                "BEGIN PESSIMISTIC; SELECT * FROM tikv_pessimistic.t FOR UPDATE; UPDATE tikv_pessimistic.t SET name = 'b' WHERE id = 1; COMMIT",
            )
            .unwrap();
        let rows = session
            .execute_stmt("SELECT * FROM tikv_pessimistic.t")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "b".to_string()]]
        );
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
    fn test_do_flush_setpwd_shutdown_alter_database() {
        let session = Session::new();
        session.execute_stmt("DO 1, 2 + 3").unwrap();
        session.execute_stmt("FLUSH TABLES").unwrap();
        session.execute_stmt("FLUSH PRIVILEGES").unwrap();

        session
            .execute_stmt("CREATE USER 'u'@'%' IDENTIFIED BY 'old'")
            .unwrap();
        session
            .execute_stmt("SET PASSWORD FOR 'u'@'%' = 'newpwd'")
            .unwrap();
        let auth = session
            .privilege_manager()
            .auth_string(&sealdb_parser::ast::UserIdentity::new("u", "%"));
        assert_eq!(auth.as_deref(), Some("newpwd"));

        assert!(!session.domain().is_shutdown_requested());
        session.execute_stmt("SHUTDOWN").unwrap();
        assert!(session.domain().is_shutdown_requested());

        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("ALTER DATABASE db1 CHARACTER SET latin1 COLLATE latin1_bin")
            .unwrap();
        let db = session.infoschema().schema_by_name("db1").unwrap().clone();
        assert_eq!(db.charset, "latin1");
        assert_eq!(db.collate, "latin1_bin");
        let create = session.execute_stmt("SHOW CREATE DATABASE db1").unwrap();
        assert!(
            create.display_rows()[0][1].contains("CHARACTER SET latin1"),
            "{:?}",
            create.display_rows()
        );
    }

    #[test]
    fn test_allowlist_kill_lock_optimize_repair_rename_binlog_change_alter_instance() {
        let session = Session::new();
        // KILL: register a fake id and mark it.
        session.domain().register_connection(99);
        assert!(!session.domain().is_connection_killed(99));
        session.execute_stmt("KILL CONNECTION 99").unwrap();
        assert!(session.domain().is_connection_killed(99));
        session.execute_stmt("KILL QUERY 99").unwrap();

        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); CREATE TABLE db1.t2 (id INT)",
            )
            .unwrap();
        session
            .execute_stmt("LOCK TABLES db1.t READ, db1.t2 WRITE")
            .unwrap();
        let sid = session.session_id();
        assert!(session.domain().session_holds_table_lock(sid, "db1.t"));
        assert!(session.domain().session_holds_table_lock(sid, "db1.t2"));
        session.execute_stmt("UNLOCK TABLES").unwrap();
        assert!(!session.domain().session_holds_table_lock(sid, "db1.t"));

        session.execute_stmt("LOCK TABLES db1.t WRITE").unwrap();
        session
            .execute_stmt("ADMIN CLEANUP TABLE LOCK db1.t")
            .unwrap();
        assert!(!session.domain().session_holds_table_lock(sid, "db1.t"));

        let optimize = session.execute_stmt("OPTIMIZE TABLE db1.t").unwrap();
        assert_eq!(
            optimize.columns,
            vec!["Table", "Op", "Msg_type", "Msg_text"]
        );
        assert_eq!(
            optimize.display_rows(),
            vec![vec![
                "db1.t".to_string(),
                "optimize".to_string(),
                "status".to_string(),
                "OK".to_string()
            ]]
        );
        let repair = session.execute_stmt("REPAIR TABLE db1.t").unwrap();
        assert_eq!(repair.display_rows()[0][1], "repair".to_string());

        session
            .execute_stmt("CREATE USER 'u'@'%' IDENTIFIED BY 'pwd'")
            .unwrap();
        session
            .execute_stmt("RENAME USER 'u'@'%' TO 'v'@'%'")
            .unwrap();
        assert_eq!(
            session
                .privilege_manager()
                .auth_string(&sealdb_parser::ast::UserIdentity::new("v", "%"))
                .as_deref(),
            Some("pwd")
        );
        assert!(
            session
                .privilege_manager()
                .auth_string(&sealdb_parser::ast::UserIdentity::new("u", "%"))
                .is_none()
        );

        session.execute_stmt("BINLOG 'deadbeef'").unwrap();
        session
            .execute_stmt("CHANGE PUMP TO NODE_STATE = 'offline' FOR NODE_ID 'n1'")
            .unwrap();
        session.execute_stmt("ALTER INSTANCE RELOAD TLS").unwrap();
    }

    #[test]
    fn test_truncate_table_clears_rows_keeps_schema() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        let before = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            before.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let table_id = session.infoschema().table_by_name("db1", "t").unwrap().id;
        // Bump next_handle via auto-handle inserts; truncate resets it.
        session
            .execute_stmt("INSERT INTO db1.t (name) VALUES ('x'), ('y')")
            .unwrap();
        session.execute_stmt("TRUNCATE TABLE db1.t").unwrap();
        let after = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert!(after.display_rows().is_empty());
        let info = session
            .infoschema()
            .table_by_name("db1", "t")
            .unwrap()
            .clone();
        assert_eq!(info.id, table_id);
        assert_eq!(info.columns.len(), 2);
        assert_eq!(info.columns[0].name.o(), "id");
        assert_eq!(info.columns[1].name.o(), "name");
        // Re-insert into the same schema; without truncate this would be a dup key.
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'c')")
            .unwrap();
        let rows = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "c".to_string()]]
        );
        session.execute_stmt("TRUNCATE db1.t").unwrap();
        assert!(
            session
                .execute_stmt("SELECT * FROM db1.t")
                .unwrap()
                .display_rows()
                .is_empty()
        );
    }

    #[test]
    fn test_load_data_infile() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sealdb_load_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\n1,a\n2,b\n").unwrap();
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        let sql = format!(
            "LOAD DATA INFILE '{}' INTO TABLE db1.t FIELDS TERMINATED BY ',' LINES TERMINATED BY '\\n' IGNORE 1 LINES",
            path.display()
        );
        session.execute_stmt(&sql).unwrap();
        let rows = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_create_drop_alter_sequence_nextval() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE SEQUENCE db1.s START WITH 10 INCREMENT BY 2")
            .unwrap();
        let first = session.execute_stmt("SELECT NEXTVAL(db1.s)").unwrap();
        assert_eq!(first.display_rows(), vec![vec!["10".to_string()]]);
        let second = session.execute_stmt("SELECT NEXTVAL(db1.s)").unwrap();
        assert_eq!(second.display_rows(), vec![vec!["12".to_string()]]);
        session
            .execute_stmt("ALTER SEQUENCE db1.s RESTART WITH 100")
            .unwrap();
        let restarted = session.execute_stmt("SELECT NEXTVAL(db1.s)").unwrap();
        assert_eq!(restarted.display_rows(), vec![vec!["100".to_string()]]);
        let is_seq = session
            .execute_stmt(
                "SELECT SEQUENCE_NAME, START, INCREMENT FROM information_schema.sequences WHERE SEQUENCE_SCHEMA = 'db1'",
            )
            .unwrap();
        assert_eq!(is_seq.display_rows()[0][0], "s");
        assert_eq!(is_seq.display_rows()[0][1], "100");
        let err = session.execute_stmt("DROP TABLE db1.s").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_WRONG_OBJECT);
        session.execute_stmt("DROP SEQUENCE db1.s").unwrap();
        let gone = session.execute_stmt("SELECT NEXTVAL(db1.s)").unwrap_err();
        assert_eq!(gone.code, sealdb_errno::ERR_NO_SUCH_TABLE);
    }

    #[test]
    fn test_view_algorithm_temptable() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        session
            .execute_stmt(
                "CREATE ALGORITHM=TEMPTABLE VIEW db1.v AS SELECT id, name FROM db1.t WHERE id = 1",
            )
            .unwrap();
        let rows = session.execute_stmt("SELECT * FROM db1.v").unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let explain = session.execute_stmt("EXPLAIN SELECT * FROM db1.v").unwrap();
        let plan = format!("{:?}", explain.display_rows());
        assert!(
            plan.contains("CTE") || plan.to_ascii_lowercase().contains("material"),
            "expected materialized CTE in plan, got:\n{plan}"
        );
        let upd = session
            .execute_stmt("UPDATE db1.v SET name = 'x'")
            .unwrap_err();
        assert!(
            upd.message.contains("TEMPTABLE") || upd.message.contains("not updatable"),
            "{}",
            upd.message
        );
        let bad = session
            .execute_stmt(
                "CREATE ALGORITHM=TEMPTABLE VIEW db1.bad AS SELECT id FROM db1.t WITH CHECK OPTION",
            )
            .unwrap_err();
        assert_eq!(bad.code, sealdb_errno::ERR_VIEW_INVALID);
        session.execute_stmt("DROP VIEW db1.v").unwrap();
    }

    #[test]
    fn test_create_drop_view_select() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        session
            .execute_stmt("CREATE VIEW db1.v AS SELECT id, name FROM db1.t WHERE id = 1")
            .unwrap();
        let rows = session.execute_stmt("SELECT * FROM db1.v").unwrap();
        assert_eq!(rows.columns, vec!["id", "name"]);
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let is_views = session
            .execute_stmt(
                "SELECT TABLE_NAME, VIEW_DEFINITION FROM information_schema.views WHERE TABLE_SCHEMA = 'db1'",
            )
            .unwrap();
        assert_eq!(is_views.display_rows()[0][0], "v");
        assert!(
            is_views.display_rows()[0][1].contains("SELECT"),
            "view definition: {}",
            is_views.display_rows()[0][1]
        );
        let show = session.execute_stmt("SHOW CREATE VIEW db1.v").unwrap();
        assert_eq!(show.columns, vec!["View", "Create View"]);
        assert!(show.display_rows()[0][1].contains("CREATE ALGORITHM="));
        session
            .execute_stmt("CREATE OR REPLACE VIEW db1.v AS SELECT id FROM db1.t WHERE id = 2")
            .unwrap();
        let replaced = session.execute_stmt("SELECT * FROM db1.v").unwrap();
        assert_eq!(replaced.display_rows(), vec![vec!["2".to_string()]]);
        let err = session.execute_stmt("DROP TABLE db1.v").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_WRONG_OBJECT);
        session.execute_stmt("DROP VIEW db1.v").unwrap();
        let gone = session.execute_stmt("SELECT * FROM db1.v").unwrap_err();
        assert_eq!(gone.code, sealdb_errno::ERR_NO_SUCH_TABLE);
    }

    #[test]
    fn test_simple_updatable_view_update_delete() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        session
            .execute_stmt("CREATE VIEW db1.v AS SELECT id, name FROM db1.t WHERE id = 1")
            .unwrap();
        session.execute_stmt("UPDATE db1.v SET name = 'x'").unwrap();
        let rows = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        session.execute_stmt("DELETE FROM db1.v").unwrap();
        let left = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            left.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_view_with_check_option_rejects_escape() {
        let session = Session::new();
        session.execute_stmt("CREATE DATABASE db1").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        session
            .execute_stmt(
                "CREATE VIEW db1.v AS SELECT id, name FROM db1.t WHERE id = 1 WITH CHECK OPTION",
            )
            .unwrap();
        session
            .execute_stmt("UPDATE db1.v SET name = 'ok'")
            .unwrap();
        let err = session.execute_stmt("UPDATE db1.v SET id = 2").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_VIEW_CHECK_FAILED);
        let rows = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "ok".to_string()]]
        );
    }

    #[test]
    fn test_create_table_like_and_as_select() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.src (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.src VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("CREATE TABLE db1.like_t LIKE db1.src")
            .unwrap();
        let like_info = session
            .infoschema()
            .table_by_name("db1", "like_t")
            .unwrap()
            .clone();
        assert_eq!(like_info.columns.len(), 2);
        assert_eq!(like_info.columns[0].name.o(), "id");
        assert_eq!(like_info.columns[1].name.o(), "name");
        assert_eq!(like_info.indices.len(), 1);
        assert_eq!(like_info.indices[0].name.o(), "idx_name");
        let like_rows = session.execute_stmt("SELECT * FROM db1.like_t").unwrap();
        assert!(like_rows.display_rows().is_empty());
        session
            .execute_stmt("CREATE TABLE db1.ctas AS SELECT * FROM db1.src")
            .unwrap();
        let copied = session
            .execute_stmt("SELECT * FROM db1.ctas ORDER BY id")
            .unwrap();
        assert_eq!(
            copied.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        session
            .execute_stmt("CREATE TABLE db1.proj AS SELECT id FROM db1.src WHERE id = 2")
            .unwrap();
        let proj = session.execute_stmt("SELECT * FROM db1.proj").unwrap();
        assert_eq!(proj.columns, vec!["id"]);
        assert_eq!(proj.display_rows(), vec![vec!["2".to_string()]]);
        session
            .execute_stmt("CREATE TABLE IF NOT EXISTS db1.ctas AS SELECT * FROM db1.src")
            .unwrap();
        let again = session
            .execute_stmt("SELECT COUNT(*) FROM db1.ctas")
            .unwrap();
        assert_eq!(again.display_rows()[0][0], "2");
        let err = session
            .execute_stmt("CREATE TABLE db1.missing_like LIKE db1.nope")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_NO_SUCH_TABLE);
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
        let full = session.execute_stmt("SHOW FULL COLUMNS FROM t").unwrap();
        assert_eq!(
            full.columns,
            vec![
                "Field",
                "Type",
                "Collation",
                "Null",
                "Key",
                "Default",
                "Extra",
                "Privileges",
                "Comment"
            ]
        );
        assert_eq!(full.display_rows()[0][0], "id");
        assert_eq!(full.display_rows()[0][2], "NULL");
        assert_eq!(full.display_rows()[1][2], "utf8mb4_bin");
        assert!(full.display_rows()[0][7].contains("select"));
        let is_tables = session
            .execute_stmt(
                "SELECT TABLE_NAME FROM information_schema.tables WHERE TABLE_SCHEMA = 'db1'",
            )
            .unwrap();
        assert_eq!(is_tables.display_rows(), vec![vec!["t".to_string()]]);
        let is_cols = session
            .execute_stmt("SELECT COLUMN_NAME FROM information_schema.columns WHERE TABLE_SCHEMA = 'db1' AND TABLE_NAME = 't' ORDER BY COLUMN_NAME")
            .unwrap();
        assert_eq!(
            is_cols.display_rows(),
            vec![vec!["id".to_string()], vec!["name".to_string()]]
        );
        let schemata = session
            .execute_stmt(
                "SELECT SCHEMA_NAME FROM information_schema.schemata WHERE SCHEMA_NAME = 'db1'",
            )
            .unwrap();
        assert_eq!(schemata.display_rows(), vec![vec!["db1".to_string()]]);
        let create_db = session.execute_stmt("SHOW CREATE DATABASE db1").unwrap();
        assert_eq!(create_db.columns, vec!["Database", "Create Database"]);
        assert_eq!(create_db.display_rows()[0][0], "db1");
        assert!(create_db.display_rows()[0][1].contains("CREATE DATABASE `db1`"));
        let create_table = session.execute_stmt("SHOW CREATE TABLE db1.t").unwrap();
        assert_eq!(create_table.columns, vec!["Table", "Create Table"]);
        assert_eq!(create_table.display_rows()[0][0], "t");
        assert!(create_table.display_rows()[0][1].contains("`id` int"));
        assert!(create_table.display_rows()[0][1].contains("`name` varchar(20)"));
        session
            .execute_stmt("CREATE INDEX idx_name ON db1.t (name)")
            .unwrap();
        let stats = session
            .execute_stmt(
                "SELECT INDEX_NAME, COLUMN_NAME FROM information_schema.statistics WHERE TABLE_SCHEMA = 'db1' AND TABLE_NAME = 't'",
            )
            .unwrap();
        assert_eq!(
            stats.display_rows(),
            vec![vec!["idx_name".to_string(), "name".to_string()]]
        );
        let show_index = session.execute_stmt("SHOW INDEX FROM db1.t").unwrap();
        assert_eq!(show_index.columns[2], "Key_name");
        assert_eq!(show_index.display_rows()[0][2], "idx_name");
        let charset = session.execute_stmt("SHOW CHARSET").unwrap();
        assert!(
            charset
                .display_rows()
                .iter()
                .any(|row| row.first().map(String::as_str) == Some("utf8mb4"))
        );
        let collation = session.execute_stmt("SHOW COLLATION").unwrap();
        assert!(
            collation
                .display_rows()
                .iter()
                .any(|row| row.first().map(String::as_str) == Some("utf8mb4_bin"))
        );
        let engines = session
            .execute_stmt("SELECT ENGINE FROM information_schema.engines")
            .unwrap();
        assert_eq!(engines.display_rows(), vec![vec!["InnoDB".to_string()]]);
        let views = session
            .execute_stmt("SELECT TABLE_NAME FROM information_schema.views")
            .unwrap();
        assert!(views.display_rows().is_empty());
        let jobs = session.execute_stmt("ADMIN SHOW DDL JOBS").unwrap();
        assert_eq!(
            jobs.columns,
            vec![
                "JOB_ID",
                "DB_NAME",
                "TABLE_NAME",
                "JOB_TYPE",
                "STATE",
                "SCHEMA_STATE"
            ]
        );
        assert_eq!(jobs.display_rows()[0][3], "add index");
        assert_eq!(jobs.display_rows()[0][4], "synced");
        assert_eq!(jobs.display_rows()[0][5], "public");
        assert_eq!(jobs.display_rows()[1][3], "create table");
        assert_eq!(jobs.display_rows()[2][3], "create schema");
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
        session
            .execute_stmt("CREATE TABLE db1.src (id INT, name VARCHAR(20)); INSERT INTO db1.src VALUES (3, 'c')")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t SELECT * FROM db1.src")
            .unwrap();
        let copied = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 3")
            .unwrap();
        assert_eq!(
            copied.display_rows(),
            vec![vec!["3".to_string(), "c".to_string()]]
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
        let between = session
            .execute_stmt("SELECT * FROM db1.t WHERE id BETWEEN 2 AND 3 ORDER BY id")
            .unwrap();
        assert_eq!(
            between.display_rows(),
            vec![
                vec!["2".to_string(), "b".to_string()],
                vec!["3".to_string(), "c".to_string()]
            ]
        );
        let like = session
            .execute_stmt("SELECT * FROM db1.t WHERE name LIKE 'b%'")
            .unwrap();
        assert_eq!(
            like.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let in_list = session
            .execute_stmt("SELECT * FROM db1.t WHERE id IN (1, 3) ORDER BY id")
            .unwrap();
        assert_eq!(
            in_list.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["3".to_string(), "c".to_string()]
            ]
        );
        session
            .execute_stmt("INSERT INTO db1.t VALUES (4, NULL)")
            .unwrap();
        let is_null = session
            .execute_stmt("SELECT id FROM db1.t WHERE name IS NULL")
            .unwrap();
        assert_eq!(is_null.columns, vec!["id"]);
        assert_eq!(is_null.display_rows(), vec![vec!["4".to_string()]]);
        let explained = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE id > 1 AND name = 'c'")
            .unwrap();
        let text = explained
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("TableReader"), "{text}");
        assert!(text.contains("pushed:"), "{text}");
        assert!(text.contains("cop"), "{text}");
        // PPD: filter is under TableReader as a cop Selection, not a root Selection.
        assert!(
            text.lines()
                .next()
                .map(|l| l.contains("TableReader"))
                .unwrap_or(false),
            "{text}"
        );
        assert!(!text.lines().any(|l| l.starts_with("Selection")), "{text}");
    }

    #[test]
    fn test_table_column_projection() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let aliased = session
            .execute_stmt("SELECT id AS i FROM db1.t WHERE id = 2")
            .unwrap();
        assert_eq!(aliased.columns, vec!["i"]);
        assert_eq!(aliased.display_rows(), vec![vec!["2".to_string()]]);
        let named = session
            .execute_stmt("SELECT name FROM db1.t WHERE name = 'b'")
            .unwrap();
        assert_eq!(named.columns, vec!["name"]);
        assert_eq!(named.display_rows(), vec![vec!["b".to_string()]]);
        let ordered = session
            .execute_stmt("SELECT name FROM db1.t ORDER BY id DESC LIMIT 1")
            .unwrap();
        assert_eq!(ordered.columns, vec!["name"]);
        assert_eq!(ordered.display_rows(), vec![vec!["c".to_string()]]);
        session
            .execute_stmt(
                "CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'); INSERT INTO db1.t2 VALUES (1, 'x')",
            )
            .unwrap();
        let projected_join = session
            .execute_stmt("SELECT t1.name FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id")
            .unwrap();
        assert_eq!(projected_join.columns, vec!["t1.name"]);
        assert_eq!(projected_join.display_rows(), vec![vec!["a".to_string()]]);
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
        let topn = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t ORDER BY id LIMIT 1")
            .unwrap();
        let topn_text = topn
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(topn_text.contains("TopN"), "{topn_text}");
        assert!(!topn_text.contains("Sort"), "{topn_text}");
        let sorted = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        let sorted_text = sorted
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sorted_text.contains("Sort"), "{sorted_text}");
        assert!(!sorted_text.contains("TopN"), "{sorted_text}");
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
        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20), extra VARCHAR(20), UNIQUE INDEX idx_u_name (name)); INSERT INTO db1.u VALUES (1, 'a', 'x'), (2, 'b', 'y')",
            )
            .unwrap();
        let unique_plan = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.u WHERE name = 'a'")
            .unwrap();
        let unique_text = unique_plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unique_text.contains("Point_Get"), "{unique_text}");
        assert!(!unique_text.contains("IndexLookUp"), "{unique_text}");
        let unique_hit = session
            .execute_stmt("SELECT * FROM db1.u WHERE name = 'a'")
            .unwrap();
        assert_eq!(
            unique_hit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "x".to_string()]]
        );
        let unique_miss = session
            .execute_stmt("SELECT * FROM db1.u WHERE name = 'missing'")
            .unwrap();
        assert!(unique_miss.display_rows().is_empty());
        let one = session
            .execute_stmt("SELECT * FROM db1.t WHERE id IN (2)")
            .unwrap();
        assert_eq!(
            one.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_batch_point_get() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let plan = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE id IN (1, 3)")
            .unwrap();
        let text = plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("BatchPointGet"), "{text}");
        let rows = session
            .execute_stmt("SELECT * FROM db1.t WHERE id IN (1, 3) ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["3".to_string(), "c".to_string()]
            ]
        );
        let miss = session
            .execute_stmt("SELECT * FROM db1.t WHERE id IN (8, 9)")
            .unwrap();
        assert!(miss.display_rows().is_empty());
        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20), extra VARCHAR(20), UNIQUE INDEX idx_u_name (name)); INSERT INTO db1.u VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', 'z')",
            )
            .unwrap();
        let unique_plan = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.u WHERE name IN ('a', 'c')")
            .unwrap();
        let unique_text = unique_plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unique_text.contains("BatchPointGet"), "{unique_text}");
        let unique_rows = session
            .execute_stmt("SELECT * FROM db1.u WHERE name IN ('a', 'c') ORDER BY id")
            .unwrap();
        assert_eq!(
            unique_rows.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string(), "x".to_string()],
                vec!["3".to_string(), "c".to_string(), "z".to_string()]
            ]
        );
        let dup = session
            .execute_stmt("SELECT * FROM db1.t WHERE id IN (1, 1, 3) ORDER BY id")
            .unwrap();
        assert_eq!(dup.display_rows().len(), 2);
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
    fn test_update_delete_order_limit() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        session
            .execute_stmt("UPDATE db1.t SET name = 'z' ORDER BY id DESC LIMIT 1")
            .unwrap();
        let after_update = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            after_update.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()],
                vec!["3".to_string(), "z".to_string()]
            ]
        );
        session
            .execute_stmt("DELETE FROM db1.t ORDER BY id LIMIT 1")
            .unwrap();
        let after_delete = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            after_delete.display_rows(),
            vec![
                vec!["2".to_string(), "b".to_string()],
                vec!["3".to_string(), "z".to_string()]
            ]
        );
        session
            .execute_stmt("UPDATE db1.t SET name = 'x' WHERE id > 1 ORDER BY id LIMIT 1")
            .unwrap();
        let with_where = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            with_where.display_rows(),
            vec![
                vec!["2".to_string(), "x".to_string()],
                vec!["3".to_string(), "z".to_string()]
            ]
        );
    }

    #[test]
    fn test_update_join_and_multi_delete() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (2, 'y')",
            )
            .unwrap();
        session
            .execute_stmt("UPDATE db1.t1 JOIN db1.t2 ON t1.id = t2.id SET t1.name = t2.tag")
            .unwrap();
        let updated = session
            .execute_stmt("SELECT * FROM db1.t1 ORDER BY id")
            .unwrap();
        assert_eq!(
            updated.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["2".to_string(), "y".to_string()]
            ]
        );
        session
            .execute_stmt(
                "DELETE db1.t1 FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id WHERE t2.tag = 'x'",
            )
            .unwrap();
        let after_one = session
            .execute_stmt("SELECT * FROM db1.t1 ORDER BY id")
            .unwrap();
        assert_eq!(
            after_one.display_rows(),
            vec![vec!["2".to_string(), "y".to_string()]]
        );
        session
            .execute_stmt("INSERT INTO db1.t1 VALUES (1, 'x')")
            .unwrap();
        session
            .execute_stmt("DELETE db1.t1, db1.t2 FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id")
            .unwrap();
        let t1 = session.execute_stmt("SELECT * FROM db1.t1").unwrap();
        let t2 = session.execute_stmt("SELECT * FROM db1.t2").unwrap();
        assert!(t1.display_rows().is_empty());
        assert!(t2.display_rows().is_empty());
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
        let stream = session
            .execute_stmt("EXPLAIN SELECT COUNT(*) FROM db1.t")
            .unwrap();
        let stream_text = stream
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(stream_text.contains("StreamAgg"), "{stream_text}");
        let hashed = session
            .execute_stmt("EXPLAIN SELECT name, COUNT(*) FROM db1.t GROUP BY name")
            .unwrap();
        let hashed_text = hashed
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(hashed_text.contains("HashAgg"), "{hashed_text}");
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
        let distinct = session
            .execute_stmt("SELECT COUNT(DISTINCT name) FROM db1.t")
            .unwrap();
        assert_eq!(distinct.display_rows(), vec![vec!["2".to_string()]]);
        session
            .execute_stmt(
                "CREATE TABLE db1.d (name VARCHAR(20), id INT); INSERT INTO db1.d VALUES ('a', 1), ('a', 1), ('a', 2), ('b', 3)",
            )
            .unwrap();
        let distinct_aggs = session
            .execute_stmt(
                "SELECT SUM(DISTINCT id), AVG(DISTINCT id), MIN(DISTINCT id), MAX(DISTINCT id) FROM db1.d",
            )
            .unwrap();
        assert_eq!(
            distinct_aggs.display_rows(),
            vec![vec![
                "6".to_string(),
                "2.0".to_string(),
                "1".to_string(),
                "3".to_string()
            ]]
        );
        let having = session
            .execute_stmt(
                "SELECT name, COUNT(*) FROM db1.t GROUP BY name HAVING COUNT(*) > 1 ORDER BY name",
            )
            .unwrap();
        assert_eq!(
            having.display_rows(),
            vec![vec!["a".to_string(), "2".to_string()]]
        );
        let having_hidden = session
            .execute_stmt("SELECT name FROM db1.t GROUP BY name HAVING COUNT(*) = 1")
            .unwrap();
        assert_eq!(having_hidden.display_rows(), vec![vec!["b".to_string()]]);
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
        let filtered = session
            .execute_stmt(
                "SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id WHERE t1.id = 1",
            )
            .unwrap();
        assert_eq!(
            filtered.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "x".to_string()]]
        );
        let missed = session
            .execute_stmt("SELECT t1.id FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id WHERE t1.id = 9")
            .unwrap();
        assert!(missed.display_rows().is_empty());
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
        let cross = session
            .execute_stmt(
                "SELECT t1.id, t2.tag FROM db1.t1 CROSS JOIN db1.t2 ORDER BY t1.id, t2.tag",
            )
            .unwrap();
        assert_eq!(
            cross.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["1".to_string(), "y".to_string()],
                vec!["2".to_string(), "x".to_string()],
                vec!["2".to_string(), "y".to_string()]
            ]
        );
        let comma = session
            .execute_stmt("SELECT t1.id, t2.tag FROM db1.t1, db1.t2 ORDER BY t1.id, t2.tag")
            .unwrap();
        assert_eq!(comma.display_rows(), cross.display_rows());
        session
            .execute_stmt(
                "CREATE TABLE db1.t3 (id INT, note VARCHAR(20)); INSERT INTO db1.t3 VALUES (1, 'n')",
            )
            .unwrap();
        let three = session
            .execute_stmt(
                "SELECT t1.id, t2.tag, t3.note FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id JOIN db1.t3 ON t2.id = t3.id",
            )
            .unwrap();
        assert_eq!(
            three.display_rows(),
            vec![vec!["1".to_string(), "x".to_string(), "n".to_string()]]
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
        let extra = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a' AND id > 1 ORDER BY id")
            .unwrap();
        assert_eq!(
            extra.display_rows(),
            vec![vec!["3".to_string(), "a".to_string()]]
        );
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
    fn test_unique_and_primary_key() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), PRIMARY KEY (id), UNIQUE INDEX idx_name (name))",
            )
            .unwrap();
        let table = session
            .infoschema()
            .table_by_name("db1", "t")
            .unwrap()
            .clone();
        assert!(table.indices.iter().any(|idx| idx.primary && idx.unique));
        assert!(table.indices.iter().any(|idx| idx.unique && !idx.primary));
        session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        let dup_pk = session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'c')")
            .unwrap_err();
        assert_eq!(dup_pk.code, sealdb_errno::ERR_DUP_ENTRY);
        let dup_unique = session
            .execute_stmt("INSERT INTO db1.t VALUES (3, 'a')")
            .unwrap_err();
        assert_eq!(dup_unique.code, sealdb_errno::ERR_DUP_ENTRY);
        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20)); INSERT INTO db1.u VALUES (1, 'a'), (2, 'a')",
            )
            .unwrap();
        let dup_backfill = session
            .execute_stmt("CREATE UNIQUE INDEX idx_u_name ON db1.u (name)")
            .unwrap_err();
        assert_eq!(dup_backfill.code, sealdb_errno::ERR_DUP_ENTRY);
        session
            .execute_stmt("CREATE UNIQUE INDEX idx_u_id ON db1.u (id)")
            .unwrap();
        let dup_created = session
            .execute_stmt("INSERT INTO db1.u VALUES (1, 'z')")
            .unwrap_err();
        assert_eq!(dup_created.code, sealdb_errno::ERR_DUP_ENTRY);
        let unique_hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert_eq!(
            unique_hit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
    }

    #[test]
    fn test_covering_index_reader() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'a', 'z')",
            )
            .unwrap();
        let covering = session
            .execute_stmt("EXPLAIN SELECT name FROM db1.t WHERE name = 'a'")
            .unwrap();
        let covering_text = covering
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(covering_text.contains("IndexReader"), "{covering_text}");
        assert!(!covering_text.contains("IndexLookUp"), "{covering_text}");
        let names = session
            .execute_stmt("SELECT name FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            names.display_rows(),
            vec![vec!["a".to_string()], vec!["a".to_string()]]
        );
        let ids = session
            .execute_stmt("SELECT id, name FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            ids.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["3".to_string(), "a".to_string()]
            ]
        );
        let lookup = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        let lookup_text = lookup
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lookup_text.contains("IndexLookUp"), "{lookup_text}");
        let extra = session
            .execute_stmt("SELECT extra FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            extra.display_rows(),
            vec![vec!["x".to_string()], vec!["z".to_string()]]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20), extra VARCHAR(20), UNIQUE INDEX idx_u_name (name)); INSERT INTO db1.u VALUES (1, 'a', 'x'), (2, 'b', 'y')",
            )
            .unwrap();
        let unique_cover = session
            .execute_stmt("EXPLAIN SELECT name FROM db1.u WHERE name = 'a'")
            .unwrap();
        let unique_cover_text = unique_cover
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            unique_cover_text.contains("Point_Get"),
            "{unique_cover_text}"
        );
        let unique_lookup = session
            .execute_stmt("SELECT extra FROM db1.u WHERE name = 'a'")
            .unwrap();
        assert_eq!(unique_lookup.display_rows(), vec![vec!["x".to_string()]]);
        let unique_star = session
            .execute_stmt("SELECT * FROM db1.u WHERE name = 'a'")
            .unwrap();
        assert_eq!(
            unique_star.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "x".to_string()]]
        );
    }

    #[test]
    fn test_index_range_scan() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', 'z')",
            )
            .unwrap();
        let lookup = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE name > 'a'")
            .unwrap();
        let lookup_text = lookup
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lookup_text.contains("IndexLookUp"), "{lookup_text}");
        let covering = session
            .execute_stmt("EXPLAIN SELECT name FROM db1.t WHERE name > 'a'")
            .unwrap();
        let covering_text = covering
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(covering_text.contains("IndexReader"), "{covering_text}");
        let gt = session
            .execute_stmt("SELECT * FROM db1.t WHERE name > 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            gt.display_rows(),
            vec![
                vec!["2".to_string(), "b".to_string(), "y".to_string()],
                vec!["3".to_string(), "c".to_string(), "z".to_string()]
            ]
        );
        let ge = session
            .execute_stmt("SELECT * FROM db1.t WHERE name >= 'b' ORDER BY id")
            .unwrap();
        assert_eq!(ge.display_rows(), gt.display_rows());
        let lt = session
            .execute_stmt("SELECT * FROM db1.t WHERE name < 'b' ORDER BY id")
            .unwrap();
        assert_eq!(
            lt.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "x".to_string()]]
        );
        let flipped = session
            .execute_stmt("SELECT * FROM db1.t WHERE 'a' < name ORDER BY id")
            .unwrap();
        assert_eq!(flipped.display_rows(), gt.display_rows());
        let between = session
            .execute_stmt("SELECT * FROM db1.t WHERE name BETWEEN 'a' AND 'b' ORDER BY id")
            .unwrap();
        assert_eq!(
            between.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string(), "x".to_string()],
                vec!["2".to_string(), "b".to_string(), "y".to_string()]
            ]
        );
        let closed = session
            .execute_stmt("SELECT name FROM db1.t WHERE name >= 'a' AND name < 'c' ORDER BY id")
            .unwrap();
        assert_eq!(
            closed.display_rows(),
            vec![vec!["a".to_string()], vec!["b".to_string()]]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20), extra VARCHAR(20), UNIQUE INDEX idx_u_name (name)); INSERT INTO db1.u VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', 'z')",
            )
            .unwrap();
        let unique_range = session
            .execute_stmt("SELECT extra FROM db1.u WHERE name > 'a' ORDER BY extra")
            .unwrap();
        assert_eq!(
            unique_range.display_rows(),
            vec![vec!["y".to_string()], vec!["z".to_string()]]
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
        session
            .execute_stmt("BEGIN; INSERT INTO db1.t VALUES (2, 'b'); SAVEPOINT sp1; INSERT INTO db1.t VALUES (3, 'c'); ROLLBACK TO sp1")
            .unwrap();
        let after_sp = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            after_sp.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        session
            .execute_stmt("RELEASE SAVEPOINT sp1; COMMIT")
            .unwrap();
        let committed = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(committed.display_rows(), after_sp.display_rows());
        let err = session.execute_stmt("ROLLBACK TO missing").unwrap_err();
        assert!(err.message.contains("SAVEPOINT missing"));
        session
            .execute_stmt("START TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        session.execute_stmt("ROLLBACK").unwrap();
        session.execute_stmt("START TRANSACTION READ ONLY").unwrap();
        let ro = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            ro.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let err = session
            .execute_stmt("INSERT INTO db1.t VALUES (4, 'd')")
            .unwrap_err();
        assert_eq!(
            err.code,
            sealdb_errno::ERR_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION
        );
        session.execute_stmt("ROLLBACK").unwrap();
        session
            .execute_stmt(
                "START TRANSACTION READ WRITE; INSERT INTO db1.t VALUES (4, 'd'); COMMIT AND CHAIN",
            )
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (5, 'e'); COMMIT")
            .unwrap();
        let chained = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(chained.display_rows().len(), 4);
    }

    #[test]
    fn test_transactional_ddl() {
        let session = Session::new();
        session
            .execute_stmt("CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt(
                "BEGIN; CREATE TABLE db1.t2 (id INT); INSERT INTO db1.t2 VALUES (1); SELECT * FROM db1.t2",
            )
            .unwrap();
        session.execute_stmt("ROLLBACK").unwrap();
        let err = session.execute_stmt("SELECT * FROM db1.t2").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_NO_SUCH_TABLE);

        session
            .execute_stmt(
                "BEGIN; CREATE TABLE db1.t2 (id INT); INSERT INTO db1.t2 VALUES (1); COMMIT",
            )
            .unwrap();
        let committed = session.execute_stmt("SELECT * FROM db1.t2").unwrap();
        assert_eq!(committed.display_rows(), vec![vec!["1".to_string()]]);

        session
            .execute_stmt("BEGIN; DROP TABLE db1.t2; ROLLBACK")
            .unwrap();
        let still = session.execute_stmt("SELECT * FROM db1.t2").unwrap();
        assert_eq!(still.display_rows(), vec![vec!["1".to_string()]]);

        session
            .execute_stmt("BEGIN; CREATE TABLE db1.ctas AS SELECT * FROM db1.t2; ROLLBACK")
            .unwrap();
        let err = session.execute_stmt("SELECT * FROM db1.ctas").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_NO_SUCH_TABLE);

        session
            .execute_stmt("BEGIN; CREATE DATABASE db2; ROLLBACK")
            .unwrap();
        let err = session
            .execute_stmt("CREATE TABLE db2.t (id INT)")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_BAD_DB);

        session
            .execute_stmt(
                "BEGIN; CREATE TABLE db1.t3 (id INT); INSERT INTO db1.t3 VALUES (7); COMMIT AND CHAIN",
            )
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t3 VALUES (8); COMMIT")
            .unwrap();
        let chained = session
            .execute_stmt("SELECT * FROM db1.t3 ORDER BY id")
            .unwrap();
        assert_eq!(
            chained.display_rows(),
            vec![vec!["7".to_string()], vec!["8".to_string()]]
        );
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
    fn test_schema_checker_and_read_committed() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(Arc::clone(&domain));
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        // BEGIN without touching the table: no MDL, concurrent DDL → 8028.
        a.execute_stmt("BEGIN").unwrap();
        b.execute_stmt("ALTER TABLE db1.t ADD COLUMN age INT")
            .unwrap();
        let err = a.execute_stmt("SELECT * FROM db1.t").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_INFO_SCHEMA_CHANGED);
        a.execute_stmt("ROLLBACK").unwrap();

        a.execute_stmt("START TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .unwrap();
        let first = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(first.display_rows()[0][1], "a");
        b.execute_stmt("UPDATE db1.t SET name = 'c' WHERE id = 1")
            .unwrap();
        let second = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(second.display_rows()[0][1], "c");
        a.execute_stmt("COMMIT").unwrap();

        a.execute_stmt("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .unwrap();
        a.execute_stmt("BEGIN").unwrap();
        let rr = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(rr.display_rows()[0][1], "c");
        b.execute_stmt("UPDATE db1.t SET name = 'd' WHERE id = 1")
            .unwrap();
        let still = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(still.display_rows()[0][1], "c");
        a.execute_stmt("ROLLBACK").unwrap();
    }

    #[test]
    fn test_mdl_blocks_concurrent_ddl() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(Arc::clone(&domain));
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("BEGIN").unwrap();
        a.execute_stmt("SELECT * FROM db1.t").unwrap();
        let err = b
            .execute_stmt("ALTER TABLE db1.t ADD COLUMN age INT")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_LOCK_WAIT_TIMEOUT);
        a.execute_stmt("COMMIT").unwrap();
        b.execute_stmt("ALTER TABLE db1.t ADD COLUMN age INT")
            .unwrap();
        let rows = b.execute_stmt("SELECT id, name, age FROM db1.t").unwrap();
        assert_eq!(rows.display_rows().len(), 1);
    }

    #[test]
    fn test_schema_lease_expired_and_reload() {
        let domain = Arc::new(Domain::new());
        let session = Session::with_domain(Arc::clone(&domain));
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
            )
            .unwrap();
        domain.set_schema_lease(std::time::Duration::from_millis(20));
        session.execute_stmt("SELECT * FROM db1.t").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        let err = session.execute_stmt("SELECT * FROM db1.t").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_INFO_SCHEMA_EXPIRED);
        domain.reload_schema_from_storage().unwrap();
        let rows = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
    }

    #[test]
    fn test_serializable_maps_to_rr_snapshot() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap();
        a.execute_stmt("BEGIN").unwrap();
        let first = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(first.display_rows()[0][1], "a");
        b.execute_stmt("UPDATE db1.t SET name = 'z' WHERE id = 1")
            .unwrap();
        // SERIALIZABLE MVB uses RR snapshot: committed update stays invisible.
        let still = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(still.display_rows()[0][1], "a");
        a.execute_stmt("COMMIT").unwrap();
        let after = a.execute_stmt("SELECT * FROM db1.t WHERE id = 1").unwrap();
        assert_eq!(after.display_rows()[0][1], "z");
    }

    #[test]
    fn test_tikv_cross_session_same_cluster_if_available() {
        // Same-cluster multi-session on live TiKV when SEALDB_TIKV=1.
        // True multi-PD / cross-cluster is out of scope (no fake PD clusters).
        if std::env::var("SEALDB_TIKV").ok().as_deref() != Some("1") {
            eprintln!("skip tikv cross-session: set SEALDB_TIKV=1 with a live cluster");
            return;
        }
        let _guard = tikv_test_lock();
        let cfg = Config {
            store: sealdb_config::StoreKind::Tikv,
            ..Config::default()
        };
        let storage = match sealdb_store::open(&cfg) {
            Ok(storage) => storage,
            Err(err) => {
                eprintln!("skip live tikv cross-session: {err}");
                return;
            }
        };
        let domain = match Domain::with_storage(storage) {
            Ok(domain) => Arc::new(domain),
            Err(err) => {
                eprintln!("skip live tikv cross-session: {err}");
                return;
            }
        };
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "DROP DATABASE IF EXISTS tikv_c3; CREATE DATABASE tikv_c3; CREATE TABLE tikv_c3.t (id INT, name VARCHAR(20)); INSERT INTO tikv_c3.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("START TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .unwrap();
        let first = a
            .execute_stmt("SELECT name FROM tikv_c3.t WHERE id = 1")
            .unwrap();
        assert_eq!(first.display_rows()[0][0], "a");
        b.execute_stmt("UPDATE tikv_c3.t SET name = 'b' WHERE id = 1")
            .unwrap();
        let second = a
            .execute_stmt("SELECT name FROM tikv_c3.t WHERE id = 1")
            .unwrap();
        assert_eq!(second.display_rows()[0][0], "b");
        a.execute_stmt("COMMIT").unwrap();
    }

    #[test]
    fn test_prepare_execute_and_plan_cache() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'")
            .unwrap();
        let first = session.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(first.columns, vec!["name"]);
        assert_eq!(first.display_rows(), vec![vec!["a".to_string()]]);
        let second = session.execute_stmt("EXECUTE s USING 2").unwrap();
        assert_eq!(second.display_rows(), vec![vec!["b".to_string()]]);
        let again = session.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(again.display_rows(), vec![vec!["a".to_string()]]);
        session.execute_stmt("DEALLOCATE PREPARE s").unwrap();
        let err = session.execute_stmt("EXECUTE s USING 1").unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_UNKNOWN_STMT_HANDLER);
    }

    #[test]
    fn test_shared_plan_cache_cross_session() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(Arc::clone(&domain));
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
        )
        .unwrap();
        a.execute_stmt("PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'")
            .unwrap();
        assert_eq!(domain.plan_cache_hit_count(), 0);
        let warmed = a.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(warmed.display_rows(), vec![vec!["a".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 0);
        b.execute_stmt("PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'")
            .unwrap();
        let hit = b.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(hit.display_rows(), vec![vec!["a".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 1);
        let again = a.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(again.display_rows(), vec![vec!["a".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 2);
    }

    #[test]
    fn test_typed_placeholders_share_plan_cache() {
        let domain = Arc::new(Domain::new());
        let session = Session::with_domain(Arc::clone(&domain));
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'")
            .unwrap();
        assert_eq!(domain.plan_cache_hit_count(), 0);
        assert_eq!(domain.plan_cache().len(), 0);
        let first = session.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(first.display_rows(), vec![vec!["a".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 0);
        assert_eq!(domain.plan_cache().len(), 1);
        let second = session.execute_stmt("EXECUTE s USING 2").unwrap();
        assert_eq!(second.display_rows(), vec![vec!["b".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 1);
        assert_eq!(domain.plan_cache().len(), 1);
        let again = session.execute_stmt("EXECUTE s USING 1").unwrap();
        assert_eq!(again.display_rows(), vec![vec!["a".to_string()]]);
        assert_eq!(domain.plan_cache_hit_count(), 2);
        assert_eq!(domain.plan_cache().len(), 1);
    }

    #[test]
    fn test_user_vars_set_select_and_execute_using() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        session.execute_stmt("SET @a = 1, @name = 'b'").unwrap();
        let got = session.execute_stmt("SELECT @a, @name, @missing").unwrap();
        assert_eq!(
            got.display_rows(),
            vec![vec!["1".to_string(), "b".to_string(), "NULL".to_string()]]
        );
        session
            .execute_stmt("PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'")
            .unwrap();
        let row = session.execute_stmt("EXECUTE s USING @a").unwrap();
        assert_eq!(row.display_rows(), vec![vec!["a".to_string()]]);
        session.execute_stmt("SET @a = 2").unwrap();
        let row2 = session.execute_stmt("EXECUTE s USING @a").unwrap();
        assert_eq!(row2.display_rows(), vec![vec!["b".to_string()]]);
    }

    #[test]
    fn test_create_drop_show_binding_and_execute() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        let unbound = session
            .execute_stmt("SELECT * FROM db1.t WHERE id > 0 ORDER BY id")
            .unwrap();
        assert_eq!(
            unbound.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        session
            .execute_stmt(
                "CREATE BINDING FOR SELECT * FROM db1.t WHERE id > 0 ORDER BY id USING SELECT * FROM db1.t WHERE id = 1 ORDER BY id",
            )
            .unwrap();
        let shown = session.execute_stmt("SHOW BINDINGS").unwrap();
        assert_eq!(shown.columns[0], "Original_sql");
        assert_eq!(shown.display_rows().len(), 1);
        assert!(shown.display_rows()[0][0].contains("id > 0"));
        assert!(shown.display_rows()[0][1].contains("id = 1"));
        let bound = session
            .execute_stmt("SELECT * FROM db1.t WHERE id > 0 ORDER BY id")
            .unwrap();
        assert_eq!(
            bound.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        session
            .execute_stmt("DROP BINDING FOR SELECT * FROM db1.t WHERE id > 0 ORDER BY id")
            .unwrap();
        let after = session
            .execute_stmt("SELECT * FROM db1.t WHERE id > 0 ORDER BY id")
            .unwrap();
        assert_eq!(
            after.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let empty = session.execute_stmt("SHOW BINDINGS").unwrap();
        assert!(empty.display_rows().is_empty());
    }

    #[test]
    fn test_set_names_and_ci_collation() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'A')",
            )
            .unwrap();
        session
            .execute_stmt("SET NAMES utf8mb4 COLLATE utf8mb4_general_ci")
            .unwrap();
        let vars = session
            .execute_stmt("SELECT @@collation_connection, @@sql_mode")
            .unwrap();
        assert!(vars.display_rows()[0][0].contains("general_ci"));
        let rows = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert_eq!(rows.display_rows().len(), 1);
        session
            .execute_stmt("SET collation_connection = utf8mb4_bin")
            .unwrap();
        let miss = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert!(miss.display_rows().is_empty());
    }

    #[test]
    fn test_set_global_sysvars_shared_domain() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));

        // Defaults match on session and global.
        let before = a
            .execute_stmt("SELECT @@sql_mode, @@GLOBAL.sql_mode")
            .unwrap();
        assert!(before.display_rows()[0][0].contains("ONLY_FULL_GROUP_BY"));
        assert_eq!(before.display_rows()[0][0], before.display_rows()[0][1]);

        // SET GLOBAL updates Domain; current session stays unchanged.
        a.execute_stmt("SET GLOBAL sql_mode = 'STRICT_TRANS_TABLES'")
            .unwrap();
        let a_sess = a
            .execute_stmt("SELECT @@sql_mode, @@SESSION.sql_mode")
            .unwrap();
        assert!(
            a_sess.display_rows()[0][0].contains("ONLY_FULL_GROUP_BY"),
            "{:?}",
            a_sess.display_rows()
        );
        assert_eq!(a_sess.display_rows()[0][0], a_sess.display_rows()[0][1]);
        let a_glob = a.execute_stmt("SELECT @@GLOBAL.sql_mode").unwrap();
        assert_eq!(a_glob.display_rows()[0][0], "STRICT_TRANS_TABLES");

        // Plain / SESSION SET stays session-local.
        a.execute_stmt("SET sql_mode = 'ONLY_FULL_GROUP_BY'")
            .unwrap();
        a.execute_stmt("SET SESSION tidb_enable_auto_analyze = OFF")
            .unwrap();
        assert_eq!(
            a.execute_stmt("SELECT @@sql_mode").unwrap().display_rows()[0][0],
            "ONLY_FULL_GROUP_BY"
        );
        assert_eq!(
            a.execute_stmt("SELECT @@GLOBAL.sql_mode")
                .unwrap()
                .display_rows()[0][0],
            "STRICT_TRANS_TABLES"
        );
        assert_eq!(
            a.execute_stmt("SELECT @@tidb_enable_auto_analyze")
                .unwrap()
                .display_rows()[0][0],
            "OFF"
        );
        assert_eq!(
            a.execute_stmt("SELECT @@GLOBAL.tidb_enable_auto_analyze")
                .unwrap()
                .display_rows()[0][0],
            "ON"
        );

        a.execute_stmt("SET GLOBAL tidb_enable_auto_analyze = OFF")
            .unwrap();
        a.execute_stmt("SET GLOBAL character_set_connection = gbk")
            .unwrap();
        assert_eq!(
            a.execute_stmt("SELECT @@GLOBAL.tidb_enable_auto_analyze")
                .unwrap()
                .display_rows()[0][0],
            "OFF"
        );
        assert_eq!(
            a.execute_stmt("SELECT @@GLOBAL.character_set_connection")
                .unwrap()
                .display_rows()[0][0],
            "gbk"
        );
        // Current session charset unchanged by SET GLOBAL.
        assert_eq!(
            a.execute_stmt("SELECT @@character_set_connection")
                .unwrap()
                .display_rows()[0][0],
            "utf8mb4"
        );

        // New session on the same Domain inherits GLOBAL defaults.
        let b = Session::with_domain(domain);
        assert_eq!(
            b.execute_stmt("SELECT @@sql_mode").unwrap().display_rows()[0][0],
            "STRICT_TRANS_TABLES"
        );
        assert_eq!(
            b.execute_stmt("SELECT @@GLOBAL.sql_mode")
                .unwrap()
                .display_rows()[0][0],
            "STRICT_TRANS_TABLES"
        );
        assert_eq!(
            b.execute_stmt("SELECT @@tidb_enable_auto_analyze")
                .unwrap()
                .display_rows()[0][0],
            "OFF"
        );
        assert_eq!(
            b.execute_stmt("SELECT @@character_set_connection")
                .unwrap()
                .display_rows()[0][0],
            "gbk"
        );
    }

    #[test]
    fn test_only_full_group_by() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); \
                 INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b')",
            )
            .unwrap();
        // Default sql_mode includes ONLY_FULL_GROUP_BY.
        let mode = session.execute_stmt("SELECT @@sql_mode").unwrap();
        assert!(
            mode.display_rows()[0][0].contains("ONLY_FULL_GROUP_BY"),
            "{:?}",
            mode.display_rows()
        );
        let err = session
            .execute_stmt("SELECT id, name, COUNT(*) FROM db1.t GROUP BY name")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_WRONG_FIELD_WITH_GROUP);
        assert!(
            err.message.contains("only_full_group_by"),
            "{}",
            err.message
        );
        // Legal under the mode: grouped column + aggregate + constant.
        let ok = session
            .execute_stmt("SELECT name, COUNT(*), 1 FROM db1.t GROUP BY name ORDER BY name")
            .unwrap();
        assert_eq!(
            ok.display_rows(),
            vec![
                vec!["a".to_string(), "2".to_string(), "1".to_string()],
                vec!["b".to_string(), "1".to_string(), "1".to_string()]
            ]
        );
        // Turning the mode off allows the previously rejected query (any-value id).
        session
            .execute_stmt("SET sql_mode = 'STRICT_TRANS_TABLES'")
            .unwrap();
        let loose = session
            .execute_stmt("SELECT id, name, COUNT(*) FROM db1.t GROUP BY name ORDER BY name")
            .unwrap();
        assert_eq!(loose.display_rows().len(), 2);
        assert_eq!(loose.display_rows()[0][1], "a");
        assert_eq!(loose.display_rows()[0][2], "2");
        assert_eq!(loose.display_rows()[1][1], "b");
        assert_eq!(loose.display_rows()[1][2], "1");
        // Re-enable and confirm rejection returns.
        session
            .execute_stmt("SET sql_mode = 'ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES'")
            .unwrap();
        let err2 = session
            .execute_stmt("SELECT id, COUNT(*) FROM db1.t GROUP BY name")
            .unwrap_err();
        assert_eq!(err2.code, sealdb_errno::ERR_WRONG_FIELD_WITH_GROUP);
    }

    #[test]
    fn test_gbk_latin1_charset_and_collation() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); \
                 INSERT INTO db1.t VALUES (1, '啊'), (2, '阿'), (3, 'A'), (4, '中')",
            )
            .unwrap();

        session.execute_stmt("SET NAMES gbk").unwrap();
        let vars = session
            .execute_stmt("SELECT @@character_set_client, @@collation_connection")
            .unwrap();
        assert_eq!(
            vars.display_rows()[0],
            vec!["gbk".to_string(), "gbk_bin".to_string()]
        );

        // gbk_bin: 啊 (B0A1) < 阿 (B0A2); case-sensitive.
        let bin_cmp = session
            .execute_stmt("SELECT '啊' < '阿', 'A' = 'a'")
            .unwrap();
        assert_eq!(
            bin_cmp.display_rows()[0],
            vec!["1".to_string(), "0".to_string()]
        );
        let bin_order = session
            .execute_stmt("SELECT name FROM db1.t WHERE id IN (1, 2) ORDER BY name, id")
            .unwrap();
        assert_eq!(
            bin_order.display_rows(),
            vec![vec!["啊".to_string()], vec!["阿".to_string()]]
        );

        session
            .execute_stmt("SET collation_connection = gbk_chinese_ci")
            .unwrap();
        // gbk_chinese_ci: 阿 (0x8453) < 啊 (0x8454); A == a.
        let ci_cmp = session
            .execute_stmt("SELECT '阿' < '啊', 'A' = 'a'")
            .unwrap();
        assert_eq!(
            ci_cmp.display_rows()[0],
            vec!["1".to_string(), "1".to_string()]
        );
        let ci_where = session
            .execute_stmt("SELECT id FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(ci_where.display_rows(), vec![vec!["3".to_string()]]);
        let ci_order = session
            .execute_stmt("SELECT name FROM db1.t WHERE id IN (1, 2) ORDER BY name, id")
            .unwrap();
        assert_eq!(
            ci_order.display_rows(),
            vec![vec!["阿".to_string()], vec!["啊".to_string()]]
        );

        session.execute_stmt("SET NAMES latin1").unwrap();
        let latin = session
            .execute_stmt("SELECT @@character_set_client, @@collation_connection")
            .unwrap();
        assert_eq!(
            latin.display_rows()[0],
            vec!["latin1".to_string(), "latin1_bin".to_string()]
        );
        let latin_eq = session.execute_stmt("SELECT 'A' = 'a', 'A' = 'A'").unwrap();
        assert_eq!(
            latin_eq.display_rows()[0],
            vec!["0".to_string(), "1".to_string()]
        );

        // Encode/decode roundtrip for the GBK / latin1 MVB subsets.
        let gbk = sealdb_parser::charset::encoding_gbk::encode("中文阿啊");
        assert_eq!(
            sealdb_parser::charset::encoding_gbk::decode(&gbk),
            "中文阿啊"
        );
        let lat = sealdb_parser::charset::encoding_latin1::encode("Aaé");
        assert_eq!(sealdb_parser::charset::encoding_latin1::decode(&lat), "Aaé");
    }

    #[test]
    fn test_unicode_ci_weights_compare_and_order() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); \
                 INSERT INTO db1.t VALUES (1, 'A'), (2, 'ä'), (3, 'b'), (4, 'ß'), (5, 'ss')",
            )
            .unwrap();
        session
            .execute_stmt("SET collation_connection = utf8mb4_unicode_ci")
            .unwrap();
        let eq = session
            .execute_stmt("SELECT 'A' = 'a', 'ä' = 'A', 'ß' = 'ss'")
            .unwrap();
        assert_eq!(
            eq.display_rows()[0],
            vec!["1".to_string(), "1".to_string(), "1".to_string()]
        );
        let where_rows = session
            .execute_stmt("SELECT id FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(
            where_rows.display_rows(),
            vec![vec!["1".to_string()], vec!["2".to_string()]]
        );
        let ordered = session
            .execute_stmt("SELECT name FROM db1.t ORDER BY name, id")
            .unwrap();
        let names: Vec<_> = ordered
            .display_rows()
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        // A/ä share primary weight; then b; ß and ss share weight.
        assert_eq!(names[0], "A");
        assert_eq!(names[1], "ä");
        assert_eq!(names[2], "b");
        assert!(names[3] == "ß" || names[3] == "ss");
        assert!(names[4] == "ß" || names[4] == "ss");
        session
            .execute_stmt("SET collation_connection = utf8mb4_bin")
            .unwrap();
        let bin_eq = session
            .execute_stmt("SELECT 'A' = 'a', 'ß' = 'ss'")
            .unwrap();
        assert_eq!(
            bin_eq.display_rows()[0],
            vec!["0".to_string(), "0".to_string()]
        );
        let bin_miss = session
            .execute_stmt("SELECT id FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert!(bin_miss.display_rows().is_empty());
    }

    /// MVB for leftover `new_character_set*` / collation suites: invalid SET NAMES,
    /// utf8mb4/gbk/latin1, SHOW COLLATION key rows, and general_ci vs bin compare.
    #[test]
    fn test_leftover_collation_suites() {
        let session = Session::new();

        let bad_cs = session.execute_stmt("SET NAMES nosuchcharset").unwrap_err();
        assert_eq!(bad_cs.code, sealdb_errno::ERR_UNKNOWN_CHARACTER_SET);
        assert!(
            bad_cs.message.contains("nosuchcharset"),
            "{}",
            bad_cs.message
        );
        let bad_col = session
            .execute_stmt("SET collation_connection = utf8mb4_nosuch_ci")
            .unwrap_err();
        assert_eq!(bad_col.code, sealdb_errno::ERR_UNKNOWN_COLLATION);
        assert!(
            bad_col.message.contains("utf8mb4_nosuch_ci"),
            "{}",
            bad_col.message
        );

        for (charset, coll) in [
            ("utf8mb4", "utf8mb4_bin"),
            ("gbk", "gbk_bin"),
            ("latin1", "latin1_bin"),
        ] {
            session
                .execute_stmt(&format!("SET NAMES {charset}"))
                .unwrap();
            let vars = session
                .execute_stmt("SELECT @@character_set_client, @@collation_connection")
                .unwrap();
            assert_eq!(
                vars.display_rows()[0],
                vec![charset.to_string(), coll.to_string()],
                "SET NAMES {charset}"
            );
        }

        let show = session.execute_stmt("SHOW COLLATION").unwrap();
        let names: Vec<_> = show
            .display_rows()
            .into_iter()
            .filter_map(|r| r.first().cloned())
            .collect();
        for key in [
            "utf8mb4_bin",
            "utf8mb4_general_ci",
            "utf8mb4_unicode_ci",
            "gbk_bin",
            "gbk_chinese_ci",
            "latin1_bin",
        ] {
            assert!(
                names.iter().any(|n| n == key),
                "SHOW COLLATION missing {key}: {names:?}"
            );
        }
        let is_rows = session
            .execute_stmt(
                "SELECT COLLATION_NAME FROM information_schema.collations \
                 WHERE COLLATION_NAME IN ('utf8mb4_bin','utf8mb4_general_ci','gbk_bin')",
            )
            .unwrap();
        assert_eq!(is_rows.display_rows().len(), 3);

        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); \
                 INSERT INTO db1.t VALUES (1, 'A')",
            )
            .unwrap();
        session
            .execute_stmt("SET NAMES utf8mb4 COLLATE utf8mb4_general_ci")
            .unwrap();
        let ci_hit = session
            .execute_stmt("SELECT id FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert_eq!(ci_hit.display_rows(), vec![vec!["1".to_string()]]);
        session
            .execute_stmt("SET collation_connection = utf8mb4_bin")
            .unwrap();
        let bin_miss = session
            .execute_stmt("SELECT id FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert!(bin_miss.display_rows().is_empty());
        let bin_eq = session.execute_stmt("SELECT 'A' = 'a', 'A' = 'A'").unwrap();
        assert_eq!(
            bin_eq.display_rows()[0],
            vec!["0".to_string(), "1".to_string()]
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
    fn test_select_for_share_lock() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(Arc::clone(&domain));
        let c = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
        )
        .unwrap();
        a.execute_stmt("BEGIN PESSIMISTIC").unwrap();
        let share = a.execute_stmt("SELECT * FROM db1.t FOR SHARE").unwrap();
        assert_eq!(
            share.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        b.execute_stmt("BEGIN PESSIMISTIC").unwrap();
        let share_mode = b
            .execute_stmt("SELECT * FROM db1.t LOCK IN SHARE MODE")
            .unwrap();
        assert_eq!(
            share_mode.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let err = c
            .execute_stmt("BEGIN PESSIMISTIC; SELECT * FROM db1.t FOR UPDATE")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_LOCK_WAIT_TIMEOUT);
        c.execute_stmt("ROLLBACK").unwrap();
        a.execute_stmt("COMMIT").unwrap();
        b.execute_stmt("COMMIT").unwrap();
        let exclusive = c.execute_stmt("SELECT * FROM db1.t FOR UPDATE").unwrap();
        assert_eq!(
            exclusive.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
    }

    #[test]
    fn test_select_nowait_and_skip_locked() {
        let domain = Arc::new(Domain::new());
        let a = Session::with_domain(Arc::clone(&domain));
        let b = Session::with_domain(domain);
        a.execute_stmt(
            "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
        )
        .unwrap();
        a.execute_stmt("BEGIN PESSIMISTIC").unwrap();
        a.execute_stmt("SELECT * FROM db1.t WHERE id = 1 FOR UPDATE")
            .unwrap();
        let err = b
            .execute_stmt("SELECT * FROM db1.t WHERE id = 1 FOR UPDATE NOWAIT")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_LOCK_NOWAIT);
        let skipped = b
            .execute_stmt("SELECT * FROM db1.t ORDER BY id FOR UPDATE SKIP LOCKED")
            .unwrap();
        assert_eq!(
            skipped.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        a.execute_stmt("COMMIT").unwrap();
        let all = b
            .execute_stmt("SELECT * FROM db1.t ORDER BY id FOR UPDATE SKIP LOCKED")
            .unwrap();
        assert_eq!(all.display_rows().len(), 2);
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
        let union_exists = session
            .execute_stmt("SELECT 1 WHERE EXISTS (SELECT 2 UNION SELECT 3)")
            .unwrap();
        assert_eq!(union_exists.display_rows(), vec![vec!["1".to_string()]]);
        let union_in = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id IN (SELECT 1 UNION SELECT 2) ORDER BY id")
            .unwrap();
        assert_eq!(
            union_in.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let err = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id = (SELECT id FROM db1.t2)")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_SUBQUERY_NO_1_ROW);
        let any_eq = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id = ANY (SELECT id FROM db1.t2) ORDER BY id")
            .unwrap();
        assert_eq!(
            any_eq.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let explain_exists = session
            .execute_stmt(
                "EXPLAIN SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id)",
            )
            .unwrap();
        let explain_text = explain_exists
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            explain_text.contains("Semi"),
            "expected HashJoin Semi, got {explain_text}"
        );
        assert!(
            explain_text.contains("HashAgg"),
            "expected Distinct/HashAgg on Semi inner, got {explain_text}"
        );
        assert!(
            !explain_text.contains("Apply"),
            "EXISTS should not stay Apply, got {explain_text}"
        );
        let explain_anti = session
            .execute_stmt(
                "EXPLAIN SELECT * FROM db1.t1 WHERE NOT EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id)",
            )
            .unwrap();
        let anti_text = explain_anti
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            anti_text.contains("Anti"),
            "expected HashJoin Anti, got {anti_text}"
        );
        let explain_in = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t1 WHERE id IN (SELECT id FROM db1.t2)")
            .unwrap();
        let in_text = explain_in
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            in_text.contains("Semi"),
            "expected HashJoin Semi for IN, got {in_text}"
        );
        assert!(
            in_text.contains("HashAgg"),
            "expected Distinct/HashAgg on IN inner, got {in_text}"
        );
    }

    #[test]
    fn test_any_all_subquery() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y')",
            )
            .unwrap();
        let some_eq = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE id = SOME (SELECT id FROM db1.t2) ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            some_eq.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let ne_all = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE id <> ALL (SELECT id FROM db1.t2) ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            ne_all.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let gt_any = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id > ANY (SELECT id FROM db1.t2) ORDER BY id")
            .unwrap();
        assert_eq!(
            gt_any.display_rows(),
            vec![vec!["2".to_string(), "b".to_string()]]
        );
        let gt_all = session
            .execute_stmt("SELECT * FROM db1.t1 WHERE id > ALL (SELECT id FROM db1.t2) ORDER BY id")
            .unwrap();
        assert!(gt_all.display_rows().is_empty());
        let empty_all = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE id > ALL (SELECT id FROM db1.t2 WHERE id = 99) ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            empty_all.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let empty_any = session
            .execute_stmt(
                "SELECT * FROM db1.t1 WHERE id > ANY (SELECT id FROM db1.t2 WHERE id = 99)",
            )
            .unwrap();
        assert!(empty_any.display_rows().is_empty());
        let explain_any = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t1 WHERE id = ANY (SELECT id FROM db1.t2)")
            .unwrap();
        let any_text = explain_any
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            any_text.contains("Semi"),
            "expected HashJoin Semi for = ANY, got {any_text}"
        );
        assert!(
            any_text.contains("HashAgg"),
            "expected Distinct/HashAgg on = ANY inner, got {any_text}"
        );
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
        let except = session.execute_stmt("SELECT 1 EXCEPT SELECT 2").unwrap();
        assert_eq!(except.display_rows(), vec![vec!["1".to_string()]]);
        let except_empty = session.execute_stmt("SELECT 1 EXCEPT SELECT 1").unwrap();
        assert!(except_empty.display_rows().is_empty());
        let intersect = session.execute_stmt("SELECT 1 INTERSECT SELECT 1").unwrap();
        assert_eq!(intersect.display_rows(), vec![vec!["1".to_string()]]);
        let except_all = session
            .execute_stmt("SELECT 1 UNION ALL SELECT 1 EXCEPT ALL SELECT 1")
            .unwrap();
        assert_eq!(except_all.display_rows(), vec![vec!["1".to_string()]]);
        let nested = session
            .execute_stmt("SELECT 1 UNION (SELECT 2 UNION SELECT 3)")
            .unwrap();
        assert_eq!(
            nested.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
        let paren = session
            .execute_stmt("(SELECT 1 UNION SELECT 2) UNION SELECT 3")
            .unwrap();
        assert_eq!(
            paren.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
    }

    #[test]
    fn test_parallel_union() {
        let session = Session::new();
        let rows = session
            .execute_stmt("SELECT 1 UNION SELECT 2 UNION SELECT 3")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
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
        let running = session
            .execute_stmt(
                "SELECT id, SUM(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM db1.t ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            running.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string()],
                vec!["2".to_string(), "3".to_string()],
                vec!["3".to_string(), "6".to_string()],
                vec!["4".to_string(), "10".to_string()]
            ]
        );
        let named = session
            .execute_stmt(
                "SELECT id, name, ROW_NUMBER() OVER w FROM db1.t WINDOW w AS (PARTITION BY name ORDER BY id) ORDER BY name, id",
            )
            .unwrap();
        assert_eq!(
            named.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string(), "1".to_string()],
                vec!["2".to_string(), "a".to_string(), "2".to_string()],
                vec!["4".to_string(), "a".to_string(), "3".to_string()],
                vec!["3".to_string(), "b".to_string(), "1".to_string()]
            ]
        );
        let sliding = session
            .execute_stmt(
                "SELECT id, SUM(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM db1.t ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            sliding.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string()],
                vec!["2".to_string(), "3".to_string()],
                vec!["3".to_string(), "5".to_string()],
                vec!["4".to_string(), "7".to_string()]
            ]
        );
        let lag = session
            .execute_stmt("SELECT id, LAG(id) OVER (ORDER BY id), LEAD(id, 1, 0) OVER (ORDER BY id) FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(
            lag.display_rows(),
            vec![
                vec!["1".to_string(), "NULL".to_string(), "2".to_string()],
                vec!["2".to_string(), "1".to_string(), "3".to_string()],
                vec!["3".to_string(), "2".to_string(), "4".to_string()],
                vec!["4".to_string(), "3".to_string(), "0".to_string()]
            ]
        );
        let first = session
            .execute_stmt(
                "SELECT id, FIRST_VALUE(id) OVER (ORDER BY id), LAST_VALUE(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM db1.t ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            first.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string(), "4".to_string()],
                vec!["2".to_string(), "1".to_string(), "4".to_string()],
                vec!["3".to_string(), "1".to_string(), "4".to_string()],
                vec!["4".to_string(), "1".to_string(), "4".to_string()]
            ]
        );
        session
            .execute_stmt("INSERT INTO db1.t VALUES (5, NULL)")
            .unwrap();
        let ignore = session
            .execute_stmt(
                "SELECT id, FIRST_VALUE(name) IGNORE NULLS OVER (ORDER BY id) FROM db1.t ORDER BY id",
            )
            .unwrap();
        assert_eq!(ignore.display_rows()[0][1], "a".to_string());
        assert_eq!(ignore.display_rows()[4][1], "a".to_string());
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
        let rec_seed = session
            .execute_stmt("WITH RECURSIVE c AS (SELECT 1) SELECT * FROM c")
            .unwrap();
        assert_eq!(rec_seed.display_rows(), vec![vec!["1".to_string()]]);
        let rec = session
            .execute_stmt(
                "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT * FROM c ORDER BY n",
            )
            .unwrap();
        assert_eq!(
            rec.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
        let mat = session
            .execute_stmt("WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM c")
            .unwrap();
        assert_eq!(mat.display_rows(), vec![vec!["1".to_string()]]);
        let inline = session
            .execute_stmt("WITH c AS NOT MATERIALIZED (SELECT 1 AS n) SELECT * FROM c")
            .unwrap();
        assert_eq!(inline.display_rows(), vec![vec!["1".to_string()]]);
        // Dual seeds always inline in EXPLAIN (seed_is_dual_projection), even
        // with AS MATERIALIZED; table-backed MATERIALIZED keeps the CTE node.
        let mat_plan = session
            .execute_stmt(
                "EXPLAIN WITH c AS MATERIALIZED (SELECT id FROM db1.t WHERE id = 1) SELECT * FROM c",
            )
            .unwrap();
        let mat_text = mat_plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(mat_text.contains("CTE"), "{mat_text}");
        let inline_plan = session
            .execute_stmt("EXPLAIN WITH c AS NOT MATERIALIZED (SELECT 1 AS n) SELECT * FROM c")
            .unwrap();
        let inline_text = inline_plan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!inline_text.contains("CTE"), "{inline_text}");
        let union_cte = session
            .execute_stmt("WITH c AS (SELECT 1 AS n UNION SELECT 2) SELECT * FROM c ORDER BY n")
            .unwrap();
        assert_eq!(
            union_cte.display_rows(),
            vec![vec!["1".to_string()], vec!["2".to_string()]]
        );
        let seed_names = session
            .execute_stmt(
                "WITH RECURSIVE cte2 AS (SELECT 1 AS col_1, 2 AS col_2) SELECT * FROM cte2",
            )
            .unwrap();
        assert_eq!(
            seed_names.display_rows(),
            vec![vec!["1".to_string(), "2".to_string()]]
        );
        let where_bare = session
            .execute_stmt(
                "WITH RECURSIVE cte2 AS (SELECT 1 AS col_1, 2 AS col_2) SELECT col_1, col_2 FROM cte2 WHERE col_2 = 2",
            )
            .unwrap();
        assert_eq!(
            where_bare.display_rows(),
            vec![vec!["1".to_string(), "2".to_string()]]
        );
        let self_join = session
            .execute_stmt(
                "WITH RECURSIVE cte2 AS (SELECT 1 AS col_1, 2 AS col_2) SELECT c1.col_1, c2.col_2 FROM cte2 AS c1, cte2 AS c2 WHERE c2.col_2 = 1",
            )
            .unwrap();
        assert!(
            self_join.display_rows().is_empty(),
            "self-join WHERE should filter all rows: {:?}",
            self_join.display_rows()
        );
        let mutual = session
            .execute_stmt(
                "WITH RECURSIVE cte (c1) AS (SELECT 1), cte1 (c2) AS (SELECT 1 UNION SELECT c1 + 1 FROM cte, cte1) SELECT * FROM cte, cte1",
            )
            .unwrap();
        assert_eq!(
            mutual.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string()],
                vec!["1".to_string(), "2".to_string()],
            ]
        );
        let no_self_ref = session
            .execute_stmt(
                "WITH RECURSIVE qn AS (SELECT 1 FROM DUAL UNION ALL SELECT 1 FROM DUAL) SELECT * FROM qn",
            )
            .unwrap();
        assert_eq!(
            no_self_ref.display_rows(),
            vec![vec!["1".to_string()], vec!["1".to_string()]]
        );
        let group_only = session
            .execute_stmt("SELECT 1 AS a FROM DUAL GROUP BY a")
            .unwrap();
        assert_eq!(group_only.display_rows(), vec![vec!["1".to_string()]]);
        let group_seed_only = session
            .execute_stmt(
                "WITH RECURSIVE qn AS (SELECT 1 AS a FROM DUAL GROUP BY a) SELECT * FROM qn",
            )
            .unwrap();
        assert_eq!(group_seed_only.display_rows(), vec![vec!["1".to_string()]]);
        let group_seed = session
            .execute_stmt(
                "WITH RECURSIVE qn AS (SELECT 1 AS a FROM DUAL GROUP BY a UNION ALL SELECT a+1 FROM qn WHERE a<3) SELECT * FROM qn",
            )
            .unwrap();
        assert_eq!(
            group_seed.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()],
            ]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.employees (ID INT PRIMARY KEY, NAME VARCHAR(100), MANAGER_ID INT); INSERT INTO db1.employees VALUES (333, 'Yasmina', NULL), (198, 'John', 333), (29, 'Pedro', 198), (72, 'Pierre', 29)",
            )
            .unwrap();
        let hierarchy = session
            .execute_stmt(
                "WITH RECURSIVE employees_extended AS (SELECT ID, NAME, MANAGER_ID, CAST(ID AS CHAR(200)) AS PATH FROM db1.employees WHERE NAME='Pierre' UNION ALL SELECT S.ID, S.NAME, S.MANAGER_ID, CONCAT(M.PATH, ',', S.ID) FROM employees_extended M JOIN db1.employees S ON M.MANAGER_ID=S.ID) SELECT ID, NAME, MANAGER_ID, PATH FROM employees_extended",
            )
            .unwrap();
        assert_eq!(
            hierarchy.display_rows(),
            vec![
                vec![
                    "72".to_string(),
                    "Pierre".to_string(),
                    "29".to_string(),
                    "72".to_string(),
                ],
                vec![
                    "29".to_string(),
                    "Pedro".to_string(),
                    "198".to_string(),
                    "72,29".to_string(),
                ],
                vec![
                    "198".to_string(),
                    "John".to_string(),
                    "333".to_string(),
                    "72,29,198".to_string(),
                ],
                vec![
                    "333".to_string(),
                    "Yasmina".to_string(),
                    "NULL".to_string(),
                    "72,29,198,333".to_string(),
                ],
            ]
        );
        let joined = session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id = 1) SELECT t.id, t.name FROM db1.t JOIN c ON t.id = c.id",
            )
            .unwrap();
        assert_eq!(
            joined.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let both = session
            .execute_stmt(
                "WITH a AS (SELECT 1 AS id), b AS (SELECT 1 AS id) SELECT a.id FROM a JOIN b ON a.id = b.id",
            )
            .unwrap();
        assert_eq!(both.display_rows(), vec![vec!["1".to_string()]]);
        let shared = session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id = 1) SELECT * FROM c UNION ALL SELECT * FROM c",
            )
            .unwrap();
        assert_eq!(
            shared.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["1".to_string(), "a".to_string()]
            ]
        );
        session
            .execute_stmt(
                "WITH c AS (SELECT 4 AS id, 'd' AS name) INSERT INTO db1.t SELECT * FROM c",
            )
            .unwrap();
        let inserted = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 4")
            .unwrap();
        assert_eq!(
            inserted.display_rows(),
            vec![vec!["4".to_string(), "d".to_string()]]
        );
        session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id = 4) UPDATE db1.t JOIN c ON t.id = c.id SET t.name = 'z'",
            )
            .unwrap();
        let updated = session
            .execute_stmt("SELECT name FROM db1.t WHERE id = 4")
            .unwrap();
        assert_eq!(updated.display_rows(), vec![vec!["z".to_string()]]);
        session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id = 4) DELETE db1.t FROM db1.t JOIN c ON t.id = c.id",
            )
            .unwrap();
        let deleted = session
            .execute_stmt("SELECT COUNT(*) FROM db1.t WHERE id = 4")
            .unwrap();
        assert_eq!(deleted.display_rows(), vec![vec!["0".to_string()]]);
        session.execute_stmt("DROP TABLE IF EXISTS db1.tbl_0").unwrap();
        session
            .execute_stmt("CREATE TABLE db1.tbl_0(a int)")
            .unwrap();
        session.execute_stmt("USE db1").unwrap();
        let shadow_err = session.execute_stmt(
            "with recursive tbl_0 (col_943,col_944,col_945,col_946,col_947) AS ( with recursive tbl_0 (col_948,col_949,col_950,col_951,col_952) AS ( select 1, 2,3,4,5 UNION ALL select col_948 + 1,col_949 + 1,col_950 + 1,col_951 + 1,col_952 + 1 from tbl_0 where col_948 < 5 ) select col_948,col_949,col_951,col_950,col_952 from tbl_0 UNION ALL select col_943 + 1,col_944 + 1,col_945 + 1,col_946 + 1,col_947 + 1 from tbl_0 where col_943 < 5 ) select * from tbl_0",
        ).expect_err("same-name nested WITH should errno 1054");
        assert_eq!(shadow_err.code, 1054, "{shadow_err:?}");
        assert!(shadow_err.message.contains("col_943"), "{shadow_err:?}");
        let err = session
            .execute_stmt("WITH c(a, b) AS (SELECT 1) SELECT * FROM c")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_VIEW_WRONG_LIST);
    }


    #[test]
    fn test_derived_table() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let dual = session
            .execute_stmt("SELECT * FROM (SELECT 1 AS id) d")
            .unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
        let filtered = session
            .execute_stmt("SELECT * FROM (SELECT * FROM db1.t WHERE id < 3) d ORDER BY id")
            .unwrap();
        assert_eq!(
            filtered.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let projected = session
            .execute_stmt("SELECT id FROM (SELECT id, name FROM db1.t) d ORDER BY id")
            .unwrap();
        assert_eq!(
            projected.display_rows(),
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
        let nested = session
            .execute_stmt("SELECT * FROM (SELECT * FROM (SELECT id FROM db1.t WHERE id = 1) a) b")
            .unwrap();
        assert_eq!(nested.display_rows(), vec![vec!["1".to_string()]]);
        let aliased = session
            .execute_stmt("SELECT x FROM (SELECT 2) AS d(x)")
            .unwrap();
        assert_eq!(aliased.display_rows(), vec![vec!["2".to_string()]]);
        let counted = session
            .execute_stmt("SELECT COUNT(*) FROM (SELECT * FROM db1.t) d")
            .unwrap();
        assert_eq!(counted.display_rows(), vec![vec!["3".to_string()]]);
        let with_outer = session
            .execute_stmt(
                "WITH c AS (SELECT * FROM db1.t WHERE id = 1) SELECT * FROM (SELECT * FROM c) d",
            )
            .unwrap();
        assert_eq!(
            with_outer.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let joined = session
            .execute_stmt("SELECT * FROM db1.t JOIN (SELECT 1 AS id) d ON t.id = d.id")
            .unwrap();
        assert_eq!(
            joined.display_rows(),
            vec![vec!["1".to_string(), "a".to_string(), "1".to_string()]]
        );
    }

    #[test]
    fn test_select_list_subquery() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y')",
            )
            .unwrap();
        let dual = session.execute_stmt("SELECT (SELECT 1)").unwrap();
        assert_eq!(dual.display_rows(), vec![vec!["1".to_string()]]);
        let correlated = session
            .execute_stmt(
                "SELECT id, (SELECT tag FROM db1.t2 WHERE t2.id = t1.id) FROM db1.t1 ORDER BY id",
            )
            .unwrap();
        assert_eq!(
            correlated.display_rows(),
            vec![
                vec!["1".to_string(), "x".to_string()],
                vec!["2".to_string(), "NULL".to_string()]
            ]
        );
        let uncorrelated = session
            .execute_stmt("SELECT id, (SELECT COUNT(*) FROM db1.t2) FROM db1.t1 ORDER BY id")
            .unwrap();
        assert_eq!(
            uncorrelated.display_rows(),
            vec![
                vec!["1".to_string(), "2".to_string()],
                vec!["2".to_string(), "2".to_string()]
            ]
        );
        let err = session
            .execute_stmt("SELECT (SELECT id FROM db1.t2) FROM db1.t1")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_SUBQUERY_NO_1_ROW);
        let union_scalar = session
            .execute_stmt("SELECT (SELECT 1 UNION SELECT 1)")
            .unwrap();
        assert_eq!(union_scalar.display_rows(), vec![vec!["1".to_string()]]);
        let union_scalar_err = session
            .execute_stmt("SELECT (SELECT 1 UNION ALL SELECT 2)")
            .unwrap_err();
        assert_eq!(union_scalar_err.code, sealdb_errno::ERR_SUBQUERY_NO_1_ROW);
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
        session
            .execute_stmt("CREATE UNIQUE INDEX idx_id ON db1.t (id)")
            .unwrap();
        let dup = session
            .execute_stmt("INSERT INTO db1.t VALUES (1, 'dup')")
            .unwrap_err();
        assert_eq!(dup.code, sealdb_errno::ERR_DUP_ENTRY);
        session
            .execute_stmt("INSERT INTO db1.t VALUES (4, 'a')")
            .unwrap();
        let uniq_backfill = session
            .execute_stmt("ALTER TABLE db1.t ADD UNIQUE INDEX idx_name_u (name)")
            .unwrap_err();
        assert_eq!(uniq_backfill.code, sealdb_errno::ERR_DUP_ENTRY);
        session
            .execute_stmt("ALTER TABLE db1.t CHANGE name title VARCHAR(20)")
            .unwrap();
        let renamed = session
            .execute_stmt("SELECT title FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(renamed.display_rows(), vec![vec!["a".to_string()]]);
    }

    #[test]
    fn test_change_modify_rename() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), age VARCHAR(8)); INSERT INTO db1.t VALUES (1, 'alice', '10'); CREATE INDEX idx_name ON db1.t (name)",
            )
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t CHANGE name title VARCHAR(20)")
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t MODIFY age INT")
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t RENAME COLUMN title TO name")
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t RENAME INDEX idx_name TO idx_title")
            .unwrap();
        let row = session
            .execute_stmt("SELECT name, age FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(
            row.display_rows(),
            vec![vec!["alice".to_string(), "10".to_string()]]
        );
        session
            .execute_stmt("INSERT INTO db1.t VALUES (2, 'bob', 11)")
            .unwrap();
        let by_index = session
            .execute_stmt("SELECT name FROM db1.t WHERE name = 'alice'")
            .unwrap();
        assert_eq!(by_index.display_rows(), vec![vec!["alice".to_string()]]);
        session
            .execute_stmt("ALTER TABLE db1.t RENAME TO t2")
            .unwrap();
        let moved = session
            .execute_stmt("SELECT name, age FROM db1.t2 WHERE id = 2")
            .unwrap();
        assert_eq!(
            moved.display_rows(),
            vec![vec!["bob".to_string(), "11".to_string()]]
        );
        session
            .execute_stmt("RENAME TABLE db1.t2 TO db1.t")
            .unwrap();
        assert!(session.infoschema().table_by_name("db1", "t").is_some());
        assert!(session.infoschema().table_by_name("db1", "t2").is_none());
        let table = session
            .infoschema()
            .table_by_name("db1", "t")
            .unwrap()
            .clone();
        let idx = table
            .indices
            .iter()
            .find(|idx| idx.name.o() == "idx_title")
            .unwrap();
        assert_eq!(idx.columns[0].name.o(), "name");
    }

    #[test]
    fn test_after_first() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
            )
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t ADD COLUMN extra INT AFTER id")
            .unwrap();
        let after = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(after.columns, vec!["id", "extra", "name"]);
        assert_eq!(
            after.display_rows(),
            vec![vec!["1".to_string(), "NULL".to_string(), "a".to_string()]]
        );
        session
            .execute_stmt("ALTER TABLE db1.t ADD COLUMN z INT FIRST")
            .unwrap();
        let first = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(first.columns, vec!["z", "id", "extra", "name"]);
        session
            .execute_stmt("ALTER TABLE db1.t MODIFY extra VARCHAR(8) AFTER name")
            .unwrap();
        let moved = session.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(moved.columns, vec!["z", "id", "name", "extra"]);
        let hit = session
            .execute_stmt("SELECT id, name FROM db1.t WHERE id = 1")
            .unwrap();
        assert_eq!(
            hit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
    }

    #[test]
    fn test_algorithm_lock() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a')",
            )
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.t ADD COLUMN age INT, ALGORITHM=INPLACE, LOCK=NONE")
            .unwrap();
        session
            .execute_stmt("CREATE INDEX idx_age ON db1.t (age) ALGORITHM=INPLACE LOCK=NONE")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (2, 'b', 10)")
            .unwrap();
        let hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE age = 10")
            .unwrap();
        assert_eq!(
            hit.display_rows(),
            vec![vec!["2".to_string(), "b".to_string(), "10".to_string()]]
        );
    }

    #[test]
    fn test_partition_prune() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN (MAXVALUE)); INSERT INTO db1.t VALUES (1, 'a'), (15, 'b'), (25, 'c')",
            )
            .unwrap();
        let table = session
            .infoschema()
            .table_by_name("db1", "t")
            .unwrap()
            .clone();
        assert_eq!(table.partition.as_ref().unwrap().definitions.len(), 3);
        let low = session
            .execute_stmt("SELECT * FROM db1.t WHERE id < 10 ORDER BY id")
            .unwrap();
        assert_eq!(
            low.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        let mid = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 15")
            .unwrap();
        assert_eq!(
            mid.display_rows(),
            vec![vec!["15".to_string(), "b".to_string()]]
        );
        let high = session
            .execute_stmt("SELECT * FROM db1.t WHERE id >= 20 ORDER BY id")
            .unwrap();
        assert_eq!(
            high.display_rows(),
            vec![vec!["25".to_string(), "c".to_string()]]
        );
        let all = session
            .execute_stmt("SELECT * FROM db1.t ORDER BY id")
            .unwrap();
        assert_eq!(all.display_rows().len(), 3);
        session
            .execute_stmt("CREATE INDEX idx_name ON db1.t (name)")
            .unwrap();
        let named = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'b'")
            .unwrap();
        assert_eq!(
            named.display_rows(),
            vec![vec!["15".to_string(), "b".to_string()]]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.h (id INT, name VARCHAR(20)) PARTITION BY HASH (id) PARTITIONS 4; INSERT INTO db1.h VALUES (1, 'x'), (2, 'y'), (5, 'z')",
            )
            .unwrap();
        let hashed = session
            .execute_stmt("SELECT * FROM db1.h WHERE id = 5")
            .unwrap();
        assert_eq!(
            hashed.display_rows(),
            vec![vec!["5".to_string(), "z".to_string()]]
        );
        // Dynamic (default) defers prune to execute; static prunes at plan time.
        let mode = session
            .execute_stmt("SELECT @@tidb_partition_prune_mode")
            .unwrap();
        assert_eq!(mode.display_rows(), vec![vec!["dynamic".to_string()]]);
        session
            .execute_stmt("SET tidb_partition_prune_mode = 'static'")
            .unwrap();
        let static_hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE id < 10 ORDER BY id")
            .unwrap();
        assert_eq!(
            static_hit.display_rows(),
            vec![vec!["1".to_string(), "a".to_string()]]
        );
        session
            .execute_stmt("SET tidb_partition_prune_mode = 'dynamic'")
            .unwrap();
        let dyn_hit = session
            .execute_stmt("SELECT * FROM db1.t WHERE id = 15")
            .unwrap();
        assert_eq!(
            dyn_hit.display_rows(),
            vec![vec!["15".to_string(), "b".to_string()]]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.r (id INT) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (5))",
            )
            .unwrap();
        let err = session
            .execute_stmt("INSERT INTO db1.r VALUES (9)")
            .unwrap_err();
        assert_eq!(err.code, sealdb_errno::ERR_NO_PARTITION_FOR_GIVEN_VALUE);
        session
            .execute_stmt(
                "CREATE TABLE db1.l (id INT, name VARCHAR(20)) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (10)); INSERT INTO db1.l VALUES (1, 'a'), (10, 'b')",
            )
            .unwrap();
        let listed = session
            .execute_stmt("SELECT * FROM db1.l WHERE id = 10")
            .unwrap();
        assert_eq!(
            listed.display_rows(),
            vec![vec!["10".to_string(), "b".to_string()]]
        );
        let miss = session
            .execute_stmt("INSERT INTO db1.l VALUES (3, 'c')")
            .unwrap_err();
        assert_eq!(miss.code, sealdb_errno::ERR_NO_PARTITION_FOR_GIVEN_VALUE);
        session
            .execute_stmt(
                "CREATE TABLE db1.k (id INT, name VARCHAR(20)) PARTITION BY KEY (id) PARTITIONS 4; INSERT INTO db1.k VALUES (1, 'x'), (5, 'z')",
            )
            .unwrap();
        let keyed = session
            .execute_stmt("SELECT * FROM db1.k WHERE id = 5")
            .unwrap();
        assert_eq!(
            keyed.display_rows(),
            vec![vec!["5".to_string(), "z".to_string()]]
        );
        session
            .execute_stmt(
                "CREATE TABLE db1.rc (a INT, b INT, name VARCHAR(20)) PARTITION BY RANGE COLUMNS (a, b) (PARTITION p0 VALUES LESS THAN (10, 20), PARTITION p1 VALUES LESS THAN (MAXVALUE, MAXVALUE)); INSERT INTO db1.rc VALUES (1, 1, 'a'), (10, 30, 'b')",
            )
            .unwrap();
        let rc = session
            .execute_stmt("SELECT name FROM db1.rc WHERE a = 10")
            .unwrap();
        assert_eq!(rc.display_rows(), vec![vec!["b".to_string()]]);
        session
            .execute_stmt(
                "CREATE TABLE db1.he (id INT, name VARCHAR(20)) PARTITION BY HASH (id % 4) PARTITIONS 4; INSERT INTO db1.he VALUES (1, 'a'), (5, 'b')",
            )
            .unwrap();
        let he = session
            .execute_stmt("SELECT name FROM db1.he WHERE id = 5")
            .unwrap();
        assert_eq!(he.display_rows(), vec![vec!["b".to_string()]]);
        session
            .execute_stmt(
                "CREATE TABLE db1.re (id INT, name VARCHAR(20)) PARTITION BY RANGE (ABS(id)) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (MAXVALUE)); INSERT INTO db1.re VALUES (-3, 'n'), (15, 'p')",
            )
            .unwrap();
        let re = session
            .execute_stmt("SELECT name FROM db1.re WHERE id = -3")
            .unwrap();
        assert_eq!(re.display_rows(), vec![vec!["n".to_string()]]);
        let table = session
            .infoschema()
            .table_by_name("db1", "he")
            .unwrap()
            .clone();
        let part = table.partition.as_ref().unwrap();
        assert!(part.is_expr_key());
        assert_eq!(part.expr, "id % 4");
        session
            .execute_stmt(
                "CREATE TABLE db1.ap (id INT, name VARCHAR(20)) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (MAXVALUE)); INSERT INTO db1.ap VALUES (1, 'a'), (15, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("ALTER TABLE db1.ap ADD PARTITION (PARTITION p2 VALUES LESS THAN (20))")
            .unwrap();
        assert_eq!(
            session
                .infoschema()
                .table_by_name("db1", "ap")
                .unwrap()
                .partition
                .as_ref()
                .unwrap()
                .definitions
                .len(),
            3
        );
        session
            .execute_stmt("ALTER TABLE db1.ap DROP PARTITION p0")
            .unwrap();
        let after_drop = session
            .execute_stmt("SELECT * FROM db1.ap ORDER BY id")
            .unwrap();
        assert_eq!(
            after_drop.display_rows(),
            vec![vec!["15".to_string(), "b".to_string()]]
        );
        assert_eq!(
            session
                .infoschema()
                .table_by_name("db1", "ap")
                .unwrap()
                .partition
                .as_ref()
                .unwrap()
                .definitions
                .len(),
            2
        );
    }

    #[test]
    fn test_grant_revoke() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); GRANT SELECT ON db1.t TO 'u'@'%' WITH GRANT OPTION",
            )
            .unwrap();
        let grants = session.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert!(
            grants.display_rows().iter().any(|row| {
                row[0].contains("SELECT") && row[0].contains("u") && row[0].contains("GRANT OPTION")
            }),
            "{:?}",
            grants.display_rows()
        );
        session
            .execute_stmt("REVOKE GRANT OPTION FOR SELECT ON db1.t FROM 'u'@'%'")
            .unwrap();
        let no_opt = session.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert!(
            no_opt
                .display_rows()
                .iter()
                .any(|row| row[0].contains("SELECT") && !row[0].contains("GRANT OPTION")),
            "{:?}",
            no_opt.display_rows()
        );
        session
            .execute_stmt("REVOKE SELECT ON db1.t FROM 'u'@'%'")
            .unwrap();
        let empty = session.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert!(empty.display_rows().is_empty());
        session
            .execute_stmt("CREATE ROLE r; GRANT SELECT ON db1.t TO r; GRANT r TO 'u'@'%'")
            .unwrap();
        let role_grants = session.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert!(
            role_grants
                .display_rows()
                .iter()
                .any(|row| row[0].contains("GRANT") && row[0].contains("'r'@'%'")),
            "{:?}",
            role_grants.display_rows()
        );

        let checked = Session::with_grant_table();
        checked
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); CREATE ROLE r; GRANT SELECT ON db1.t TO r; GRANT r TO 'u'@'%'",
            )
            .unwrap();
        checked.set_current_user(sealdb_parser::ast::UserIdentity::new("u", "%"));
        let denied = checked.execute_stmt("SELECT * FROM db1.t").unwrap_err();
        assert_eq!(denied.code, sealdb_errno::ERR_TABLEACCESS_DENIED_ERROR);
        checked.execute_stmt("SET ROLE r").unwrap();
        let ok = checked.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(ok.display_rows().len(), 1);
        let still_denied = checked
            .execute_stmt("INSERT INTO db1.t VALUES (2, 'b')")
            .unwrap_err();
        assert_eq!(
            still_denied.code,
            sealdb_errno::ERR_TABLEACCESS_DENIED_ERROR
        );
        checked.execute_stmt("SET ROLE NONE").unwrap();
        let denied_again = checked.execute_stmt("SELECT * FROM db1.t").unwrap_err();
        assert_eq!(
            denied_again.code,
            sealdb_errno::ERR_TABLEACCESS_DENIED_ERROR
        );
    }

    #[test]
    fn test_mysql_user_persist() {
        // Grant tables survive Storage reload (mysql.user equivalent).
        let storage: std::sync::Arc<dyn sealdb_kv::Storage> =
            std::sync::Arc::new(sealdb_kv::MemStorage::new());
        let first = Session::with_domain(std::sync::Arc::new(
            sealdb_domain::Domain::with_storage_and_grant_table(std::sync::Arc::clone(&storage))
                .unwrap(),
        ));
        first
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); GRANT SELECT ON db1.t TO 'u'@'%'",
            )
            .unwrap();
        let before = first.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert!(!before.display_rows().is_empty());

        let second = Session::with_domain(std::sync::Arc::new(
            sealdb_domain::Domain::with_storage_and_grant_table(storage).unwrap(),
        ));
        let after = second.execute_stmt("SHOW GRANTS FOR 'u'@'%'").unwrap();
        assert_eq!(before.display_rows(), after.display_rows());
    }

    #[test]
    fn test_analyze_stats() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'a')",
            )
            .unwrap();
        let parser = sealdb_parser::Parser::new();
        let select = parser
            .parse_one_stmt("SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        let before = PlanBuilder::new()
            .build_with_schema(
                &select,
                Some(&session.infoschema()),
                "",
                Some(session.stats().as_ref()),
                PartitionPruneMode::Dynamic,
            )
            .unwrap();
        assert!(
            before.explain().contains("IndexReader"),
            "{}",
            before.explain()
        );
        assert!(
            !before.explain().contains("IndexLookUp"),
            "{}",
            before.explain()
        );
        session.execute_stmt("ANALYZE TABLE db1.t").unwrap();
        let stats = session.stats().table("db1", "t").unwrap();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.column("name").unwrap().ndv, 1);
        let after = PlanBuilder::new()
            .build_with_schema(
                &select,
                Some(&session.infoschema()),
                "",
                Some(session.stats().as_ref()),
                PartitionPruneMode::Dynamic,
            )
            .unwrap();
        assert!(
            after.explain().contains("TableReader"),
            "{}",
            after.explain()
        );
        let rows = session
            .execute_stmt("SELECT * FROM db1.t WHERE name = 'a' ORDER BY id")
            .unwrap();
        assert_eq!(rows.display_rows().len(), 3);
    }

    #[test]
    fn test_analyze_index() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b')",
            )
            .unwrap();
        session
            .execute_stmt("ANALYZE TABLE db1.t INDEX idx_name")
            .unwrap();
        let stats = session.stats().table("db1", "t").unwrap();
        assert_eq!(stats.count, 3);
        assert!(stats.column("name").is_some());
        assert!(stats.column("id").is_none());
        assert_eq!(stats.column("name").unwrap().ndv, 2);
        session.execute_stmt("ANALYZE TABLE db1.t").unwrap();
        let full = session.stats().table("db1", "t").unwrap();
        assert!(full.column("id").is_some());
        assert!(full.column("name").is_some());
    }

    #[test]
    fn test_auto_analyze() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'); ANALYZE TABLE db1.t",
            )
            .unwrap();
        assert_eq!(session.stats().table("db1", "t").unwrap().count, 2);
        assert_eq!(session.stats().table("db1", "t").unwrap().modify_count, 0);
        // modify/count = 2/2 > 0.5 → synchronous auto-analyze refreshes count.
        session
            .execute_stmt("INSERT INTO db1.t VALUES (3, 'b'), (4, 'b')")
            .unwrap();
        let stats = session.stats().table("db1", "t").unwrap();
        assert_eq!(stats.count, 4);
        assert_eq!(stats.modify_count, 0);
        session
            .execute_stmt("SET tidb_enable_auto_analyze = OFF")
            .unwrap();
        session
            .execute_stmt("INSERT INTO db1.t VALUES (5, 'c'), (6, 'c'), (7, 'c')")
            .unwrap();
        let disabled = session.stats().table("db1", "t").unwrap();
        assert_eq!(disabled.count, 4);
        assert_eq!(disabled.modify_count, 3);
    }

    #[test]
    fn test_stats_persist() {
        let storage: std::sync::Arc<dyn sealdb_kv::Storage> =
            std::sync::Arc::new(sealdb_kv::MemStorage::new());
        let first = Session::with_domain(std::sync::Arc::new(
            Domain::with_storage(std::sync::Arc::clone(&storage)).unwrap(),
        ));
        first
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'); ANALYZE TABLE db1.t",
            )
            .unwrap();
        let before = first.stats().table("db1", "t").unwrap();
        assert_eq!(before.count, 2);
        assert_eq!(before.column("name").unwrap().ndv, 1);
        drop(first);

        let second =
            Session::with_domain(std::sync::Arc::new(Domain::with_storage(storage).unwrap()));
        let after = second.stats().table("db1", "t").unwrap();
        assert_eq!(after.count, before.count);
        assert_eq!(
            after.column("name").unwrap().ndv,
            before.column("name").unwrap().ndv
        );
        assert!(after.column("name").unwrap().histogram.is_some());
        assert!(after.column("name").unwrap().topn.is_some());
    }

    #[test]
    fn test_explain_skyline_and_join_reorder() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'b'), (5, 'a'), (6, 'b'), (7, 'a'), (8, 'b'), (9, 'a'), (10, 'b')",
            )
            .unwrap();
        let before = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        assert_eq!(
            before.columns,
            vec!["id", "estRows", "task", "access object", "operator info"]
        );
        let before_text = before
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(before_text.contains("IndexReader"), "{before_text}");
        assert!(!before_text.contains("IndexLookUp"), "{before_text}");
        session.execute_stmt("ANALYZE TABLE db1.t").unwrap();
        let after = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        let after_text = after
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(after_text.contains("TableReader"), "{after_text}");
        assert!(!after_text.contains("IndexLookUp"), "{after_text}");
        assert!(!after_text.contains("IndexReader"), "{after_text}");

        let dual = session.execute_stmt("EXPLAIN SELECT 1").unwrap();
        let dual_text = dual
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dual_text.contains("TableDual"), "{dual_text}");
        let json = session
            .execute_stmt("EXPLAIN FORMAT='json' SELECT 1")
            .unwrap();
        assert_eq!(json.columns, vec!["json"]);
        assert!(json.display_rows()[0][0].contains("TableDual"));
        let dot = session
            .execute_stmt("EXPLAIN FORMAT='dot' SELECT 1")
            .unwrap();
        assert!(dot.display_rows()[0][0].contains("digraph"));

        session
            .execute_stmt(
                "CREATE TABLE db1.small (id INT); CREATE TABLE db1.big (id INT); INSERT INTO db1.small VALUES (1); INSERT INTO db1.big VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10); ANALYZE TABLE db1.small; ANALYZE TABLE db1.big",
            )
            .unwrap();
        let join = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.small JOIN db1.big ON small.id = big.id")
            .unwrap();
        let join_text = join
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        let big_pos = join_text.find("table:big").expect(&join_text);
        let small_pos = join_text.find("table:small").expect(&join_text);
        assert!(big_pos < small_pos, "{join_text}");
        let rows = session
            .execute_stmt(
                "SELECT small.id, big.id FROM db1.small JOIN db1.big ON small.id = big.id",
            )
            .unwrap();
        assert_eq!(rows.display_rows().len(), 1);

        let analyze = session.execute_stmt("EXPLAIN ANALYZE SELECT 1").unwrap();
        assert_eq!(
            analyze.columns,
            vec![
                "id",
                "estRows",
                "actRows",
                "task",
                "access object",
                "execution info",
                "operator info"
            ]
        );
        let analyze_text = analyze
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(analyze_text.contains("TableDual"), "{analyze_text}");
        assert!(analyze_text.contains("time:"), "{analyze_text}");
        assert!(
            analyze
                .display_rows()
                .iter()
                .any(|r| r.get(2).map(|c| c.as_str()) == Some("1")),
            "{analyze_text}"
        );
    }

    #[test]
    fn test_explain_cost_model_v2() {
        // Cost Model v2 MVB: JoinReorder builds the cheaper side; EXPLAIN shows
        // estCost from plan_cost_ver2 helpers on HashJoin.
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.small (id INT); CREATE TABLE db1.big (id INT); \
                 INSERT INTO db1.small VALUES (1); \
                 INSERT INTO db1.big VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10); \
                 ANALYZE TABLE db1.small; ANALYZE TABLE db1.big",
            )
            .unwrap();
        let join = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.small JOIN db1.big ON small.id = big.id")
            .unwrap();
        let join_text = join
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(join_text.contains("estCost="), "{join_text}");
        let big_pos = join_text.find("table:big").expect(&join_text);
        let small_pos = join_text.find("table:small").expect(&join_text);
        assert!(
            big_pos < small_pos,
            "v2 cost should probe big / build small:\n{join_text}"
        );
        let est: f64 = join
            .display_rows()
            .iter()
            .find_map(|r| {
                r.last().and_then(|info| {
                    info.split("estCost=")
                        .nth(1)
                        .and_then(|s| s.split(',').next())
                        .and_then(|s| s.trim().parse().ok())
                })
            })
            .expect(&join_text);
        assert!(est > 0.0, "{join_text}");
        let expected = sealdb_planner::core::plan_cost_ver2::hash_join_est_cost(10_000.0, 10_000.0);
        assert!(
            (est - expected).abs() < 1e-6,
            "est={est} expected={expected} text={join_text}"
        );
    }

    #[test]
    fn test_explain_index_merge_or() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; \
                 CREATE TABLE db1.t (id INT, a INT, b INT, INDEX idx_a (a), INDEX idx_b (b)); \
                 INSERT INTO db1.t VALUES (1, 1, 10), (2, 2, 20), (3, 1, 20), (4, 9, 9)",
            )
            .unwrap();
        let explain = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE a = 1 OR b = 20")
            .unwrap();
        let text = explain
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("IndexMerge"), "{text}");
        assert!(text.contains("IndexRangeScan"), "{text}");
        assert!(text.contains("TableRowIDScan"), "{text}");
        assert!(
            explain
                .display_rows()
                .iter()
                .any(|r| r.get(2).map(|t| t.as_str()) == Some("cop")),
            "{text}"
        );
        let rows = session
            .execute_stmt("SELECT * FROM db1.t WHERE a = 1 OR b = 20 ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "1".to_string(), "10".to_string()],
                vec!["2".to_string(), "2".to_string(), "20".to_string()],
                vec!["3".to_string(), "1".to_string(), "20".to_string()],
            ]
        );
    }

    #[test]
    fn test_explain_n_table_join_reorder_dp() {
        let session = Session::new();
        // Skewed sizes: naive SQL order is big ⋈ mid ⋈ small (left-deep).
        // DP joins mid+small first, then lets big probe that tree.
        session
            .execute_stmt(
                "CREATE DATABASE db1; \
                 CREATE TABLE db1.big (id INT); \
                 CREATE TABLE db1.mid (id INT); \
                 CREATE TABLE db1.small (id INT); \
                 INSERT INTO db1.big VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),\
                   (11),(12),(13),(14),(15),(16),(17),(18),(19),(20),\
                   (21),(22),(23),(24),(25),(26),(27),(28),(29),(30),\
                   (31),(32),(33),(34),(35),(36),(37),(38),(39),(40),\
                   (41),(42),(43),(44),(45),(46),(47),(48),(49),(50),\
                   (51),(52),(53),(54),(55),(56),(57),(58),(59),(60),\
                   (61),(62),(63),(64),(65),(66),(67),(68),(69),(70),\
                   (71),(72),(73),(74),(75),(76),(77),(78),(79),(80),\
                   (81),(82),(83),(84),(85),(86),(87),(88),(89),(90),\
                   (91),(92),(93),(94),(95),(96),(97),(98),(99),(100); \
                 INSERT INTO db1.mid VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10); \
                 INSERT INTO db1.small VALUES (1); \
                 ANALYZE TABLE db1.big; ANALYZE TABLE db1.mid; ANALYZE TABLE db1.small",
            )
            .unwrap();
        let join = session
            .execute_stmt(
                "EXPLAIN SELECT * FROM db1.big JOIN db1.mid ON big.id = mid.id \
                 JOIN db1.small ON mid.id = small.id",
            )
            .unwrap();
        let rows = join.display_rows();
        let text = rows
            .iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        let root = rows
            .iter()
            .position(|r| {
                r.first()
                    .map(|id| id.contains("HashJoin") && !id.contains("└─") && !id.contains("├─"))
                    .unwrap_or(false)
            })
            .expect(&text);
        // Naive left-deep starts with a nested HashJoin on the left; DP puts
        // TableReader(big) as the probe (left) child of the root HashJoin.
        let left_child = rows.get(root + 1).expect(&text);
        assert!(
            left_child
                .first()
                .map(|id| id.contains("TableReader"))
                .unwrap_or(false),
            "expected TableReader probe under root HashJoin, got:\n{text}"
        );
        assert!(
            text.matches("HashJoin").count() >= 2,
            "expected nested HashJoin build side:\n{text}"
        );
        // Probe subtree lists table:big before mid/small (cop TableFullScan).
        let big_pos = text.find("table:big").expect(&text);
        let mid_pos = text.find("table:mid").expect(&text);
        let small_pos = text.find("table:small").expect(&text);
        assert!(big_pos < mid_pos && mid_pos < small_pos, "{text}");

        let result = session
            .execute_stmt(
                "SELECT big.id, mid.id, small.id FROM db1.big \
                 JOIN db1.mid ON big.id = mid.id \
                 JOIN db1.small ON mid.id = small.id",
            )
            .unwrap();
        assert_eq!(result.display_rows().len(), 1, "{text}");
    }

    #[test]
    fn test_explain_analyze_table_scan_act_rows() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            )
            .unwrap();
        let result = session
            .execute_stmt("EXPLAIN ANALYZE SELECT * FROM db1.t WHERE id = 1")
            .unwrap();
        assert!(result.columns.iter().any(|c| c == "actRows"));
        let text = result
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Point_Get") || text.contains("TableReader") || text.contains("Selection"),
            "{text}"
        );
        assert!(
            result
                .display_rows()
                .iter()
                .any(|r| r.get(2).map(|c| c.as_str()) == Some("1")),
            "{text}"
        );
        let json = session
            .execute_stmt("EXPLAIN ANALYZE FORMAT='json' SELECT 1")
            .unwrap();
        assert!(json.display_rows()[0][0].contains("actRows"));
    }

    #[test]
    fn test_explain_cop_task_rows() {
        let session = Session::new();
        session
            .execute_stmt(
                "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b')",
            )
            .unwrap();
        let scan = session.execute_stmt("EXPLAIN SELECT * FROM db1.t").unwrap();
        let scan_text = scan
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(scan_text.contains("TableReader"), "{scan_text}");
        assert!(scan_text.contains("TableFullScan"), "{scan_text}");
        let scan_rows = scan.display_rows();
        let cop_scan = scan_rows
            .iter()
            .find(|r| {
                r.first()
                    .map(|id| id.contains("TableFullScan"))
                    .unwrap_or(false)
            })
            .expect(&scan_text);
        assert_eq!(
            cop_scan.get(2).map(String::as_str),
            Some("cop"),
            "{scan_text}"
        );
        assert!(
            !cop_scan
                .get(1)
                .map(|e| e.is_empty() || e == "0.00")
                .unwrap_or(true),
            "cop estRows empty/zero: {scan_text}"
        );
        assert!(
            cop_scan
                .get(3)
                .map(|a| a.contains("table:"))
                .unwrap_or(false),
            "{scan_text}"
        );

        let filtered = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.t WHERE name = 'a'")
            .unwrap();
        let filtered_text = filtered
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(filtered_text.contains("TableReader"), "{filtered_text}");
        assert!(filtered_text.contains("TableFullScan"), "{filtered_text}");
        assert!(
            filtered
                .display_rows()
                .iter()
                .any(|r| r.get(2).map(|t| t.as_str()) == Some("cop")),
            "{filtered_text}"
        );

        session
            .execute_stmt(
                "CREATE TABLE db1.u (id INT, name VARCHAR(20), extra VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.u VALUES (1, 'a', 'x')",
            )
            .unwrap();
        let lookup = session
            .execute_stmt("EXPLAIN SELECT * FROM db1.u WHERE name = 'a'")
            .unwrap();
        let lookup_text = lookup
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            lookup_text.contains("IndexLookUp") || lookup_text.contains("IndexReader"),
            "{lookup_text}"
        );
        assert!(
            lookup_text.contains("IndexRangeScan") || lookup_text.contains("IndexFullScan"),
            "{lookup_text}"
        );
        assert!(
            lookup
                .display_rows()
                .iter()
                .any(|r| r.get(2).map(|t| t.as_str()) == Some("cop")
                    && r.get(3).map(|a| !a.is_empty()).unwrap_or(false)),
            "{lookup_text}"
        );

        let analyze = session
            .execute_stmt("EXPLAIN ANALYZE SELECT * FROM db1.t")
            .unwrap();
        let analyze_text = analyze
            .display_rows()
            .into_iter()
            .map(|r| r.join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(analyze_text.contains("TableFullScan"), "{analyze_text}");
        assert!(
            analyze.display_rows().iter().any(|r| r
                .first()
                .map(|id| id.contains("TableFullScan"))
                .unwrap_or(false)
                && r.get(2).map(|a| a.as_str()) == Some("2")
                && r.get(3).map(|t| t.as_str()) == Some("cop")),
            "{analyze_text}"
        );
    }

    #[test]
    fn test_brie_local_backup_restore() {
        let dir = std::env::temp_dir().join(format!(
            "sealdb-brie-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("backup.json");
        let path_str = path.to_string_lossy().into_owned();

        let session = Session::new();
        session.execute_stmt("CREATE DATABASE brie_db").unwrap();
        session
            .execute_stmt("CREATE TABLE brie_db.t (id INT, name VARCHAR(20))")
            .unwrap();
        session
            .execute_stmt("INSERT INTO brie_db.t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        let backup = session
            .execute_stmt(&format!("BACKUP DATABASE brie_db TO '{path_str}'"))
            .unwrap();
        assert_eq!(backup.display_rows()[0][0], "Backup");
        assert_eq!(backup.display_rows()[0][2], "OK");

        session.execute_stmt("DROP TABLE brie_db.t").unwrap();
        let gone = session.execute_stmt("SELECT * FROM brie_db.t").unwrap_err();
        assert_eq!(gone.code, sealdb_errno::ERR_NO_SUCH_TABLE);

        let restore = session
            .execute_stmt(&format!("RESTORE DATABASE FROM '{path_str}'"))
            .unwrap();
        assert_eq!(restore.display_rows()[0][0], "Restore");
        let rows = session
            .execute_stmt("SELECT * FROM brie_db.t ORDER BY id")
            .unwrap();
        assert_eq!(
            rows.display_rows(),
            vec![
                vec!["1".to_string(), "a".to_string()],
                vec!["2".to_string(), "b".to_string()]
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

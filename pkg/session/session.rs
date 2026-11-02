use sealdb_config::Config;
use sealdb_domain::{Domain, SchemaSnap};
use sealdb_errno::Result;
use sealdb_executor::{DDLExec, ExecutorBuilder, QueryResult};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::IsolationLevel;
use sealdb_parser::Parser;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{
    InsertStmt, IsolationLevel as AstIsolation, PrivilegeType, Statement, UserIdentity,
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
    /// Session user variables (`SET @a = …` / `SELECT @a`).
    user_vars: Mutex<HashMap<String, sealdb_types::Datum>>,
    /// TiDB `tidb_partition_prune_mode` (default dynamic).
    partition_prune_mode: Mutex<PartitionPruneMode>,
    /// Active roles for the session (`SET ROLE`).
    active_roles: Mutex<Vec<UserIdentity>>,
    /// TiDB `tidb_enable_auto_analyze` (default ON).
    enable_auto_analyze: Mutex<bool>,
    /// TiDB `tidb_auto_analyze_ratio` (default 0.5).
    auto_analyze_ratio: Mutex<f64>,
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
        Self {
            parser: Parser::new(),
            planner: PlanBuilder::new(),
            domain,
            current_db: Mutex::new(String::new()),
            current_user: Mutex::new(UserIdentity::root()),
            txn: Mutex::new(TxnManager::new()),
            schema_snap: Mutex::new(None),
            isolation: Mutex::new(IsolationLevel::RepeatableRead),
            read_only: Mutex::new(false),
            prepared: Mutex::new(HashMap::new()),
            cache_plans: Mutex::new(false),
            plan_cache_key: Mutex::new(None),
            collation: Mutex::new(Collation::bin()),
            sql_mode: Mutex::new("ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES".to_string()),
            user_vars: Mutex::new(HashMap::new()),
            partition_prune_mode: Mutex::new(PartitionPruneMode::Dynamic),
            active_roles: Mutex::new(Vec::new()),
            enable_auto_analyze: Mutex::new(true),
            auto_analyze_ratio: Mutex::new(0.5),
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

    /// Privilege manager for MySQL handshake auth (F22).
    pub fn privilege_manager(&self) -> Arc<sealdb_privilege::Manager> {
        self.domain.privilege()
    }

    pub fn execute_stmt(&self, sql: &str) -> Result<QueryResult> {
        let mut results = self.execute_all(sql)?;
        Ok(results.pop().unwrap_or(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
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

    /// Execute every statement in `sql` and return each result (F22 multi-resultset).
    pub fn execute_all(&self, sql: &str) -> Result<Vec<QueryResult>> {
        let stmts = self.parser.parse_sql(sql)?;
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
                apply_set(self, set)?;
                return Ok(empty_result());
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
                *self.cache_plans.lock().expect("session cache flag lock") = true;
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
            | Statement::CreateTable(_)
            | Statement::DropTable(_)
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
        let current_db = self
            .current_db
            .lock()
            .expect("session current_db lock")
            .clone();
        let is = self.domain.infoschema();
        let ctas_insert = match stmt {
            Statement::CreateTable(create) => create.select.as_ref().and_then(|select| {
                let (schema, table) = create.table.qualified(&current_db);
                if create.if_not_exists && is.table_by_name(&schema, &table).is_some() {
                    return None;
                }
                Some(InsertStmt {
                    table: create.table.clone(),
                    columns: Vec::new(),
                    rows: Vec::new(),
                    select: Some(select.clone()),
                    with: None,
                })
            }),
            _ => None,
        };
        // Apply CREATE BINDING rewrite before plan build (MVB: substitute bind SQL).
        let rebound = self.apply_sql_binding(stmt)?;
        let stmt = rebound.as_ref().unwrap_or(stmt);
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
        let plan = if use_cache {
            let cache = self.domain.plan_cache();
            if let Some(plan) = cache.get(&cache_key, schema_ver) {
                plan
            } else {
                let prune_mode = *self
                    .partition_prune_mode
                    .lock()
                    .expect("session partition prune mode lock");
                // Shortcut: plan is built from the first bound SQL, then reused
                // under the template key. PointGet/index lookup re-read literals
                // from the current bound stmt in ExecutorBuilder.
                let plan = self.planner.build_with_schema(
                    stmt,
                    Some(&is),
                    &current_db,
                    Some(stats.as_ref()),
                    prune_mode,
                )?;
                cache.put(cache_key, schema_ver, plan.clone());
                plan
            }
        } else {
            let prune_mode = *self
                .partition_prune_mode
                .lock()
                .expect("session partition prune mode lock");
            self.planner.build_with_schema(
                stmt,
                Some(&is),
                &current_db,
                Some(stats.as_ref()),
                prune_mode,
            )?
        };
        if let Some(result) = system_var_result(self, stmt) {
            return Ok(result);
        }
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
        let mode = sealdb_expression::CollateMode::from_name(&coll.name);
        let result = sealdb_expression::with_collation(mode, || ExecutorBuilder::run(exec));
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
        if result.is_ok()
            && let Some(insert) = ctas_insert
        {
            return self.execute_one(&Statement::Insert(insert));
        }
        if result.is_ok() {
            maybe_auto_analyze(self, stmt)?;
        }
        result
    }

    fn capture_schema_snap(&self) {
        let mut snap = self.schema_snap.lock().expect("session schema snap lock");
        if snap.is_none() {
            *snap = Some(self.domain.schema_snap());
        }
    }

    /// Persist in-memory DDL on COMMIT, or restore the pre-DDL snapshot on
    /// ROLLBACK. CREATE INDEX / ALTER backfill KV is not undone (F18).
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
            | Statement::RevokeRole(_) => {
                if !mgr.request_verification_with_roles(&user, &roles, "", "", PrivilegeType::ALL) {
                    return Err(access_denied("", "", PrivilegeType::ALL, &user));
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
                require(
                    &mgr,
                    &user,
                    &roles,
                    &analyze.table,
                    &current_db,
                    PrivilegeType::SELECT,
                )?;
            }
            _ => {}
        }
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
    }
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
        if enable && stats.needs_auto_analyze(&schema, &name, ratio) {
            due.push((schema, name));
        }
    }
    for (schema, name) in due {
        let analyze = Statement::AnalyzeTable(sealdb_parser::ast::AnalyzeTableStmt {
            table: sealdb_parser::ast::TableName::new(Some(schema), name),
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

fn parse_set_literal(value: &str) -> sealdb_types::Datum {
    let v = value.trim();
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

fn apply_set(session: &Session, stmt: &sealdb_parser::ast::SetStmt) -> Result<()> {
    for a in &stmt.assignments {
        if is_user_var_name(&a.name) {
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
        match name.as_str() {
            "names"
            | "character_set_client"
            | "character_set_connection"
            | "character_set_results" => {
                let mut collation = session.collation.lock().expect("session collation lock");
                collation.charset = value.to_ascii_lowercase();
                if collation.name == "utf8mb4_bin" && collation.charset != "utf8mb4" {
                    collation.name = format!("{}_bin", collation.charset);
                }
            }
            "collation_connection" | "collation_database" => {
                *session.collation.lock().expect("session collation lock") =
                    Collation::parse(&value);
            }
            "sql_mode" => {
                *session.sql_mode.lock().expect("session sql_mode lock") = value;
            }
            "tidb_partition_prune_mode" => {
                let mode = PartitionPruneMode::parse(&value).ok_or_else(|| {
                    sealdb_errno::Error::new(
                        sealdb_errno::ERR_WRONG_ARGUMENTS,
                        format!("Variable 'tidb_partition_prune_mode' can't be set to the value of '{value}'"),
                    )
                })?;
                *session
                    .partition_prune_mode
                    .lock()
                    .expect("session partition prune mode lock") = mode;
                // Prune mode changes plan shape; drop Domain-shared cached plans.
                session.domain.plan_cache().clear();
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
            "autocommit" | "unique_checks" | "foreign_key_checks" => {}
            other => {
                return Err(sealdb_errno::Error::unsupported(format!("SET {other}")));
            }
        }
    }
    Ok(())
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
        let ExprNode::Column(col) = &field.expr else {
            return None;
        };
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
        let name = col.name.trim_start_matches('@').to_ascii_lowercase();
        let value = match name.as_str() {
            "sql_mode" => session
                .sql_mode
                .lock()
                .expect("session sql_mode lock")
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
            _ => return None,
        };
        row.push(sealdb_types::Datum::Bytes(value.into_bytes()));
    }
    if columns.is_empty() {
        return None;
    }
    Some(QueryResult {
        columns,
        rows: vec![row],
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

fn map_isolation(level: AstIsolation) -> IsolationLevel {
    match level {
        AstIsolation::ReadCommitted => IsolationLevel::ReadCommitted,
        AstIsolation::RepeatableRead => IsolationLevel::RepeatableRead,
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
            | Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::AlterTable(_)
            | Statement::Grant(_)
            | Statement::Revoke(_)
            | Statement::CreateRole(_)
            | Statement::DropRole(_)
            | Statement::CreateUser(_)
            | Statement::DropUser(_)
            | Statement::GrantRole(_)
            | Statement::RevokeRole(_)
    )
}

impl Session {
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
    use sealdb_errno::{ERR_DB_CREATE_EXISTS, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS};
    use sealdb_types::Datum;

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
        assert!(unique_text.contains("PointGet"), "{unique_text}");
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
            unique_cover_text.contains("PointGet"),
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
        let err = session
            .execute_stmt("START TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .unwrap_err();
        assert!(err.message.contains("SERIALIZABLE"));
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
        a.execute_stmt("BEGIN").unwrap();
        let snap = a.execute_stmt("SELECT * FROM db1.t").unwrap();
        assert_eq!(snap.display_rows().len(), 1);
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
        let mat_plan = session
            .execute_stmt("EXPLAIN WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM c")
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
        let err = session
            .execute_stmt("WITH c(a, b) AS (SELECT 1) SELECT * FROM c")
            .unwrap_err();
        assert_eq!(
            err.code,
            sealdb_errno::ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT
        );
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
        let err = session
            .execute_stmt("SELECT * FROM db1.t JOIN (SELECT 1 AS id) d ON t.id = d.id")
            .unwrap_err();
        assert!(err.message.contains("subquery JOIN"));
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
            text.contains("PointGet") || text.contains("TableReader") || text.contains("Selection"),
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
}

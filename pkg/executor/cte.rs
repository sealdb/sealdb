// core path: CTEExec for WITH, including WITH RECURSIVE UNION/UNION ALL
// (TiDB pkg/executor/cte.go). Completeness wave / not this feature:
// Nested WITH / qualified CTE names.
use crate::builder::ExecutorBuilder;
use crate::internal::exec::{Executor, QueryResult};
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_BAD_FIELD, ERR_DUP_FIELDNAME, ERR_UNSUPPORTED, ERR_VIEW_WRONG_LIST,
    ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result,
};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::expressions::{
    ColumnNameExpr, ExprNode, InListExpr, ValueExpr,
};
use sealdb_parser::ast::{
    ResultSetNode, SelectField, SelectLock, SelectLockWait, SelectStmt, SetOprType, Statement,
    WithClause,
};
use sealdb_planner::PlanBuilder;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type CteWorking = (Vec<String>, Vec<Vec<Datum>>);

thread_local! {
    static WORKING: RefCell<HashMap<String, CteWorking>> = RefCell::new(HashMap::new());
    static SHARED: RefCell<(u32, HashMap<String, CteWorking>)> =
        RefCell::new((0, HashMap::new()));
}

/// Scope for statement-local CTETableReader reuse; nested runs keep the map.
pub fn with_shared_ctes<T>(f: impl FnOnce() -> T) -> T {
    SHARED.with(|cell| {
        cell.borrow_mut().0 += 1;
    });
    let out = f();
    SHARED.with(|cell| {
        let mut guard = cell.borrow_mut();
        guard.0 = guard.0.saturating_sub(1);
        if guard.0 == 0 {
            guard.1.clear();
        }
    });
    out
}

/// Isolate CTE share for correlated Apply subqueries so a recursive CTE bound
/// to outer row N cannot be reused for outer row N+1 (same CTE name, different
/// seed filter).
pub fn with_isolated_shared_ctes<T>(f: impl FnOnce() -> T) -> T {
    let saved = SHARED.with(|cell| {
        let mut guard = cell.borrow_mut();
        (guard.0, std::mem::take(&mut guard.1))
    });
    SHARED.with(|cell| {
        let mut guard = cell.borrow_mut();
        guard.0 = 0;
        guard.1.clear();
    });
    let out = with_shared_ctes(f);
    SHARED.with(|cell| {
        let mut guard = cell.borrow_mut();
        guard.0 = saved.0;
        guard.1 = saved.1;
    });
    out
}

fn shared_cte(name: &str) -> Option<CteWorking> {
    SHARED.with(|cell| {
        cell.borrow()
            .1
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    })
}

fn store_shared_cte(name: &str, schema: Vec<String>, rows: Vec<Vec<Datum>>) {
    SHARED.with(|cell| {
        cell.borrow_mut().1.insert(name.to_string(), (schema, rows));
    });
}

fn check_duplicate_field_names(names: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for name in names {
        // Join schemas may still carry `table.col`; TiDB derived-table
        // uniqueness is on the bare column name.
        let bare = name.rsplit('.').next().unwrap_or(name.as_str());
        let key = bare.to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        if !seen.insert(key) {
            return Err(Error::new(
                ERR_DUP_FIELDNAME,
                format!("Duplicate column name '{bare}'"),
            ));
        }
    }
    Ok(())
}

/// Evaluate `SELECT * FROM cte` against `with` (INSERT/UPDATE/DELETE JOIN sides).
pub fn eval_named_cte(
    name: &str,
    with: &WithClause,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<QueryResult> {
    let select = SelectStmt {
        fields: vec![SelectField {
            orig_text: None,
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
        }],
        from: Some(TableName::new(None, name)),
        joins: Vec::new(),
        from_item: None,
        selection: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: SelectLock::None,
        lock_wait: SelectLockWait::Block,
        with: Some(with.clone()),
        text: String::new(),
    };
    run_select(&select, domain, current_db, txn)
}

/// Working rows for a recursive CTE, visible to TableReader as `FROM cte`.
pub fn working_cte(name: &str) -> Option<CteWorking> {
    WORKING.with(|cell| {
        cell.borrow()
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    })
}

/// Recursive working set, else statement-shared CTE (sibling CTEs in mutual recursion).
pub fn lookup_cte_rows(name: &str) -> Option<CteWorking> {
    working_cte(name).or_else(|| shared_cte(name))
}

fn with_working_cte<T>(
    name: &str,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    f: impl FnOnce() -> T,
) -> T {
    WORKING.with(|cell| {
        cell.borrow_mut().insert(name.to_string(), (schema, rows));
    });
    let out = f();
    WORKING.with(|cell| {
        cell.borrow_mut().remove(name);
    });
    out
}

const MAX_RECURSION: usize = 1000;

/// Materializes a `WITH` query; recursive CTEs iterate `recursive` against
/// a TableReader working set published under `name`. Shared storage reuses
/// one materialization per statement (TiDB `CTETableReader`).
pub struct CTEExec {
    columns: Vec<String>,
    name: String,
    recursive: Option<(SetOprType, SelectStmt)>,
    seed: Box<dyn Executor>,
    domain: Arc<Domain>,
    current_db: String,
    txn: Arc<TxnHandle>,
    share: bool,
    /// Seed nested WITH reuses `name`; recursive working names stay seed output.
    shadow_same_name: bool,
    /// Outer LIMIT/OFFSET on a recursive CTE body.
    limit: Option<u64>,
    offset: Option<u64>,
    from_cache: bool,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl CTEExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        columns: Vec<String>,
        name: String,
        recursive: Option<(SetOprType, SelectStmt)>,
        seed: Box<dyn Executor>,
        domain: Arc<Domain>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        share: bool,
        shadow_same_name: bool,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Self {
        Self {
            columns,
            name,
            recursive,
            seed,
            domain,
            current_db: current_db.into(),
            txn,
            share,
            shadow_same_name,
            limit,
            offset,
            from_cache: false,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for CTEExec {
    fn open(&mut self) -> Result<()> {
        if self.share
            && let Some((schema, rows)) = shared_cte(&self.name)
        {
            self.schema = schema;
            self.rows = rows;
            self.from_cache = true;
            return Ok(());
        }
        self.seed.open()?;
        let seed_schema = self.seed.schema().to_vec();
        self.schema = seed_schema.clone();
        self.rows = collect_rows(&mut self.seed)?;
        if !self.columns.is_empty() {
            let width = self
                .rows
                .first()
                .map(|row| row.len())
                .unwrap_or(self.schema.len());
            if width != 0 && self.columns.len() != width {
                return Err(Error::new(
                    ERR_VIEW_WRONG_LIST,
                    "In definition of view, derived table or common table expression, SELECT list and column names list have different column counts",
                ));
            }
            // Remap output names; recursive working set may keep seed names when
            // a same-name nested WITH shadowed the CTE (TiDB errno 1054).
            if !self.shadow_same_name {
                self.schema = self.columns.clone();
            }
        }
        // TiDB/MySQL: derived-table / CTE output names must be unique (errno 1060),
        // including after `SELECT *` expands join schemas with overlapping names.
        check_duplicate_field_names(&self.schema)?;
        if let Some((op, rec)) = self.recursive.clone() {
            // TiDB: same-name nested WITH keeps recursive names as seed output;
            // outer col_name_list refs in the recursive arm → errno 1054.
            if self.shadow_same_name {
                if let Some(bad) =
                    unknown_outer_col_in_recursive(&rec, &self.schema, &self.columns)
                {
                    let _ = self.seed.close();
                    return Err(Error::new(
                        ERR_BAD_FIELD,
                        format!("Unknown column '{bad}' in 'where clause'"),
                    ));
                }
            }
            self.rows = iterate_recursive(
                &self.name,
                self.schema.clone(),
                std::mem::take(&mut self.rows),
                op,
                rec,
                &self.domain,
                &self.current_db,
                &self.txn,
                self.limit,
                self.offset,
            )?;
        } else if self.limit.is_some() || self.offset.is_some() {
            self.rows = apply_cte_limit(std::mem::take(&mut self.rows), self.limit, self.offset);
        }
        if !self.columns.is_empty() {
            self.schema = self.columns.clone();
        }
        // Always publish for sibling refs (mutual recursion). `share` only
        // gates reuse on a later open of the same CTE name.
        store_shared_cte(&self.name, self.schema.clone(), self.rows.clone());
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
        if self.from_cache {
            Ok(())
        } else {
            self.seed.close()
        }
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

/// Expand the recursive UNION / UNION ALL arm against the working set until
/// a step produces no new rows or `MAX_RECURSION` is hit.
#[allow(clippy::too_many_arguments)]
fn iterate_recursive(
    name: &str,
    schema: Vec<String>,
    seed: Vec<Vec<Datum>>,
    op: SetOprType,
    rec: SelectStmt,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<Vec<Vec<Datum>>> {
    // TiDB rejects ORDER BY / LIMIT / DISTINCT in the recursive arm.
    if rec.limit.is_some() || !rec.order_by.is_empty() || rec.distinct {
        return Err(Error::new(
            ERR_UNSUPPORTED,
            "This version of TiDB doesn't yet support 'ORDER BY / LIMIT / SELECT DISTINCT in recursive query block of Common Table Expression'",
        ));
    }
    let distinct = op.is_distinct();
    // UNION (distinct) also collapses duplicate seed rows (TiDB / MySQL).
    let (mut all, mut seen) = if distinct {
        let mut uniq = Vec::new();
        let mut seen = HashSet::new();
        for row in seed {
            if seen.insert(row_key(&row)) {
                uniq.push(row);
            }
        }
        (uniq, seen)
    } else {
        let seen: HashSet<Vec<String>> = seed.iter().map(|row| row_key(row)).collect();
        (seed, seen)
    };
    // Outer CTE LIMIT applies to the whole result (seed + recursive).
    if let Some(lim) = limit {
        let need = lim.saturating_add(offset.unwrap_or(0)) as usize;
        if all.len() >= need {
            return Ok(apply_cte_limit(all, limit, offset));
        }
    }
    let mut working = all.clone();
    // TiDB/MySQL: recursive arm that never reads this CTE runs once (not forever).
    let self_ref = select_refs_table(&rec, name);
    for iter in 0..=MAX_RECURSION {
        if working.is_empty() {
            break;
        }
        if iter == MAX_RECURSION {
            return Err(Error::unsupported("recursive CTE exceeded max recursion"));
        }
        let next = with_working_cte(name, schema.clone(), working.clone(), || {
            run_select(&rec, domain, current_db, txn)
        })?;
        if let Some(row) = next.rows.first() {
            if row.len() != schema.len() {
                return Err(Error::new(
                    ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT,
                    "The used SELECT statements have a different number of columns",
                ));
            }
        }
        let mut step = Vec::new();
        for row in next.rows {
            if distinct && !seen.insert(row_key(&row)) {
                continue;
            }
            step.push(row);
        }
        if step.is_empty() {
            break;
        }
        all.extend(step.clone());
        if let Some(lim) = limit {
            let need = lim.saturating_add(offset.unwrap_or(0)) as usize;
            if all.len() >= need {
                return Ok(apply_cte_limit(all, limit, offset));
            }
        }
        if !self_ref {
            break;
        }
        working = step;
    }
    Ok(apply_cte_limit(all, limit, offset))
}

fn apply_cte_limit(
    mut rows: Vec<Vec<Datum>>,
    limit: Option<u64>,
    offset: Option<u64>,
) -> Vec<Vec<Datum>> {
    let off = offset.unwrap_or(0) as usize;
    if off > 0 {
        if off >= rows.len() {
            return Vec::new();
        }
        rows = rows.split_off(off);
    }
    if let Some(lim) = limit {
        let n = lim as usize;
        if rows.len() > n {
            rows.truncate(n);
        }
    }
    rows
}

fn select_refs_table(select: &SelectStmt, name: &str) -> bool {
    select
        .join_tables()
        .into_iter()
        .any(|t| t.resolve_name().eq_ignore_ascii_case(name))
}

fn run_select(
    select: &SelectStmt,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<QueryResult> {
    let stmt = Statement::Select(select.clone());
    run_statement(&stmt, domain, current_db, txn)
}

/// Run a subquery `ResultSetNode` (SELECT or set operation) to completion.
pub fn run_result_set(
    query: &ResultSetNode,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<QueryResult> {
    let stmt = match query {
        ResultSetNode::Select(select) => Statement::Select(select.clone()),
        ResultSetNode::SetOpr(set) => Statement::SetOpr(set.clone()),
    };
    run_statement(&stmt, domain, current_db, txn)
}

fn run_statement(
    stmt: &Statement,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<QueryResult> {
    let stats = domain.stats();
    let plan = PlanBuilder::new().build_with_schema(
        stmt,
        Some(&domain.infoschema()),
        current_db,
        Some(stats.as_ref()),
        sealdb_planner::PartitionPruneMode::Dynamic,
    )?;
    let exec = ExecutorBuilder::build(
        &plan,
        stmt,
        Arc::clone(domain),
        current_db,
        Arc::clone(txn),
    )?;
    ExecutorBuilder::run(exec)
}

/// Materialize uncorrelated subqueries in a DML predicate.
///
/// `UPDATE`/`DELETE` filters are evaluated row-by-row, which cannot execute a
/// subquery (TiDB builds an Apply). Before filtering we run each uncorrelated
/// `IN`/`EXISTS`/scalar subquery once and rewrite it into a row-local form.
/// Correlated subqueries (unknown outer column) are left untouched.
pub fn pre_evaluate_dml_predicate(
    expr: &ExprNode,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<ExprNode> {
    match expr {
        ExprNode::Binary(b) => {
            let mut b = b.clone();
            b.left = Box::new(pre_evaluate_dml_predicate(
                &b.left, domain, current_db, txn,
            )?);
            b.right = Box::new(pre_evaluate_dml_predicate(
                &b.right, domain, current_db, txn,
            )?);
            Ok(ExprNode::Binary(b))
        }
        ExprNode::Unary(u) => {
            let mut u = u.clone();
            u.expr = Box::new(pre_evaluate_dml_predicate(
                &u.expr, domain, current_db, txn,
            )?);
            Ok(ExprNode::Unary(u))
        }
        ExprNode::InSubquery(i) => {
            let result = match run_result_set(&i.query, domain, current_db, txn) {
                Ok(result) => result,
                // Unknown outer column: correlated, leave for the Apply path.
                Err(e) if e.code == ERR_BAD_FIELD => return Ok(expr.clone()),
                Err(e) => return Err(e),
            };
            let list = result
                .rows
                .into_iter()
                .map(|mut row| {
                    ExprNode::Value(ValueExpr {
                        datum: if row.is_empty() {
                            Datum::Null
                        } else {
                            row.swap_remove(0)
                        },
                    })
                })
                .collect();
            Ok(ExprNode::InList(InListExpr {
                expr: i.expr.clone(),
                not: i.not,
                list,
            }))
        }
        ExprNode::Exists(e) => {
            let result = match run_result_set(&e.query, domain, current_db, txn) {
                Ok(result) => result,
                Err(e) if e.code == ERR_BAD_FIELD => return Ok(expr.clone()),
                Err(e) => return Err(e),
            };
            let found = !result.rows.is_empty();
            let datum = Datum::I64(i64::from(found != e.not));
            Ok(ExprNode::Value(ValueExpr { datum }))
        }
        ExprNode::ScalarSubquery(s) => {
            let result = match run_result_set(&s.query, domain, current_db, txn) {
                Ok(result) => result,
                Err(e) if e.code == ERR_BAD_FIELD => return Ok(expr.clone()),
                Err(e) => return Err(e),
            };
            let datum = result
                .rows
                .into_iter()
                .next()
                .and_then(|mut row| {
                    if row.is_empty() {
                        None
                    } else {
                        Some(row.swap_remove(0))
                    }
                })
                .unwrap_or(Datum::Null);
            Ok(ExprNode::Value(ValueExpr { datum }))
        }
        _ => Ok(expr.clone()),
    }
}

fn collect_rows(exec: &mut Box<dyn Executor>) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = exec.next()? {
        for i in 0..chunk.num_rows() {
            rows.push(chunk.get_row(i));
        }
    }
    Ok(rows)
}

fn row_key(row: &[Datum]) -> Vec<String> {
    row.iter().map(Datum::display).collect()
}

/// First outer `col_name_list` name referenced by the recursive arm that is not
/// present in the seed output schema (TiDB same-name nested WITH shadowing).
fn unknown_outer_col_in_recursive(
    rec: &SelectStmt,
    seed_schema: &[String],
    outer_cols: &[String],
) -> Option<String> {
    if outer_cols.is_empty() {
        return None;
    }
    let mut found = None;
    let mut visit = |col: &ColumnNameExpr| {
        if found.is_some() {
            return;
        }
        let is_outer = outer_cols
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&col.name));
        if !is_outer {
            return;
        }
        let in_seed = seed_schema.iter().any(|s| {
            let bare = s.rsplit('.').next().unwrap_or(s.as_str());
            bare.eq_ignore_ascii_case(&col.name) || s.eq_ignore_ascii_case(&col.name)
        });
        if !in_seed {
            found = Some(col.name.clone());
        }
    };
    if let Some(sel) = &rec.selection {
        walk_expr_columns(sel, &mut visit);
    }
    for field in &rec.fields {
        walk_expr_columns(&field.expr, &mut visit);
    }
    found
}

fn walk_expr_columns(expr: &ExprNode, visit: &mut impl FnMut(&ColumnNameExpr)) {
    match expr {
        ExprNode::Column(c) => visit(c),
        ExprNode::Binary(b) => {
            walk_expr_columns(&b.left, visit);
            walk_expr_columns(&b.right, visit);
        }
        ExprNode::Unary(u) => walk_expr_columns(&u.expr, visit),
        ExprNode::Func(f) => {
            for arg in &f.args {
                walk_expr_columns(arg, visit);
            }
        }
        ExprNode::Cast(c) => walk_expr_columns(&c.expr, visit),
        ExprNode::InList(i) => {
            walk_expr_columns(&i.expr, visit);
            for e in &i.list {
                walk_expr_columns(e, visit);
            }
        }
        ExprNode::Between(b) => {
            walk_expr_columns(&b.expr, visit);
            walk_expr_columns(&b.low, visit);
            walk_expr_columns(&b.high, visit);
        }
        ExprNode::Like(l) => {
            walk_expr_columns(&l.expr, visit);
            walk_expr_columns(&l.pattern, visit);
        }
        ExprNode::IsNull(i) => walk_expr_columns(&i.expr, visit),
        ExprNode::Case(c) => {
            if let Some(operand) = &c.operand {
                walk_expr_columns(operand, visit);
            }
            for (when, then) in &c.when_then {
                walk_expr_columns(when, visit);
                walk_expr_columns(then, visit);
            }
            if let Some(branch) = &c.else_expr {
                walk_expr_columns(branch, visit);
            }
        }
        ExprNode::Value(_)
        | ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_)
        | ExprNode::Window(_) => {}
    }
}

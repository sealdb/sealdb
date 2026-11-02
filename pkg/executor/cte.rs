// F17 core path: CTEExec for WITH, including WITH RECURSIVE UNION/UNION ALL
// (TiDB pkg/executor/cte.go). Completeness wave / not this feature:
// Nested WITH / qualified CTE names.
use crate::builder::ExecutorBuilder;
use crate::internal::exec::{Executor, QueryResult};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{
    SelectField, SelectLock, SelectLockWait, SelectStmt, SetOprType, Statement, WithClause,
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
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
        }],
        from: Some(TableName::new(None, name)),
        joins: Vec::new(),
        selection: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
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
        self.schema = self.seed.schema().to_vec();
        self.rows = collect_rows(&mut self.seed)?;
        if !self.columns.is_empty() {
            let width = self
                .rows
                .first()
                .map(|row| row.len())
                .unwrap_or(self.schema.len());
            if width != 0 && self.columns.len() != width {
                return Err(Error::new(
                    ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT,
                    "The used SELECT statements have a different number of columns",
                ));
            }
            self.schema = self.columns.clone();
        }
        if let Some((op, rec)) = self.recursive.clone() {
            self.rows = iterate_recursive(
                &self.name,
                self.schema.clone(),
                std::mem::take(&mut self.rows),
                op,
                rec,
                &self.domain,
                &self.current_db,
                &self.txn,
            )?;
        }
        if self.share {
            store_shared_cte(&self.name, self.schema.clone(), self.rows.clone());
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
) -> Result<Vec<Vec<Datum>>> {
    let distinct = op.is_distinct();
    let mut all = seed;
    let mut working = all.clone();
    let mut seen: HashSet<Vec<String>> = all.iter().map(|row| row_key(row)).collect();
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
        working = step;
    }
    Ok(all)
}

fn run_select(
    select: &SelectStmt,
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<QueryResult> {
    let stmt = Statement::Select(select.clone());
    let stats = domain.stats();
    let plan = PlanBuilder::new().build_with_schema(
        &stmt,
        Some(&domain.infoschema()),
        current_db,
        Some(stats.as_ref()),
        sealdb_planner::PartitionPruneMode::Dynamic,
    )?;
    let exec = ExecutorBuilder::build(
        &plan,
        &stmt,
        Arc::clone(domain),
        current_db,
        Arc::clone(txn),
    )?;
    ExecutorBuilder::run(exec)
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

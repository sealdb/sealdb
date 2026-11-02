// F14 core path: NestedLoopApplyExec for EXISTS / IN / scalar / ANY/ALL.
// Parallel apply: SEALDB_APPLY_CONCURRENCY workers (default 1). Completeness
// wave / not this feature: semi/anti rewrite, subquery JOIN. Derived tables
// flatten to CTEs. SELECT-list scalar subqueries reuse SubqueryEvalCtx.
use crate::builder::ExecutorBuilder;
use crate::internal::exec::{Executor, QueryResult};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_SUBQUERY_NO_1_ROW, ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_expression::{collation_ci, eval_expr_with_row, eval_filter, with_collation_ci};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ColumnNameExpr, ExistsExpr, ExprNode, FuncCallExpr, FuncCastExpr,
    InListExpr, InSubqueryExpr, IsNullExpr, PatternLikeExpr, QuantifiedCmpExpr, ScalarSubqueryExpr,
    UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{SelectField, SelectStmt, Statement};
use sealdb_parser::opcode::Op;
use sealdb_planner::PlanBuilder;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

pub struct NestedLoopApplyExec {
    filter: ExprNode,
    child: Box<dyn Executor>,
    domain: Arc<Domain>,
    current_db: String,
    txn: Arc<TxnHandle>,
    outer_table: Option<String>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl NestedLoopApplyExec {
    pub fn new(
        filter: ExprNode,
        child: Box<dyn Executor>,
        domain: Arc<Domain>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        outer_table: Option<String>,
    ) -> Self {
        Self {
            filter,
            child,
            domain,
            current_db: current_db.into(),
            txn,
            outer_table,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for NestedLoopApplyExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.schema = self.child.schema().to_vec();
        let mut outer = Vec::new();
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                outer.push(chunk.get_row(i));
            }
        }
        self.rows = filter_outer_rows(
            outer,
            &self.filter,
            &self.schema,
            &self.domain,
            &self.current_db,
            &self.txn,
            &self.outer_table,
        )?;
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
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

/// Worker count for NestedLoopApply. Default 1 (serial); set
/// `SEALDB_APPLY_CONCURRENCY` to fan out outer rows across threads.
fn apply_concurrency() -> usize {
    std::env::var("SEALDB_APPLY_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(1)
}

fn filter_outer_rows(
    rows: Vec<Vec<Datum>>,
    filter: &ExprNode,
    schema: &[String],
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
    outer_table: &Option<String>,
) -> Result<Vec<Vec<Datum>>> {
    let workers = apply_concurrency().min(rows.len().max(1));
    if workers <= 1 {
        return filter_outer_rows_serial(
            rows,
            filter,
            schema,
            domain,
            current_db,
            txn,
            outer_table,
        );
    }
    let chunk_len = rows.len().div_ceil(workers);
    let mut parts = Vec::new();
    let mut rest = rows;
    while !rest.is_empty() {
        let take = chunk_len.min(rest.len());
        parts.push(rest.drain(..take).collect::<Vec<_>>());
    }
    let ci = collation_ci();
    let mut handles = Vec::with_capacity(parts.len());
    for part in parts {
        let filter = filter.clone();
        let schema = schema.to_vec();
        let domain = Arc::clone(domain);
        let current_db = current_db.to_string();
        let txn = Arc::clone(txn);
        let outer_table = outer_table.clone();
        handles.push(thread::spawn(move || {
            with_collation_ci(ci, || {
                filter_outer_rows_serial(
                    part,
                    &filter,
                    &schema,
                    &domain,
                    &current_db,
                    &txn,
                    &outer_table,
                )
            })
        }));
    }
    let mut out = Vec::new();
    for handle in handles {
        out.extend(
            handle
                .join()
                .map_err(|_| Error::new(ERR_UNKNOWN_ERROR, "parallel apply worker panicked"))??,
        );
    }
    Ok(out)
}

fn filter_outer_rows_serial(
    rows: Vec<Vec<Datum>>,
    filter: &ExprNode,
    schema: &[String],
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
    outer_table: &Option<String>,
) -> Result<Vec<Vec<Datum>>> {
    let mut cache = HashMap::new();
    let mut kept = Vec::new();
    for row in rows {
        let ctx = ApplyCtx {
            domain: Arc::clone(domain),
            current_db: current_db.to_string(),
            txn: Arc::clone(txn),
            outer_table: outer_table.clone(),
            outer_schema: schema.to_vec(),
            outer_row: row.clone(),
        };
        let rewritten = materialize(filter, &ctx, &mut cache)?;
        if eval_filter(&rewritten, schema, &row)? {
            kept.push(row);
        }
    }
    Ok(kept)
}

struct ApplyCtx {
    domain: Arc<Domain>,
    current_db: String,
    txn: Arc<TxnHandle>,
    outer_table: Option<String>,
    outer_schema: Vec<String>,
    outer_row: Vec<Datum>,
}

/// Runs nested subqueries while projecting a row (SELECT-list scalar subquery).
pub(crate) struct SubqueryEvalCtx {
    domain: Arc<Domain>,
    current_db: String,
    txn: Arc<TxnHandle>,
    outer_table: Option<String>,
    cache: HashMap<String, QueryResult>,
}

impl SubqueryEvalCtx {
    pub(crate) fn new(
        domain: Arc<Domain>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        outer_table: Option<String>,
    ) -> Self {
        Self {
            domain,
            current_db: current_db.into(),
            txn,
            outer_table,
            cache: HashMap::new(),
        }
    }

    /// Materialize subqueries in `expr` against `row`, then evaluate.
    pub(crate) fn eval(
        &mut self,
        expr: &ExprNode,
        schema: &[String],
        row: &[Datum],
    ) -> Result<Datum> {
        if !expr.has_subquery() {
            return eval_expr_with_row(expr, schema, row);
        }
        let ctx = ApplyCtx {
            domain: Arc::clone(&self.domain),
            current_db: self.current_db.clone(),
            txn: Arc::clone(&self.txn),
            outer_table: self.outer_table.clone(),
            outer_schema: schema.to_vec(),
            outer_row: row.to_vec(),
        };
        let rewritten = materialize(expr, &ctx, &mut self.cache)?;
        eval_expr_with_row(&rewritten, schema, row)
    }
}

fn materialize(
    expr: &ExprNode,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<ExprNode> {
    match expr {
        ExprNode::Exists(exists) => Ok(ExprNode::Value(ValueExpr {
            datum: eval_exists(exists, ctx, cache)?,
        })),
        ExprNode::InSubquery(sub) => Ok(ExprNode::Value(ValueExpr {
            datum: eval_in_subquery(sub, ctx, cache)?,
        })),
        ExprNode::ScalarSubquery(sub) => Ok(ExprNode::Value(ValueExpr {
            datum: eval_scalar_subquery(sub, ctx, cache)?,
        })),
        ExprNode::QuantifiedCmp(cmp) => Ok(ExprNode::Value(ValueExpr {
            datum: eval_quantified_cmp(cmp, ctx, cache)?,
        })),
        ExprNode::Binary(bin) => Ok(ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(materialize(&bin.left, ctx, cache)?),
            right: Box::new(materialize(&bin.right, ctx, cache)?),
        })),
        ExprNode::Unary(unary) => Ok(ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(materialize(&unary.expr, ctx, cache)?),
        })),
        ExprNode::Func(func) => Ok(ExprNode::Func(FuncCallExpr {
            name: func.name.clone(),
            args: func
                .args
                .iter()
                .map(|arg| materialize(arg, ctx, cache))
                .collect::<Result<Vec<_>>>()?,
            distinct: func.distinct,
        })),
        ExprNode::Cast(cast) => Ok(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(materialize(&cast.expr, ctx, cache)?),
            target: cast.target.clone(),
        })),
        ExprNode::InList(list) => Ok(ExprNode::InList(InListExpr {
            expr: Box::new(materialize(&list.expr, ctx, cache)?),
            not: list.not,
            list: list
                .list
                .iter()
                .map(|item| materialize(item, ctx, cache))
                .collect::<Result<Vec<_>>>()?,
        })),
        ExprNode::Between(between) => Ok(ExprNode::Between(BetweenExpr {
            expr: Box::new(materialize(&between.expr, ctx, cache)?),
            not: between.not,
            low: Box::new(materialize(&between.low, ctx, cache)?),
            high: Box::new(materialize(&between.high, ctx, cache)?),
        })),
        ExprNode::Like(like) => Ok(ExprNode::Like(PatternLikeExpr {
            expr: Box::new(materialize(&like.expr, ctx, cache)?),
            not: like.not,
            pattern: Box::new(materialize(&like.pattern, ctx, cache)?),
            ci: like.ci,
        })),
        ExprNode::IsNull(is_null) => Ok(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(materialize(&is_null.expr, ctx, cache)?),
            not: is_null.not,
        })),
        other => Ok(other.clone()),
    }
}

fn eval_exists(
    exists: &ExistsExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let mut inner = bind_select(&exists.query, ctx)?;
    inner.limit = Some(inner.limit.unwrap_or(1).min(1));
    let result = run_select(&inner, ctx, cache)?;
    Ok(bool_datum(result.rows.is_empty() == exists.not))
}

fn eval_in_subquery(
    sub: &InSubqueryExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let left_expr = materialize(&sub.expr, ctx, cache)?;
    let left = eval_expr_with_row(&left_expr, &ctx.outer_schema, &ctx.outer_row)?;
    if left.is_null() {
        return Ok(Datum::Null);
    }
    let inner = bind_select(&sub.query, ctx)?;
    let result = run_select(&inner, ctx, cache)?;
    let mut saw_null = false;
    for row in &result.rows {
        let value = row.first().cloned().unwrap_or(Datum::Null);
        if value.is_null() {
            saw_null = true;
            continue;
        }
        if left.compare(&value).is_eq() {
            return Ok(bool_datum(!sub.not));
        }
    }
    if saw_null {
        Ok(Datum::Null)
    } else {
        Ok(bool_datum(sub.not))
    }
}

/// Evaluate `left op ANY|ALL (subquery)` with three-valued SQL logic.
/// Empty ANY is false; empty ALL is true. A NULL comparison yields NULL
/// unless another row already decided the quantifier.
fn eval_quantified_cmp(
    cmp: &QuantifiedCmpExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let left_expr = materialize(&cmp.expr, ctx, cache)?;
    let left = eval_expr_with_row(&left_expr, &ctx.outer_schema, &ctx.outer_row)?;
    let inner = bind_select(&cmp.query, ctx)?;
    let result = run_select(&inner, ctx, cache)?;
    if result.rows.is_empty() {
        return Ok(bool_datum(cmp.all));
    }
    if left.is_null() {
        return Ok(Datum::Null);
    }
    let mut saw_null = false;
    for row in &result.rows {
        let value = row.first().cloned().unwrap_or(Datum::Null);
        if value.is_null() {
            saw_null = true;
            continue;
        }
        let matched = cmp_op(&left, cmp.op, &value);
        if cmp.all {
            if !matched {
                return Ok(bool_datum(false));
            }
        } else if matched {
            return Ok(bool_datum(true));
        }
    }
    if saw_null {
        Ok(Datum::Null)
    } else {
        Ok(bool_datum(cmp.all))
    }
}

fn cmp_op(left: &Datum, op: Op, right: &Datum) -> bool {
    let ord = left.compare(right);
    match op {
        Op::Eq => ord.is_eq(),
        Op::Ne => !ord.is_eq(),
        Op::Lt => ord.is_lt(),
        Op::Le => ord.is_le(),
        Op::Gt => ord.is_gt(),
        Op::Ge => ord.is_ge(),
        _ => false,
    }
}

fn eval_scalar_subquery(
    sub: &ScalarSubqueryExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let inner = bind_select(&sub.query, ctx)?;
    let result = run_select(&inner, ctx, cache)?;
    if result.rows.len() > 1 {
        return Err(Error::new(
            ERR_SUBQUERY_NO_1_ROW,
            "Subquery returns more than 1 row",
        ));
    }
    Ok(result
        .rows
        .first()
        .and_then(|row| row.first())
        .cloned()
        .unwrap_or(Datum::Null))
}

fn bind_select(select: &SelectStmt, ctx: &ApplyCtx) -> Result<SelectStmt> {
    let inner_table = select.from.as_ref().map(|t| t.name.clone());
    let inner_cols = inner_columns(select, ctx);
    let mut out = select.clone();
    out.fields = select
        .fields
        .iter()
        .map(|field| SelectField {
            expr: bind_expr(&field.expr, ctx, inner_table.as_deref(), &inner_cols),
            as_name: field.as_name.clone(),
        })
        .collect();
    out.selection = select
        .selection
        .as_ref()
        .map(|expr| bind_expr(expr, ctx, inner_table.as_deref(), &inner_cols));
    out.having = select
        .having
        .as_ref()
        .map(|expr| bind_expr(expr, ctx, inner_table.as_deref(), &inner_cols));
    out.group_by = select
        .group_by
        .iter()
        .map(|expr| bind_expr(expr, ctx, inner_table.as_deref(), &inner_cols))
        .collect();
    for item in &mut out.order_by {
        item.expr = bind_expr(&item.expr, ctx, inner_table.as_deref(), &inner_cols);
    }
    Ok(out)
}

fn inner_columns(select: &SelectStmt, ctx: &ApplyCtx) -> Vec<String> {
    let Some(from) = &select.from else {
        return Vec::new();
    };
    let (schema, name) = from.qualified(&ctx.current_db);
    ctx.domain
        .infoschema()
        .table_by_name(&schema, &name)
        .map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn bind_expr(
    expr: &ExprNode,
    ctx: &ApplyCtx,
    inner_table: Option<&str>,
    inner_cols: &[String],
) -> ExprNode {
    match expr {
        ExprNode::Column(col) => bind_column(col, ctx, inner_table, inner_cols),
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(bind_expr(&bin.left, ctx, inner_table, inner_cols)),
            right: Box::new(bind_expr(&bin.right, ctx, inner_table, inner_cols)),
        }),
        ExprNode::Unary(unary) => ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(bind_expr(&unary.expr, ctx, inner_table, inner_cols)),
        }),
        ExprNode::Func(func) => ExprNode::Func(FuncCallExpr {
            name: func.name.clone(),
            args: func
                .args
                .iter()
                .map(|arg| bind_expr(arg, ctx, inner_table, inner_cols))
                .collect(),
            distinct: func.distinct,
        }),
        ExprNode::Cast(cast) => ExprNode::Cast(FuncCastExpr {
            expr: Box::new(bind_expr(&cast.expr, ctx, inner_table, inner_cols)),
            target: cast.target.clone(),
        }),
        ExprNode::InList(list) => ExprNode::InList(InListExpr {
            expr: Box::new(bind_expr(&list.expr, ctx, inner_table, inner_cols)),
            not: list.not,
            list: list
                .list
                .iter()
                .map(|item| bind_expr(item, ctx, inner_table, inner_cols))
                .collect(),
        }),
        ExprNode::Between(between) => ExprNode::Between(BetweenExpr {
            expr: Box::new(bind_expr(&between.expr, ctx, inner_table, inner_cols)),
            not: between.not,
            low: Box::new(bind_expr(&between.low, ctx, inner_table, inner_cols)),
            high: Box::new(bind_expr(&between.high, ctx, inner_table, inner_cols)),
        }),
        ExprNode::Like(like) => ExprNode::Like(PatternLikeExpr {
            expr: Box::new(bind_expr(&like.expr, ctx, inner_table, inner_cols)),
            not: like.not,
            pattern: Box::new(bind_expr(&like.pattern, ctx, inner_table, inner_cols)),
            ci: like.ci,
        }),
        ExprNode::IsNull(is_null) => ExprNode::IsNull(IsNullExpr {
            expr: Box::new(bind_expr(&is_null.expr, ctx, inner_table, inner_cols)),
            not: is_null.not,
        }),
        ExprNode::Exists(exists) => ExprNode::Exists(ExistsExpr {
            not: exists.not,
            query: exists.query.clone(),
        }),
        ExprNode::InSubquery(sub) => ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(bind_expr(&sub.expr, ctx, inner_table, inner_cols)),
            not: sub.not,
            query: sub.query.clone(),
        }),
        ExprNode::QuantifiedCmp(cmp) => ExprNode::QuantifiedCmp(QuantifiedCmpExpr {
            expr: Box::new(bind_expr(&cmp.expr, ctx, inner_table, inner_cols)),
            op: cmp.op,
            all: cmp.all,
            query: cmp.query.clone(),
        }),
        ExprNode::ScalarSubquery(sub) => ExprNode::ScalarSubquery(sub.clone()),
        other => other.clone(),
    }
}

fn bind_column(
    col: &ColumnNameExpr,
    ctx: &ApplyCtx,
    inner_table: Option<&str>,
    inner_cols: &[String],
) -> ExprNode {
    if column_is_inner(col, inner_table, inner_cols) {
        return ExprNode::Column(col.clone());
    }
    if let Some(value) = lookup_outer(col, ctx) {
        return ExprNode::Value(ValueExpr { datum: value });
    }
    ExprNode::Column(col.clone())
}

fn column_is_inner(col: &ColumnNameExpr, inner_table: Option<&str>, inner_cols: &[String]) -> bool {
    if let Some(table) = &col.table {
        if inner_table.is_some_and(|inner| inner.eq_ignore_ascii_case(table)) {
            return true;
        }
        return false;
    }
    inner_cols
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&col.name))
}

fn lookup_outer(col: &ColumnNameExpr, ctx: &ApplyCtx) -> Option<Datum> {
    if let Some(table) = &col.table
        && ctx
            .outer_table
            .as_ref()
            .is_none_or(|outer| !outer.eq_ignore_ascii_case(table))
    {
        return None;
    }
    ctx.outer_schema
        .iter()
        .position(|name| {
            name.eq_ignore_ascii_case(&col.name)
                || name.eq_ignore_ascii_case(&format!(
                    "{}.{}",
                    col.table.as_deref().unwrap_or(""),
                    col.name
                ))
        })
        .and_then(|idx| ctx.outer_row.get(idx).cloned())
}

fn run_select(
    select: &SelectStmt,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<QueryResult> {
    let key = restore_select(select)?;
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    let stmt = Statement::Select(select.clone());
    let stats = ctx.domain.stats();
    let plan = PlanBuilder::new().build_with_schema(
        &stmt,
        Some(&ctx.domain.infoschema()),
        &ctx.current_db,
        Some(stats.as_ref()),
    )?;
    let exec = ExecutorBuilder::build(
        &plan,
        &stmt,
        Arc::clone(&ctx.domain),
        &ctx.current_db,
        Arc::clone(&ctx.txn),
    )?;
    let result = ExecutorBuilder::run(exec)?;
    cache.insert(key, result.clone());
    Ok(result)
}

fn restore_select(select: &SelectStmt) -> Result<String> {
    let mut ctx = sealdb_parser::format::RestoreCtx::new();
    select.restore(&mut ctx)?;
    Ok(ctx.into_string())
}

fn bool_datum(v: bool) -> Datum {
    Datum::I64(i64::from(v))
}

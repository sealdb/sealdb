// core path: NestedLoopApplyExec for EXISTS / IN / scalar / ANY/ALL.
// Parallel apply: SEALDB_APPLY_CONCURRENCY workers (default 1). Subquery
// bodies are ResultSetNode so UNION/EXCEPT/INTERSECT run through UnionExec.
// Completeness wave / not this feature: subquery JOIN. Derived tables
// flatten to CTEs. SELECT-list scalar subqueries reuse SubqueryEvalCtx.
use crate::builder::ExecutorBuilder;
use crate::cte::with_isolated_shared_ctes;
use crate::internal::exec::{Executor, QueryResult};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_SUBQUERY_NO_1_ROW, ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_expression::{collation_mode, eval_expr_with_row, eval_filter, truthy, with_collation};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, CaseExpr, ColumnNameExpr, ExistsExpr, ExprNode, FuncCallExpr,
    FuncCastExpr, InListExpr, InSubqueryExpr, IsNullExpr, PatternLikeExpr, QuantifiedCmpExpr,
    ScalarSubqueryExpr, UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{
    CommonTableExpression, ResultSetNode, SelectField, SelectStmt, SetOprStmt, Statement,
    WithClause,
};
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
    /// Outer SELECT WITH — EXISTS/IN bodies that reference those CTEs need it.
    outer_with: Option<WithClause>,
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
        outer_with: Option<WithClause>,
    ) -> Self {
        Self {
            filter,
            child,
            domain,
            current_db: current_db.into(),
            txn,
            outer_table,
            outer_with,
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
            self.outer_with.as_ref(),
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
    outer_with: Option<&WithClause>,
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
            outer_with,
        );
    }
    let chunk_len = rows.len().div_ceil(workers);
    let mut parts = Vec::new();
    let mut rest = rows;
    while !rest.is_empty() {
        let take = chunk_len.min(rest.len());
        parts.push(rest.drain(..take).collect::<Vec<_>>());
    }
    let mode = collation_mode();
    let mut handles = Vec::with_capacity(parts.len());
    for part in parts {
        let filter = filter.clone();
        let schema = schema.to_vec();
        let domain = Arc::clone(domain);
        let current_db = current_db.to_string();
        let txn = Arc::clone(txn);
        let outer_table = outer_table.clone();
        let outer_with = outer_with.cloned();
        handles.push(thread::spawn(move || {
            with_collation(mode, || {
                filter_outer_rows_serial(
                    part,
                    &filter,
                    &schema,
                    &domain,
                    &current_db,
                    &txn,
                    &outer_table,
                    outer_with.as_ref(),
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
    outer_with: Option<&WithClause>,
) -> Result<Vec<Vec<Datum>>> {
    let mut cache = HashMap::new();
    let mut kept = Vec::new();
    for row in rows {
        let ctx = ApplyCtx {
            domain: Arc::clone(domain),
            current_db: current_db.to_string(),
            txn: Arc::clone(txn),
            outer_table: outer_table.clone(),
            outer_with: outer_with.cloned(),
            outer_schema: schema.to_vec(),
            outer_row: row.clone(),
            enclosing_tables: Vec::new(),
            enclosing_cols: Vec::new(),
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
    outer_with: Option<WithClause>,
    outer_schema: Vec<String>,
    outer_row: Vec<Datum>,
    /// Columns of scopes between `outer_row` and the query being bound. A
    /// reference that names one of them stays a column (the executor's Apply
    /// resolves it against the enclosing row) instead of taking the outer
    /// value, so nested subqueries keep inner-scope columns inner.
    enclosing_tables: Vec<String>,
    enclosing_cols: Vec<String>,
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
            outer_with: None,
            outer_schema: schema.to_vec(),
            outer_row: row.to_vec(),
            enclosing_tables: Vec::new(),
            enclosing_cols: Vec::new(),
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
            single_cnf_item: false,
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
        // CASE is lazy in TiDB: only the branch that is taken runs, so an
        // untaken branch's subquery must not be materialized (it may even be
        // an erroring multi-row scalar). The caller evaluates the result against
        // exactly `ctx.outer_schema`/`ctx.outer_row`, so conditions can be
        // decided here.
        ExprNode::Case(case) => {
            let taken = match &case.operand {
                Some(operand) => {
                    let operand = materialize(operand, ctx, cache)?;
                    let operand = eval_expr_with_row(&operand, &ctx.outer_schema, &ctx.outer_row)?;
                    let mut taken = None;
                    for (when, then) in &case.when_then {
                        let when = materialize(when, ctx, cache)?;
                        let when = eval_expr_with_row(&when, &ctx.outer_schema, &ctx.outer_row)?;
                        let matched = !operand.is_null()
                            && !when.is_null()
                            && operand.compare(&when).is_eq();
                        if matched {
                            taken = Some(then);
                            break;
                        }
                    }
                    taken
                }
                None => {
                    let mut taken = None;
                    for (when, then) in &case.when_then {
                        let when = materialize(when, ctx, cache)?;
                        let cond = eval_expr_with_row(&when, &ctx.outer_schema, &ctx.outer_row)?;
                        if truthy(&cond).unwrap_or(false) {
                            taken = Some(then);
                            break;
                        }
                    }
                    taken
                }
            };
            match taken.or(case.else_expr.as_deref()) {
                Some(branch) => materialize(branch, ctx, cache),
                None => Ok(ExprNode::Value(ValueExpr {
                    datum: Datum::Null,
                })),
            }
        }
        other => Ok(other.clone()),
    }
}

fn eval_exists(
    exists: &ExistsExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let mut inner = bind_result_set(&exists.query, ctx)?;
    inner.cap_limit(1);
    let result = run_result_set(&inner, ctx, cache)?;
    Ok(bool_datum(result.rows.is_empty() == exists.not))
}

fn eval_in_subquery(
    sub: &InSubqueryExpr,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<Datum> {
    let left_expr = materialize(&sub.expr, ctx, cache)?;
    // Row constructors compare element-wise: `(a, b) IN (SELECT x, y …)`.
    let left_values: Vec<Datum> = match &left_expr {
        ExprNode::Row(row) => {
            let mut values = Vec::with_capacity(row.exprs.len());
            for item in &row.exprs {
                let item = materialize(item, ctx, cache)?;
                values.push(eval_expr_with_row(
                    &item,
                    &ctx.outer_schema,
                    &ctx.outer_row,
                )?);
            }
            values
        }
        other => vec![eval_expr_with_row(
            other,
            &ctx.outer_schema,
            &ctx.outer_row,
        )?],
    };
    let left_is_row = matches!(&left_expr, ExprNode::Row(_));
    if left_values.iter().any(Datum::is_null) {
        // Row-value `NOT IN` decorrelates to an anti semi join, where a NULL
        // operand never matches any inner row — TiDB answers TRUE.
        if left_is_row && sub.not {
            return Ok(bool_datum(true));
        }
        return Ok(Datum::Null);
    }
    let inner = bind_result_set(&sub.query, ctx)?;
    let result = run_result_set(&inner, ctx, cache)?;
    let mut saw_null = false;
    for row in &result.rows {
        let mut matched = true;
        let mut row_null = false;
        for (i, left) in left_values.iter().enumerate() {
            let value = row.get(i).cloned().unwrap_or(Datum::Null);
            if value.is_null() {
                row_null = true;
                matched = false;
                break;
            }
            if !left.compare(&value).is_eq() {
                matched = false;
                break;
            }
        }
        if matched {
            return Ok(bool_datum(!sub.not));
        }
        if row_null {
            saw_null = true;
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
    let inner = bind_result_set(&cmp.query, ctx)?;
    let result = run_result_set(&inner, ctx, cache)?;
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
    let inner = bind_result_set(&sub.query, ctx)?;
    let result = run_result_set(&inner, ctx, cache)?;
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

fn bind_result_set(query: &ResultSetNode, ctx: &ApplyCtx) -> Result<ResultSetNode> {
    match query {
        ResultSetNode::Select(select) => Ok(ResultSetNode::Select(bind_select(select, ctx)?)),
        ResultSetNode::SetOpr(set) => {
            let mut rest = Vec::with_capacity(set.rest.len());
            for (op, select) in &set.rest {
                rest.push((*op, bind_select(select, ctx)?));
            }
            Ok(ResultSetNode::SetOpr(SetOprStmt {
                first: bind_select(&set.first, ctx)?,
                rest,
                order_by: set.order_by.clone(),
                limit: set.limit,
                offset: set.offset,
                with: set.with.clone(),
            }))
        }
    }
}

fn bind_select(select: &SelectStmt, ctx: &ApplyCtx) -> Result<SelectStmt> {
    bind_select_with_cte_schemas(select, ctx, &HashMap::new())
}

fn bind_select_with_cte_schemas(
    select: &SelectStmt,
    ctx: &ApplyCtx,
    prior_ctes: &HashMap<String, Vec<String>>,
) -> Result<SelectStmt> {
    // A query's own columns are qualified by the alias (`FROM t t1` → t1.x).
    // Once aliased, the physical name is only a valid qualifier for CTEs, whose
    // corpus queries sometimes reference the CTE name instead of the alias.
    // Every FROM / JOIN table is inner, not just the leading one: a correlated
    // condition over `FROM t1, t3` must keep `t3.x` on the inner side.
    let mut inner_tables: Vec<String> = Vec::new();
    for t in select.join_tables() {
        let mut push = |name: &str| {
            if !inner_tables.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                inner_tables.push(name.to_string());
            }
        };
        push(&t.name);
        let cte_name = t.resolve_name();
        // An alias hides the table / CTE name (`FROM cte1 dt1` → only
        // `dt1.x` qualifies; `cte1.x` is an outer reference).
        let is_cte = t.physical.is_none()
            && (prior_ctes.contains_key(&cte_name.to_ascii_lowercase())
                || select
                    .with
                    .as_ref()
                    .is_some_and(|with| with.find(cte_name).is_some()));
        if is_cte {
            push(cte_name);
        }
    }
    let mut inner_cols = inner_columns(select, ctx, prior_ctes);
    // CTE output names count as inner so `WHERE b=a` keeps `b` on the CTE.
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            inner_cols.extend(cte_output_names(cte));
        }
    }
    let mut out = select.clone();
    // Attach outer WITH when the subquery references those CTEs (no_decorrelate
    // EXISTS over a WITH recursive cte1 defined on the outer SELECT).
    out.with = match (&select.with, &ctx.outer_with) {
        (Some(inner), Some(outer)) => Some(bind_with(
            &merge_with_clauses(outer, inner),
            ctx,
        )?),
        (Some(inner), None) => Some(bind_with(inner, ctx)?),
        (None, Some(outer)) => Some(bind_with(outer, ctx)?),
        (None, None) => None,
    };
    if let Some(with) = &out.with {
        for cte in &with.ctes {
            inner_cols.extend(cte_output_names(cte));
        }
    }
    out.fields = select
        .fields
        .iter()
        .map(|field| SelectField {
            expr: bind_expr(&field.expr, ctx, inner_tables.as_slice(), &inner_cols),
            as_name: field.as_name.clone(),
            orig_text: field.orig_text.clone(),
        })
        .collect();
    out.selection = select
        .selection
        .as_ref()
        .map(|expr| bind_expr(expr, ctx, inner_tables.as_slice(), &inner_cols));
    out.having = select
        .having
        .as_ref()
        .map(|expr| bind_expr(expr, ctx, inner_tables.as_slice(), &inner_cols));
    out.group_by = select
        .group_by
        .iter()
        .map(|expr| bind_expr(expr, ctx, inner_tables.as_slice(), &inner_cols))
        .collect();
    for item in &mut out.order_by {
        item.expr = bind_expr(&item.expr, ctx, inner_tables.as_slice(), &inner_cols);
    }
    Ok(out)
}

fn merge_with_clauses(outer: &WithClause, inner: &WithClause) -> WithClause {
    let mut ctes = outer.ctes.clone();
    for cte in &inner.ctes {
        if !ctes
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&cte.name))
        {
            ctes.push(cte.clone());
        }
    }
    WithClause {
        is_recursive: outer.is_recursive || inner.is_recursive,
        ctes,
    }
}

fn bind_with(with: &WithClause, ctx: &ApplyCtx) -> Result<WithClause> {
    let mut ctes = Vec::with_capacity(with.ctes.len());
    // CTE bodies must not re-attach the very WITH being bound: a body without
    // its own WITH would otherwise pull `ctx.outer_with` in again and recurse
    // forever (`WITH cte AS (SELECT …) SELECT … WHERE CASE … (SELECT … FROM cte)`).
    // Outer projection values are already substituted into the caller's plan.
    let body_ctx = ApplyCtx {
        domain: Arc::clone(&ctx.domain),
        current_db: ctx.current_db.clone(),
        txn: Arc::clone(&ctx.txn),
        outer_table: ctx.outer_table.clone(),
        outer_with: None,
        outer_schema: ctx.outer_schema.clone(),
        outer_row: ctx.outer_row.clone(),
        enclosing_tables: ctx.enclosing_tables.clone(),
        enclosing_cols: ctx.enclosing_cols.clone(),
    };
    let ctx = &body_ctx;
    // Sibling CTEs: later bodies see earlier output cols as inner so
    // `qn2 AS (SELECT 3*a FROM qn)` keeps `a` on qn (not outer correlation).
    let mut prior: HashMap<String, Vec<String>> = HashMap::new();
    for cte in &with.ctes {
        // The CTE's own output names are inner while binding its body, so a
        // recursive arm keeps `c1` on the CTE instead of the outer row.
        prior.insert(cte.name.to_ascii_lowercase(), cte_output_names(cte));
        let mut bound = cte.clone();
        bound.query = Box::new(bind_select_with_cte_schemas(&cte.query, ctx, &prior)?);
        if let Some((op, rec)) = &cte.recursive {
            bound.recursive = Some((*op, bind_select_with_cte_schemas(rec, ctx, &prior)?));
        }
        let mut rest = Vec::with_capacity(cte.set_rest.len());
        for (op, select) in &cte.set_rest {
            rest.push((*op, bind_select_with_cte_schemas(select, ctx, &prior)?));
        }
        bound.set_rest = rest;
        prior.insert(cte.name.to_ascii_lowercase(), cte_output_names(&bound));
        ctes.push(bound);
    }
    Ok(WithClause {
        is_recursive: with.is_recursive,
        ctes,
    })
}

fn cte_output_names(cte: &CommonTableExpression) -> Vec<String> {
    if !cte.col_name_list.is_empty() {
        return cte.col_name_list.clone();
    }
    let mut names = Vec::new();
    for field in &cte.query.fields {
        if let Some(name) = &field.as_name
            && !name.is_empty()
        {
            names.push(name.clone());
        } else if let ExprNode::Column(c) = &field.expr {
            names.push(c.name.clone());
        }
    }
    names
}

fn inner_columns(
    select: &SelectStmt,
    ctx: &ApplyCtx,
    prior_ctes: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let Some(from) = &select.from else {
        return Vec::new();
    };
    let cte_key = from.resolve_name().to_ascii_lowercase();
    if let Some(cols) = prior_ctes.get(&cte_key) {
        return cols.clone();
    }
    if let Some(with) = &select.with
        && let Some(cte) = with.find(from.resolve_name())
    {
        return cte_output_names(cte);
    }
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
    inner_tables: &[String],
    inner_cols: &[String],
) -> ExprNode {
    match expr {
        ExprNode::Column(col) => bind_column(col, ctx, inner_tables, inner_cols),
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(bind_expr(&bin.left, ctx, inner_tables, inner_cols)),
            right: Box::new(bind_expr(&bin.right, ctx, inner_tables, inner_cols)),
            single_cnf_item: false,
        }),
        ExprNode::Unary(unary) => ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(bind_expr(&unary.expr, ctx, inner_tables, inner_cols)),
        }),
        ExprNode::Func(func) => ExprNode::Func(FuncCallExpr {
            name: func.name.clone(),
            args: func
                .args
                .iter()
                .map(|arg| bind_expr(arg, ctx, inner_tables, inner_cols))
                .collect(),
            distinct: func.distinct,
        }),
        ExprNode::Cast(cast) => ExprNode::Cast(FuncCastExpr {
            expr: Box::new(bind_expr(&cast.expr, ctx, inner_tables, inner_cols)),
            target: cast.target.clone(),
        }),
        ExprNode::InList(list) => ExprNode::InList(InListExpr {
            expr: Box::new(bind_expr(&list.expr, ctx, inner_tables, inner_cols)),
            not: list.not,
            list: list
                .list
                .iter()
                .map(|item| bind_expr(item, ctx, inner_tables, inner_cols))
                .collect(),
        }),
        ExprNode::Between(between) => ExprNode::Between(BetweenExpr {
            expr: Box::new(bind_expr(&between.expr, ctx, inner_tables, inner_cols)),
            not: between.not,
            low: Box::new(bind_expr(&between.low, ctx, inner_tables, inner_cols)),
            high: Box::new(bind_expr(&between.high, ctx, inner_tables, inner_cols)),
        }),
        ExprNode::Like(like) => ExprNode::Like(PatternLikeExpr {
            expr: Box::new(bind_expr(&like.expr, ctx, inner_tables, inner_cols)),
            not: like.not,
            pattern: Box::new(bind_expr(&like.pattern, ctx, inner_tables, inner_cols)),
            ci: like.ci,
        }),
        ExprNode::IsNull(is_null) => ExprNode::IsNull(IsNullExpr {
            expr: Box::new(bind_expr(&is_null.expr, ctx, inner_tables, inner_cols)),
            not: is_null.not,
        }),
        ExprNode::Case(case) => ExprNode::Case(CaseExpr {
            operand: case
                .operand
                .as_ref()
                .map(|operand| Box::new(bind_expr(operand, ctx, inner_tables, inner_cols))),
            when_then: case
                .when_then
                .iter()
                .map(|(when, then)| {
                    (
                        bind_expr(when, ctx, inner_tables, inner_cols),
                        bind_expr(then, ctx, inner_tables, inner_cols),
                    )
                })
                .collect(),
            else_expr: case
                .else_expr
                .as_ref()
                .map(|branch| Box::new(bind_expr(branch, ctx, inner_tables, inner_cols))),
        }),
        // Nested subqueries are bound too: an inner subquery may correlate with
        // this query's row (`… WHERE b > (SELECT … WHERE c > (SELECT ref.a …))`),
        // and that reference must be substituted while this scope is available.
        ExprNode::Exists(exists) => ExprNode::Exists(ExistsExpr {
            not: exists.not,
            query: Box::new(bind_nested_query(
                &exists.query,
                &nested_ctx(ctx, inner_tables, inner_cols),
            )),
        }),
        ExprNode::InSubquery(sub) => ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(bind_expr(&sub.expr, ctx, inner_tables, inner_cols)),
            not: sub.not,
            query: Box::new(bind_nested_query(
                &sub.query,
                &nested_ctx(ctx, inner_tables, inner_cols),
            )),
            all_ne: sub.all_ne,
        }),
        ExprNode::QuantifiedCmp(cmp) => ExprNode::QuantifiedCmp(QuantifiedCmpExpr {
            expr: Box::new(bind_expr(&cmp.expr, ctx, inner_tables, inner_cols)),
            op: cmp.op,
            all: cmp.all,
            query: Box::new(bind_nested_query(
                &cmp.query,
                &nested_ctx(ctx, inner_tables, inner_cols),
            )),
        }),
        ExprNode::ScalarSubquery(sub) => ExprNode::ScalarSubquery(ScalarSubqueryExpr {
            query: Box::new(bind_nested_query(
                &sub.query,
                &nested_ctx(ctx, inner_tables, inner_cols),
            )),
        }),
        other => other.clone(),
    }
}

/// Scope for binding a subquery nested in this one: this query's tables are
/// enclosing scopes for it, so its references to them stay column references.
fn nested_ctx(ctx: &ApplyCtx, inner_tables: &[String], inner_cols: &[String]) -> ApplyCtx {
    let mut enclosing_tables = inner_tables.to_vec();
    enclosing_tables.extend(ctx.enclosing_tables.iter().cloned());
    let mut enclosing_cols = inner_cols.to_vec();
    enclosing_cols.extend(ctx.enclosing_cols.iter().cloned());
    ApplyCtx {
        domain: Arc::clone(&ctx.domain),
        current_db: ctx.current_db.clone(),
        txn: Arc::clone(&ctx.txn),
        outer_table: ctx.outer_table.clone(),
        outer_with: ctx.outer_with.clone(),
        outer_schema: ctx.outer_schema.clone(),
        outer_row: ctx.outer_row.clone(),
        enclosing_tables,
        enclosing_cols,
    }
}

/// Bind a subquery body in the enclosing scope; on failure keep it unchanged so
/// planning still reports the original error rather than a nested one.
fn bind_nested_query(query: &ResultSetNode, ctx: &ApplyCtx) -> ResultSetNode {
    bind_result_set(query, ctx).unwrap_or_else(|_| query.clone())
}

fn bind_column(
    col: &ColumnNameExpr,
    ctx: &ApplyCtx,
    inner_tables: &[String],
    inner_cols: &[String],
) -> ExprNode {
    if column_is_inner(col, inner_tables, inner_cols) || column_is_enclosing(col, ctx) {
        return ExprNode::Column(col.clone());
    }
    if let Some(value) = lookup_outer(col, ctx) {
        return ExprNode::Value(ValueExpr { datum: value });
    }
    ExprNode::Column(col.clone())
}

fn column_is_inner(col: &ColumnNameExpr, inner_tables: &[String], inner_cols: &[String]) -> bool {
    if let Some(table) = &col.table {
        if inner_tables
            .iter()
            .any(|inner| inner.eq_ignore_ascii_case(table))
        {
            return true;
        }
        return false;
    }
    inner_cols
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&col.name))
}

fn column_is_enclosing(col: &ColumnNameExpr, ctx: &ApplyCtx) -> bool {
    if let Some(table) = &col.table {
        return ctx
            .enclosing_tables
            .iter()
            .any(|inner| inner.eq_ignore_ascii_case(table));
    }
    ctx.enclosing_cols
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&col.name))
}

fn lookup_outer(col: &ColumnNameExpr, ctx: &ApplyCtx) -> Option<Datum> {
    // Outer schemas render columns as `t.c` or `db.t.c`; a qualifier is a hit
    // when the schema entry is that qualified name or ends with it, whatever
    // table the subquery's outer FROM started with.
    let bare = |s: &str| s.replace('`', "");
    let matches = |name: &str| -> bool {
        let name = bare(name);
        let name = name.as_str();
        if let Some(table) = &col.table {
            let qualified = format!("{table}.{}", col.name);
            name.eq_ignore_ascii_case(&qualified)
                // SELECT-list / aggregate schemas carry bare names.
                || name.eq_ignore_ascii_case(&col.name)
                || name
                    .rsplit_once('.')
                    .is_some_and(|(head, tail)| {
                        tail.eq_ignore_ascii_case(&col.name)
                            && head
                                .rsplit_once('.')
                                .map(|(_, t)| t.eq_ignore_ascii_case(table))
                                .unwrap_or_else(|| head.eq_ignore_ascii_case(table))
                    })
        } else {
            name.eq_ignore_ascii_case(&col.name)
                || name
                    .rsplit_once('.')
                    .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case(&col.name))
        }
    };
    ctx.outer_schema
        .iter()
        .position(|name| matches(name))
        .and_then(|idx| ctx.outer_row.get(idx).cloned())
}

fn run_result_set(
    query: &ResultSetNode,
    ctx: &ApplyCtx,
    cache: &mut HashMap<String, QueryResult>,
) -> Result<QueryResult> {
    let key = restore_result_set(query)?;
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    let stmt = match query {
        ResultSetNode::Select(select) => Statement::Select(select.clone()),
        ResultSetNode::SetOpr(set) => Statement::SetOpr(set.clone()),
    };
    let stats = ctx.domain.stats();
    let plan = PlanBuilder::new().build_with_schema(
        &stmt,
        Some(&ctx.domain.infoschema()),
        &ctx.current_db,
        Some(stats.as_ref()),
        sealdb_planner::PartitionPruneMode::Dynamic,
    )?;
    let exec = ExecutorBuilder::build(
        &plan,
        &stmt,
        Arc::clone(&ctx.domain),
        &ctx.current_db,
        Arc::clone(&ctx.txn),
    )?;
    // Fresh CTE share per outer row — correlated recursive CTEs must not reuse
    // a prior row's materialization under the same CTE name.
    let result = with_isolated_shared_ctes(|| ExecutorBuilder::run(exec))?;
    cache.insert(key, result.clone());
    Ok(result)
}

fn restore_result_set(query: &ResultSetNode) -> Result<String> {
    let mut ctx = sealdb_parser::format::RestoreCtx::new();
    query.restore(&mut ctx)?;
    Ok(ctx.into_string())
}

fn bool_datum(v: bool) -> Datum {
    Datum::I64(i64::from(v))
}

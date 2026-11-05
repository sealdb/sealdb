use crate::aggregate::{HashAggExec, StreamAggExec};
use crate::analyze::AnalyzeExec;
use crate::batch_point_get::BatchPointGetExec;
use crate::cte::{CTEExec, with_shared_ctes};
use crate::ddl::DDLExec;
use crate::delete::DeleteExec;
use crate::distsql::{IndexAccess, IndexLookUpExecutor, IndexMergeExecutor};
use crate::explain::ExplainExec;
use crate::grant::GrantExec;
use crate::insert::InsertExec;
use crate::internal::exec::{Executor, QueryResult};
use crate::join::apply::SubqueryEvalCtx;
use crate::join::{HashJoinV1Exec, NestedLoopApplyExec};
use crate::point_get::PointGetExec;
use crate::projection::ProjectionExec;
use crate::select::{DistinctExec, LimitExec, MemTableExec, SelectionExec, TableDualExec};
use crate::show::ShowExec;
use crate::sortexec::{SortExec, TopNExec};
use crate::table_reader::TableReaderExec;
use crate::unionexec::UnionExec;
use crate::update::UpdateExec;
use crate::window::WindowExec;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_kv::{LockWait, RowLock, Storage, TxnHandle};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{
    ByItem, ResultSetNode, SelectLock, SelectLockWait, SelectStmt, SetOprStmt, Statement, TableName,
};
use sealdb_planner::{JoinType, PhysicalPlan, PlanBuilder};
use std::sync::Arc;

pub struct ExecutorBuilder;

impl ExecutorBuilder {
    pub fn build(
        plan: &PhysicalPlan,
        stmt: &Statement,
        domain: Arc<Domain>,
        current_db: &str,
        txn: Arc<TxnHandle>,
    ) -> Result<Box<dyn Executor>> {
        match plan {
            PhysicalPlan::DDL { .. } => Ok(Box::new(DDLExec::new(
                domain,
                stmt.clone(),
                current_db.to_string(),
                true,
            ))),
            PhysicalPlan::Projection { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("projection without select"))?;
                // Child schema is empty until open(); resolve FROM columns via infoschema
                // for star expansion (`SELECT *, 1 FROM t` → c1, c2, 1).
                let from_cols = select_from_column_names(select, &domain, current_db);
                let child_exec = Self::build(
                    child,
                    stmt,
                    Arc::clone(&domain),
                    current_db,
                    Arc::clone(&txn),
                )?;
                let (exprs, names) = expand_projection_fields(select, &from_cols);
                let mut exec = ProjectionExec::new(exprs, names, child_exec);
                if select.fields.iter().any(|field| field.expr.has_subquery()) {
                    exec = exec.with_subquery(SubqueryEvalCtx::new(
                        domain,
                        current_db.to_string(),
                        txn,
                        select.from.as_ref().map(|table| table.name.clone()),
                    ));
                }
                Ok(Box::new(exec))
            }
            PhysicalPlan::Sort { child } => {
                let by = stmt_order_by(stmt)
                    .ok_or_else(|| Error::unsupported("sort without ORDER BY"))?;
                Ok(Box::new(SortExec::new(
                    by,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::TopN { child, .. } => {
                let by = stmt_order_by(stmt)
                    .ok_or_else(|| Error::unsupported("topn without ORDER BY"))?;
                let (limit, offset) =
                    stmt_limit(stmt).ok_or_else(|| Error::unsupported("topn without LIMIT"))?;
                let limit = limit.ok_or_else(|| Error::unsupported("topn without LIMIT"))?;
                Ok(Box::new(TopNExec::new(
                    by,
                    limit,
                    offset,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Limit { child, .. } => {
                let (limit, offset) =
                    stmt_limit(stmt).ok_or_else(|| Error::unsupported("limit without select"))?;
                Ok(Box::new(LimitExec::new(
                    limit,
                    offset,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Selection { child, filter } => Ok(Box::new(SelectionExec::new(
                filter.clone(),
                Self::build(child, stmt, domain, current_db, txn)?,
            ))),
            PhysicalPlan::Distinct { keys, child } => Ok(Box::new(DistinctExec::new(
                keys.clone(),
                Self::build(child, stmt, domain, current_db, txn)?,
            ))),
            PhysicalPlan::Simple { kind } if kind == "Analyze" => Ok(Box::new(AnalyzeExec::new(
                domain,
                stmt.clone(),
                current_db,
                txn,
            ))),
            PhysicalPlan::Simple { .. } => Ok(Box::new(GrantExec::new(domain, stmt.clone()))),
            PhysicalPlan::Show { .. } => {
                let show = match stmt {
                    Statement::Show(s) => s.clone(),
                    Statement::Use(_) => {
                        return Err(Error::unsupported("USE should be handled by session"));
                    }
                    _ => return Err(Error::unsupported("show without show statement")),
                };
                Ok(Box::new(ShowExec::new(
                    domain,
                    show,
                    current_db.to_string(),
                )))
            }
            PhysicalPlan::Insert => {
                let insert = match stmt {
                    Statement::Insert(s) => s.clone(),
                    _ => return Err(Error::unsupported("insert without insert statement")),
                };
                let select = insert
                    .select
                    .as_ref()
                    .map(|select| {
                        Self::build_select_arm(
                            select,
                            Arc::clone(&domain),
                            current_db,
                            Arc::clone(&txn),
                        )
                    })
                    .transpose()?;
                Ok(Box::new(InsertExec::new(
                    domain,
                    insert,
                    current_db.to_string(),
                    txn,
                    select,
                )))
            }
            PhysicalPlan::InfoSchema { table } => {
                let (columns, rows) =
                    sealdb_infoschema::tables::virtual_rows(table, &domain.infoschema())?;
                Ok(Box::new(MemTableExec::new(columns, rows)))
            }
            PhysicalPlan::Update => {
                let update = match stmt {
                    Statement::Update(s) => s.clone(),
                    _ => return Err(Error::unsupported("update without update statement")),
                };
                Ok(Box::new(UpdateExec::new(
                    domain,
                    update,
                    current_db.to_string(),
                    txn,
                )))
            }
            PhysicalPlan::Delete => {
                let delete = match stmt {
                    Statement::Delete(s) => s.clone(),
                    _ => return Err(Error::unsupported("delete without delete statement")),
                };
                Ok(Box::new(DeleteExec::new(
                    domain,
                    delete,
                    current_db.to_string(),
                    txn,
                )))
            }
            PhysicalPlan::IndexLookUpReader {
                index,
                partitions,
                pushed,
                ..
            }
            | PhysicalPlan::IndexReader {
                index,
                partitions,
                pushed,
                ..
            } => {
                let covering = matches!(plan, PhysicalPlan::IndexReader { .. });
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index lookup without select"))?;
                let from = select
                    .from
                    .clone()
                    .ok_or_else(|| Error::unsupported("index lookup without FROM"))?;
                let access =
                    index_access(&domain, current_db, &from, index, select.selection.as_ref())
                        .ok_or_else(|| Error::unsupported("index lookup without access path"))?;
                Ok(Box::new(IndexLookUpExecutor::new(
                    domain,
                    from,
                    index.clone(),
                    access,
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                    partitions.clone(),
                    pushed.clone(),
                    covering,
                )))
            }
            PhysicalPlan::IndexMerge {
                partitions, pushed, ..
            } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index merge without select"))?;
                let from = select
                    .from
                    .clone()
                    .ok_or_else(|| Error::unsupported("index merge without FROM"))?;
                Ok(Box::new(IndexMergeExecutor::new(
                    domain,
                    from,
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                    partitions.clone(),
                    pushed.clone(),
                    select.selection.clone(),
                )))
            }
            PhysicalPlan::PointGet { handle, index, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("point get without FROM"))?;
                let from = select
                    .from
                    .clone()
                    .ok_or_else(|| Error::unsupported("point get without FROM"))?;
                // Prefer literals from the current bound stmt so a plan cached
                // under a prepared `?` template still PointGets the right row.
                let handle = select
                    .selection
                    .as_ref()
                    .and_then(|expr| {
                        expr.split_cnf().into_iter().find_map(|item| {
                            item.eq_column_const().and_then(|(_, value)| match value {
                                sealdb_types::Datum::I64(h) => Some(h),
                                _ => None,
                            })
                        })
                    })
                    .or(*handle);
                let lookup = index.as_ref().and_then(|index_name| {
                    index_lookup_value(
                        &domain,
                        current_db,
                        &from,
                        index_name,
                        select.selection.as_ref(),
                    )
                });
                Ok(Box::new(PointGetExec::new(
                    domain,
                    from,
                    handle,
                    index.clone(),
                    lookup,
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                )))
            }
            PhysicalPlan::BatchPointGet { column, index, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("batch point get without FROM"))?;
                let from = select
                    .from
                    .clone()
                    .ok_or_else(|| Error::unsupported("batch point get without FROM"))?;
                let lookups = select
                    .selection
                    .as_ref()
                    .and_then(|expr| expr.in_const_on(column))
                    .ok_or_else(|| Error::unsupported("batch point get without IN list"))?;
                Ok(Box::new(BatchPointGetExec::new(
                    domain,
                    from,
                    index.clone(),
                    lookups,
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                )))
            }
            PhysicalPlan::TableReader {
                table,
                partitions,
                pushed,
            } => {
                let from = resolve_reader_table(stmt, table)?;
                Ok(Box::new(TableReaderExec::new(
                    domain,
                    from,
                    current_db.to_string(),
                    txn,
                    stmt_row_lock(stmt),
                    stmt_lock_wait(stmt),
                    partitions.clone(),
                    pushed.clone(),
                )))
            }
            PhysicalPlan::TableDual { .. } => Ok(Box::new(TableDualExec::new())),
            PhysicalPlan::HashAgg { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("hash agg without select"))?;
                let built = Self::build(child, stmt, domain, current_db, txn)?;
                // Outer DISTINCT HashAgg over StreamAgg: dedup projected columns.
                if matches!(
                    child.as_ref(),
                    PhysicalPlan::StreamAgg { .. } | PhysicalPlan::HashAgg { .. }
                ) {
                    let keys = select
                        .fields
                        .iter()
                        .map(|f| {
                            f.as_name
                                .clone()
                                .filter(|s| !s.is_empty())
                                .or_else(|| f.orig_text.clone())
                                .unwrap_or_else(|| f.expr.restore_string())
                        })
                        .collect();
                    return Ok(Box::new(DistinctExec::new(keys, built)));
                }
                Ok(Box::new(HashAggExec::new(select, built)?))
            }
            PhysicalPlan::StreamAgg { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("stream agg without select"))?;
                Ok(Box::new(StreamAggExec::new(
                    select,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )?))
            }
            PhysicalPlan::Window { child } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("window without select"))?
                    .clone();
                Ok(Box::new(WindowExec::new(
                    select,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Union { .. } => {
                let set = match stmt {
                    Statement::SetOpr(s) => s,
                    _ => return Err(Error::unsupported("union without set opr")),
                };
                let mut first = set.first.clone();
                if first.with.is_none() {
                    first.with = set.with.clone();
                }
                let mut exec = Self::build_select_arm(
                    &first,
                    Arc::clone(&domain),
                    current_db,
                    Arc::clone(&txn),
                )?;
                for (op, select) in &set.rest {
                    let mut select = select.clone();
                    if select.with.is_none() {
                        select.with = set.with.clone();
                    }
                    let right = Self::build_select_arm(
                        &select,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?;
                    exec = Box::new(UnionExec::new(exec, right, *op));
                }
                Ok(exec)
            }
            PhysicalPlan::Apply { child, filter } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("apply without select"))?;
                Ok(Box::new(NestedLoopApplyExec::new(
                    filter.clone(),
                    Self::build(
                        child,
                        stmt,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?,
                    domain,
                    current_db.to_string(),
                    txn,
                    select.from.as_ref().map(|t| t.name.clone()),
                )))
            }
            PhysicalPlan::CTE {
                name,
                columns,
                seed,
                inline,
            } => {
                let with = stmt
                    .with_clause()
                    .ok_or_else(|| Error::unsupported("CTE without WITH"))?;
                let first = with.seed_select(name)?;
                let rest = with.set_rest(name)?;
                let seed_stmt = if rest.is_empty() {
                    Statement::Select(first)
                } else {
                    Statement::SetOpr(SetOprStmt {
                        first,
                        rest,
                        order_by: Vec::new(),
                        limit: None,
                        offset: None,
                        with: None,
                    })
                };
                let recursive = with.find(name).and_then(|cte| cte.recursive.clone());
                let share = !inline;
                Ok(Box::new(CTEExec::new(
                    columns.clone(),
                    name.clone(),
                    recursive,
                    Self::build(seed, &seed_stmt, domain.clone(), current_db, txn.clone())?,
                    domain,
                    current_db,
                    txn,
                    share,
                )))
            }
            PhysicalPlan::HashJoin {
                tp,
                left,
                right,
                left_table,
                right_table,
                on,
            } => {
                let using_cols = stmt
                    .as_select()
                    .map(|select| {
                        select
                            .joins
                            .iter()
                            .find(|j| {
                                j.left.name.eq_ignore_ascii_case(left_table)
                                    && j.right.name.eq_ignore_ascii_case(right_table)
                            })
                            .map(|j| j.using_cols.clone())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let join = sealdb_parser::ast::Join {
                    left: sealdb_parser::ast::TableName::new(None, left_table.clone()),
                    right: sealdb_parser::ast::TableName::new(None, right_table.clone()),
                    tp: ast_join_type(*tp),
                    on: on.clone(),
                    using_cols,
                };
                Ok(Box::new(HashJoinV1Exec::new(
                    join,
                    Self::build(
                        left,
                        stmt,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?,
                    Self::build(right, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Explain {
                format,
                analyze,
                child,
            } => {
                if *analyze {
                    let inner = match stmt {
                        Statement::Explain(explain) => explain.stmt.as_ref(),
                        other => other,
                    };
                    let child_exec = Self::build(
                        child,
                        inner,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?;
                    Ok(Box::new(ExplainExec::with_analyze(
                        *format,
                        child.as_ref().clone(),
                        child_exec,
                    )))
                } else {
                    Ok(Box::new(ExplainExec::new(*format, child.as_ref().clone())))
                }
            }
        }
    }

    fn build_select_arm(
        select: &SelectStmt,
        domain: Arc<Domain>,
        current_db: &str,
        txn: Arc<TxnHandle>,
    ) -> Result<Box<dyn Executor>> {
        let stmt = Statement::Select(select.clone());
        let stats = domain.stats();
        let plan = PlanBuilder::new().build_with_schema(
            &stmt,
            Some(&domain.infoschema()),
            current_db,
            Some(stats.as_ref()),
            sealdb_planner::PartitionPruneMode::Dynamic,
        )?;
        Self::build(&plan, &stmt, domain, current_db, txn)
    }

    pub fn build_select(
        plan: &PhysicalPlan,
        select: &sealdb_parser::ast::SelectStmt,
    ) -> Result<Box<dyn Executor>> {
        let domain = Arc::new(Domain::new());
        let txn = Arc::new(TxnHandle::new(domain.begin()?));
        Self::build(plan, &Statement::Select(select.clone()), domain, "", txn)
    }

    pub fn run(mut exec: Box<dyn Executor>) -> Result<QueryResult> {
        with_shared_ctes(|| {
            exec.open()?;
            let columns = exec.schema().to_vec();
            let mut rows = Vec::new();
            while let Some(chunk) = exec.next()? {
                for i in 0..chunk.num_rows() {
                    rows.push(chunk.get_row(i));
                }
            }
            exec.close()?;
            Ok(QueryResult { columns, rows })
        })
    }
}

fn stmt_order_by(stmt: &Statement) -> Option<Vec<ByItem>> {
    match stmt {
        Statement::Select(select) if !select.order_by.is_empty() => Some(select.order_by.clone()),
        Statement::SetOpr(set) if !set.order_by.is_empty() => Some(set.order_by.clone()),
        _ => None,
    }
}

fn stmt_limit(stmt: &Statement) -> Option<(Option<u64>, u64)> {
    match stmt {
        Statement::Select(select) if select.limit.is_some() || select.offset.is_some() => {
            Some((select.limit, select.offset.unwrap_or(0)))
        }
        Statement::SetOpr(set) if set.limit.is_some() || set.offset.is_some() => {
            Some((set.limit, set.offset.unwrap_or(0)))
        }
        _ => None,
    }
}

fn resolve_reader_table(stmt: &Statement, table: &str) -> Result<sealdb_parser::ast::TableName> {
    let select = stmt
        .as_select()
        .ok_or_else(|| Error::unsupported("table reader without FROM"))?;
    find_table_in_select(select, table)
        .or_else(|| select.from.clone())
        .ok_or_else(|| Error::unsupported("table reader without FROM"))
}

fn find_table_in_result_set(query: &ResultSetNode, table: &str) -> Option<TableName> {
    query
        .selects()
        .into_iter()
        .find_map(|select| find_table_in_select(select, table))
}

/// Resolve a TableReader name against FROM, JOIN, or a subquery in WHERE
/// (EXISTS / IN rewritten to HashJoin Semi still need the inner table).
fn find_table_in_select(select: &SelectStmt, table: &str) -> Option<TableName> {
    if let Some(from) = &select.from
        && from.name.eq_ignore_ascii_case(table)
    {
        return Some(from.clone());
    }
    for join in &select.joins {
        if join.left.name.eq_ignore_ascii_case(table) {
            return Some(join.left.clone());
        }
        if join.right.name.eq_ignore_ascii_case(table) {
            return Some(join.right.clone());
        }
    }
    select
        .selection
        .as_ref()
        .and_then(|expr| find_table_in_expr(expr, table))
}

fn find_table_in_expr(expr: &ExprNode, table: &str) -> Option<TableName> {
    match expr {
        ExprNode::Exists(e) => find_table_in_result_set(&e.query, table),
        ExprNode::InSubquery(s) => {
            find_table_in_result_set(&s.query, table).or_else(|| find_table_in_expr(&s.expr, table))
        }
        ExprNode::ScalarSubquery(s) => find_table_in_result_set(&s.query, table),
        ExprNode::QuantifiedCmp(q) => {
            find_table_in_result_set(&q.query, table).or_else(|| find_table_in_expr(&q.expr, table))
        }
        ExprNode::Binary(b) => {
            find_table_in_expr(&b.left, table).or_else(|| find_table_in_expr(&b.right, table))
        }
        ExprNode::Unary(u) => find_table_in_expr(&u.expr, table),
        ExprNode::Func(f) => f.args.iter().find_map(|arg| find_table_in_expr(arg, table)),
        ExprNode::Cast(c) => find_table_in_expr(&c.expr, table),
        ExprNode::InList(i) => find_table_in_expr(&i.expr, table).or_else(|| {
            i.list
                .iter()
                .find_map(|item| find_table_in_expr(item, table))
        }),
        ExprNode::Between(b) => find_table_in_expr(&b.expr, table)
            .or_else(|| find_table_in_expr(&b.low, table))
            .or_else(|| find_table_in_expr(&b.high, table)),
        ExprNode::Like(l) => {
            find_table_in_expr(&l.expr, table).or_else(|| find_table_in_expr(&l.pattern, table))
        }
        ExprNode::IsNull(i) => find_table_in_expr(&i.expr, table),
        ExprNode::Window(w) => w.args.iter().find_map(|arg| find_table_in_expr(arg, table)),
        ExprNode::Column(_) | ExprNode::Value(_) => None,
    }
}

fn index_lookup_value(
    domain: &Domain,
    current_db: &str,
    table: &sealdb_parser::ast::TableName,
    index_name: &str,
    selection: Option<&ExprNode>,
) -> Option<sealdb_types::Datum> {
    let (schema, name) = table.qualified(current_db);
    let is = domain.infoschema();
    let info = is.table_by_name(&schema, &name)?;
    let index = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))?;
    let column = index.columns.first()?.name.o().to_string();
    if let Some(value) = selection.and_then(|expr| expr.eq_const_on(&column)) {
        return Some(value);
    }
    selection
        .and_then(|expr| expr.in_const_on(&column))
        .and_then(|values| values.into_iter().next())
}

fn index_access(
    domain: &Domain,
    current_db: &str,
    table: &sealdb_parser::ast::TableName,
    index_name: &str,
    selection: Option<&ExprNode>,
) -> Option<IndexAccess> {
    let (schema, name) = table.qualified(current_db);
    let is = domain.infoschema();
    let info = is.table_by_name(&schema, &name)?;
    let index = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))?;
    let column = index.columns.first()?.name.o().to_string();
    if let Some(value) = selection.and_then(|expr| expr.eq_const_on(&column)) {
        return Some(IndexAccess::Equal(value));
    }
    if let Some(range) = selection.and_then(|expr| {
        expr.column_ranges()
            .into_iter()
            .find(|range| range.column.eq_ignore_ascii_case(&column))
    }) {
        if range.low.is_some() || range.high.is_some() {
            return Some(IndexAccess::Range {
                low: range.low.as_ref().map(|(value, _)| value.clone()),
                low_inclusive: range
                    .low
                    .as_ref()
                    .map(|(_, inclusive)| *inclusive)
                    .unwrap_or(true),
                high: range.high.as_ref().map(|(value, _)| value.clone()),
                high_inclusive: range
                    .high
                    .as_ref()
                    .map(|(_, inclusive)| *inclusive)
                    .unwrap_or(true),
            });
        }
    }
    // USE INDEX / covering IndexReader with no predicate → IndexFullScan.
    Some(IndexAccess::Range {
        low: None,
        low_inclusive: true,
        high: None,
        high_inclusive: true,
    })
}

fn select_row_lock(select: &SelectStmt) -> RowLock {
    match select.lock {
        SelectLock::ForUpdate => RowLock::Exclusive,
        SelectLock::ForShare => RowLock::Share,
        SelectLock::None => RowLock::None,
    }
}

fn stmt_row_lock(stmt: &Statement) -> RowLock {
    stmt.as_select()
        .map(select_row_lock)
        .unwrap_or(RowLock::None)
}

fn select_lock_wait(select: &SelectStmt) -> LockWait {
    match select.lock_wait {
        SelectLockWait::Nowait => LockWait::Nowait,
        SelectLockWait::SkipLocked => LockWait::SkipLocked,
        SelectLockWait::Block => LockWait::Block,
    }
}

fn stmt_lock_wait(stmt: &Statement) -> LockWait {
    stmt.as_select()
        .map(select_lock_wait)
        .unwrap_or(LockWait::Block)
}

fn ast_join_type(tp: JoinType) -> sealdb_parser::ast::JoinType {
    match tp {
        JoinType::Inner => sealdb_parser::ast::JoinType::Inner,
        JoinType::LeftOuter => sealdb_parser::ast::JoinType::LeftOuter,
        JoinType::RightOuter => sealdb_parser::ast::JoinType::RightOuter,
        JoinType::Cross => sealdb_parser::ast::JoinType::Cross,
        JoinType::Semi => sealdb_parser::ast::JoinType::Semi,
        JoinType::Anti => sealdb_parser::ast::JoinType::Anti,
    }
}

fn field_output_name(field: &sealdb_parser::ast::SelectField) -> String {
    if let Some(name) = &field.as_name
        && !name.is_empty()
    {
        return name.clone();
    }
    // TiDB `buildProjectionFieldNameFromExpressions` (logical_plan_builder.go).
    tidb_projection_field_name(&field.expr, field.orig_text.as_deref())
}

fn is_star_expr(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Value(v) if matches!(&v.datum, sealdb_types::Datum::Bytes(b) if b == b"*")
    )
}

/// Expand select-list stars against the FROM schema (TiDB ResolveFields).
fn expand_projection_fields(
    select: &sealdb_parser::ast::SelectStmt,
    from_cols: &[String],
) -> (Vec<ExprNode>, Vec<String>) {
    let mut exprs = Vec::new();
    let mut names = Vec::new();
    for field in &select.fields {
        if is_star_expr(&field.expr) {
            for col in from_cols {
                exprs.push(ExprNode::Column(
                    sealdb_parser::ast::expressions::ColumnNameExpr {
                        schema: None,
                        table: None,
                        name: col.clone(),
                    },
                ));
                names.push(col.clone());
            }
        } else {
            exprs.push(field.expr.clone());
            names.push(field_output_name(field));
        }
    }
    (exprs, names)
}

/// Column names visible to `SELECT *` from the FROM/JOIN clause.
fn select_from_column_names(
    select: &sealdb_parser::ast::SelectStmt,
    domain: &Domain,
    current_db: &str,
) -> Vec<String> {
    let is = domain.infoschema();
    let mut cols = Vec::new();
    let mut push_table = |table: &TableName| {
        let (schema, name) = table.qualified(current_db);
        if let Some(info) = is.table_by_name(&schema, &name) {
            for c in &info.columns {
                cols.push(c.name.o().to_string());
            }
        }
    };
    if let Some(from) = &select.from {
        push_table(from);
    }
    for join in &select.joins {
        push_table(&join.right);
    }
    cols
}

/// TiDB projection naming: unwrap unary `+` / parens, then literal rules or `field.Text()`.
fn tidb_projection_field_name(expr: &ExprNode, orig_text: Option<&str>) -> String {
    let inner = unwrap_paren_and_unary_plus(expr);
    match inner {
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::Bytes(b) => {
                // String literal: content with leading non-graph trimmed (TiDB #3686/#3994).
                let s = String::from_utf8_lossy(b);
                s.trim_start_matches(|c: char| {
                    !(c.is_ascii_alphanumeric()
                        || (c as u32 > 0x20 && c as u32 <= 0x7e && !c.is_ascii_whitespace()))
                })
                .to_string()
            }
            sealdb_types::Datum::Null => "NULL".into(),
            sealdb_types::Datum::I64(_)
            | sealdb_types::Datum::U64(_)
            | sealdb_types::Datum::F64(_)
            | sealdb_types::Datum::Decimal(_) => {
                // Trim leading `+(` / whitespace and trailing `)` from original text.
                let raw = orig_text.unwrap_or("").trim();
                let trimmed = raw
                    .trim_start_matches(|c: char| matches!(c, '\t' | '\n' | ' ' | '+' | '('))
                    .trim_end_matches(|c: char| matches!(c, '\t' | '\n' | ' ' | ')'));
                if trimmed.is_empty() {
                    tidb_expr_display_name(inner)
                } else {
                    // Preserve TRUE/FALSE spelling from original when boolean literals.
                    let upper = trimmed.to_ascii_uppercase();
                    if upper == "TRUE" || upper == "FALSE" || upper == "NULL" {
                        upper
                    } else {
                        trimmed.to_string()
                    }
                }
            }
            other => format!("{other:?}"),
        },
        ExprNode::Column(_) => tidb_expr_display_name(inner),
        _ => {
            // Non-literal: TiDB uses field.Text() (comments stripped).
            if let Some(text) = orig_text {
                text.trim().to_string()
            } else {
                tidb_expr_display_name(expr)
            }
        }
    }
}

fn unwrap_paren_and_unary_plus(expr: &ExprNode) -> &ExprNode {
    match expr {
        ExprNode::Unary(u) if matches!(u.op, sealdb_parser::opcode::Op::Plus) => {
            unwrap_paren_and_unary_plus(&u.expr)
        }
        other => other,
    }
}

/// TiDB select-list display names: bare idents for top-level columns,
/// keep `t.c1` inside compound expressions, no spaces around `=`.
fn tidb_expr_display_name(expr: &ExprNode) -> String {
    tidb_expr_display_name_inner(expr, false)
}

fn tidb_expr_display_name_inner(expr: &ExprNode, keep_table_qual: bool) -> String {
    match expr {
        ExprNode::Column(col) => {
            if keep_table_qual {
                match &col.table {
                    Some(table) => format!("{table}.{}", col.name),
                    None => col.name.clone(),
                }
            } else {
                // Top-level `SELECT t.c1` → column name `c1`.
                col.name.clone()
            }
        }
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::I64(n) => n.to_string(),
            sealdb_types::Datum::U64(n) => n.to_string(),
            sealdb_types::Datum::F64(n) => n.to_string(),
            sealdb_types::Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            sealdb_types::Datum::Null => "NULL".into(),
            other => format!("{other:?}"),
        },
        ExprNode::Binary(b) => {
            let op = match b.op {
                sealdb_parser::opcode::Op::Eq => "=".to_string(),
                sealdb_parser::opcode::Op::Ne => "!=".to_string(),
                sealdb_parser::opcode::Op::Lt => "<".to_string(),
                sealdb_parser::opcode::Op::Le => "<=".to_string(),
                sealdb_parser::opcode::Op::Gt => ">".to_string(),
                sealdb_parser::opcode::Op::Ge => ">=".to_string(),
                sealdb_parser::opcode::Op::Plus => " + ".to_string(),
                sealdb_parser::opcode::Op::Minus => " - ".to_string(),
                sealdb_parser::opcode::Op::Mul => " * ".to_string(),
                sealdb_parser::opcode::Op::Div => " / ".to_string(),
                other => format!(" {} ", other.restore()),
            };
            format!(
                "{}{}{}",
                tidb_expr_display_name_inner(&b.left, true),
                op,
                tidb_expr_display_name_inner(&b.right, true)
            )
        }
        other if other.has_subquery() => other.restore_string(),
        other => other.restore_string(),
    }
}

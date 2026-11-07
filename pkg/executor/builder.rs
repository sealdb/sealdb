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
use crate::join::{HashJoinV1Exec, NestedLoopApplyExec, NullAwareAntiJoinExec};
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
            PhysicalPlan::Projection { exprs, child } => {
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
                // Window plans synthesize an identity projection above `Window`
                // (`Column#8->Column#9`); `WindowExec` already emits the
                // SELECT-list columns, so re-evaluating the fields would call
                // window functions outside `WindowExec`.
                if is_window_plan(&child) && is_identity_projection(exprs) {
                    let names: Vec<String> = select.fields.iter().map(field_output_name).collect();
                    return Ok(Box::new(ProjectionExec::passthrough(names, child_exec)));
                }
                // HAVING decorates the aggregate input with a projection whose
                // exprs are EXPLAIN placeholders (`cast(…) ->Column#13`); the
                // aggregate executor evaluates its arguments against the child
                // rows, so the projection only passes rows through.
                if select.having.is_some()
                    && !is_agg_plan(&child)
                    && is_explain_only_projection(exprs)
                {
                    return Ok(child_exec);
                }
                // `SELECT g, count(*) … HAVING …` plans a Projection above the
                // aggregate purely for EXPLAIN shape; the aggregate executor
                // already emits the SELECT-list columns in order.
                if is_agg_plan(&child) && is_agg_rename_projection(select) {
                    let names: Vec<String> = select.fields.iter().map(field_output_name).collect();
                    return Ok(Box::new(ProjectionExec::passthrough(names, child_exec)));
                }
                // GLOBAL index aggregation puts a placeholder cast/rename
                // Projection between the HashAgg and the index reader. Only the
                // aggregate executor evaluates the aggregate arguments against
                // the reader's rows, so drop the projection entirely.
                if is_explain_only_projection(exprs)
                    && select
                        .fields
                        .iter()
                        .any(|field| matches!(&field.expr, ExprNode::Func(func) if crate::aggfuncs::is_agg_name(&func.name)))
                {
                    return Ok(child_exec);
                }
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
            PhysicalPlan::Sort { child, by_info } => {
                let by = stmt_order_by(stmt)
                    .or_else(|| order_by_from_plan_info(by_info))
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
            PhysicalPlan::Selection { child, filter } => {
                // TiDB's printed HAVING filter references the aggregate's
                // `Column#N` outputs; `HashAggExec` already evaluates HAVING, so
                // the selection is EXPLAIN-only and must not run.
                if stmt.as_select().is_some_and(|s| s.having.is_some())
                    && is_explain_only_filter(filter)
                {
                    return Self::build(child, stmt, domain, current_db, txn);
                }
                let under_index = matches!(
                    child.as_ref(),
                    PhysicalPlan::IndexReader { .. }
                        | PhysicalPlan::IndexLookUpReader { .. }
                        | PhysicalPlan::IndexMerge { .. }
                );
                Ok(Box::new(SelectionExec::new(
                    filter.clone(),
                    Self::build(child, stmt, domain, current_db, txn)?,
                ).with_index_dst_double(under_index)))
            }
            PhysicalPlan::Distinct {
                keys,
                fields,
                names,
                child,
            } => Ok(Box::new(DistinctExec::new(
                keys.clone(),
                fields.clone(),
                names.clone(),
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
            PhysicalPlan::Insert { .. } => {
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
            PhysicalPlan::Update { .. } => {
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
            PhysicalPlan::Delete { .. } => {
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
                table,
                index,
                partitions,
                pushed,
                ..
            }
            | PhysicalPlan::IndexReader {
                table,
                index,
                partitions,
                pushed,
                ..
            } => {
                let covering = matches!(plan, PhysicalPlan::IndexReader { .. });
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index lookup without select"))?;
                // The reader may belong to any table of a joined SELECT (index
                // join / applied subquery), not only the leading FROM factor.
                let from = index_reader_table(select, table).ok_or_else(|| {
                    Error::unsupported("index lookup without FROM")
                })?;
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
            PhysicalPlan::PointGet {
                table,
                handle,
                index,
                partition_names,
                ..
            } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("point get without FROM"))?;
                // Multi-table joins push `pk = const` into one side, so the plan
                // names the table to fetch; single-table plans can fall back to
                // FROM (prepare templates keep the bound literal in WHERE).
                let from = resolve_reader_table(stmt, table)?;
                // Prefer plan literals; a plan cached under a prepared `?`
                // template keeps the bound value in the current stmt's WHERE.
                let handle = (*handle).or_else(|| {
                    select.selection.as_ref().and_then(|expr| {
                        expr.split_cnf().into_iter().find_map(|item| {
                            item.eq_column_const().and_then(|(_, value)| match value {
                                sealdb_types::Datum::I64(h) => Some(h),
                                _ => None,
                            })
                        })
                    })
                });
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
                    partition_names.clone(),
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                )))
            }
            PhysicalPlan::BatchPointGet {
                table,
                column,
                index,
                ..
            } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("batch point get without FROM"))?;
                let from = resolve_reader_table(stmt, table)?;
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
                ..
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
            PhysicalPlan::PartitionUnion { children } => {
                let mut iter = children.iter();
                let first = iter
                    .next()
                    .ok_or_else(|| Error::unsupported("empty PartitionUnion"))?;
                let mut exec =
                    Self::build(first, stmt, Arc::clone(&domain), current_db, Arc::clone(&txn))?;
                for child in iter {
                    let right =
                        Self::build(child, stmt, Arc::clone(&domain), current_db, Arc::clone(&txn))?;
                    exec = Box::new(UnionExec::new(
                        exec,
                        right,
                        sealdb_parser::ast::SetOprType::UnionAll,
                    ));
                }
                Ok(exec)
            }
            PhysicalPlan::TableDual { .. } => Ok(Box::new(TableDualExec::new())),
            PhysicalPlan::HashAgg { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("hash agg without select"))?;
                let built = Self::build(
                    child,
                    stmt,
                    Arc::clone(&domain),
                    current_db,
                    Arc::clone(&txn),
                )?;
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
                    // `SELECT DISTINCT *` keeps the full child row.
                    let star = select.fields.iter().all(|f| {
                        matches!(
                            &f.expr,
                            sealdb_parser::ast::expressions::ExprNode::Value(
                                sealdb_parser::ast::expressions::ValueExpr {
                                    datum: sealdb_types::Datum::Bytes(b),
                                }
                            ) if b == b"*"
                        )
                    });
                    let fields = if star {
                        Vec::new()
                    } else {
                        select.fields.iter().map(|f| f.expr.clone()).collect()
                    };
                    let names = if star {
                        Vec::new()
                    } else {
                        select
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                f.as_name
                                    .clone()
                                    .filter(|s| !s.is_empty())
                                    .or_else(|| f.orig_text.clone())
                                    .unwrap_or_else(|| format!("col{i}"))
                            })
                            .collect()
                    };
                    return Ok(Box::new(DistinctExec::new(keys, fields, names, built)));
                }
                let mut exec = HashAggExec::new(select, built)?;
                if let Some(ctx) = agg_subquery_ctx(select, domain, current_db, txn) {
                    exec = exec.with_subquery(ctx);
                }
                Ok(Box::new(exec))
            }
            PhysicalPlan::StreamAgg { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("stream agg without select"))?;
                let ctx =
                    agg_subquery_ctx(select, Arc::clone(&domain), current_db, Arc::clone(&txn));
                let mut exec = StreamAggExec::new(
                    select,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )?;
                if let Some(ctx) = ctx {
                    exec = exec.with_subquery(ctx);
                }
                Ok(Box::new(exec))
            }
            PhysicalPlan::Window { child, .. }
            | PhysicalPlan::Shuffle { child, .. }
            | PhysicalPlan::ShuffleReceiver { child } => {
                if matches!(plan, PhysicalPlan::Shuffle { .. } | PhysicalPlan::ShuffleReceiver { .. })
                {
                    return Self::build(child, stmt, domain, current_db, txn);
                }
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("window without select"))?
                    .clone();
                Ok(Box::new(WindowExec::new(
                    select,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            // TiDB `buildDistinct` HashAgg: EXPLAIN shape for a distinct UNION
            // group. Duplicates are eliminated by the per-arm UNION fold below.
            PhysicalPlan::DistinctUnion { child, .. } => {
                Self::build(child, stmt, domain, current_db, txn)
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
            PhysicalPlan::Apply {
                child,
                filter,
                probe: _,
                info: _,
            } => {
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
                    select.with.clone(),
                )))
            }
            PhysicalPlan::IndexJoin { outer, .. } => Self::build(
                outer,
                stmt,
                domain,
                current_db,
                txn,
            ),
            PhysicalPlan::UnionScan { child, .. } => Self::build(
                child,
                stmt,
                domain,
                current_db,
                txn,
            ),
            PhysicalPlan::CTE {
                name,
                columns,
                seed,
                inline,
                ..
            } => {
                let with = stmt
                    .with_clause()
                    .ok_or_else(|| Error::unsupported("CTE without WITH"))?;
                let first = with.seed_select(name)?;
                // TiDB: same-name nested WITH keeps recursive working-table names
                // as the seed output (not outer col_name_list) → errno 1054.
                let shadow_same_name = first
                    .with
                    .as_ref()
                    .is_some_and(|nested| nested.find(name).is_some());
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
                let cte = with.find(name);
                let recursive = with.recursive_arms(name)?;
                let limit = cte.and_then(|c| c.limit);
                let offset = cte.and_then(|c| c.offset);
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
                    shadow_same_name,
                    limit,
                    offset,
                )))
            }
            PhysicalPlan::HashJoin {
                tp,
                left,
                right,
                left_table,
                right_table,
                on,
            } => Self::build_hash_join(
                *tp,
                left,
                right,
                left_table,
                right_table,
                on,
                false,
                stmt,
                domain,
                current_db,
                txn,
            ),
            // MergeJoin executes as HashJoin; EXPLAIN already labeled MergeJoin.
            PhysicalPlan::MergeJoin {
                left,
                right,
                left_table,
                right_table,
                tp,
                on,
                ..
            } => Self::build_hash_join(
                *tp,
                left,
                right,
                left_table,
                right_table,
                on,
                true,
                stmt,
                domain,
                current_db,
                txn,
            ),
            // Row-constructor projection: EXPLAIN-only, execution passes rows
            // through (the join evaluates the operands itself).
            PhysicalPlan::RowProjection {
                exprs,
                names,
                child,
                ..
            } => Ok(Box::new(ProjectionExec::new(
                exprs.clone(),
                names.clone(),
                Self::build(child, stmt, domain, current_db, txn)?,
            ))),
            PhysicalPlan::NullAwareAntiJoin {
                left_outer,
                bool_only,
                null_aware,
                bool_column,
                keys,
                other_cond,
                probe_table,
                build_table,
                probe_width,
                probe,
                build,
                ..
            } => Ok(Box::new(NullAwareAntiJoinExec::new(
                *left_outer,
                *bool_only,
                *null_aware,
                bool_column.clone(),
                keys.clone(),
                other_cond.clone(),
                probe_table.clone(),
                build_table.clone(),
                *probe_width,
                Self::build(probe, stmt, Arc::clone(&domain), current_db, Arc::clone(&txn))?,
                Self::build(build, stmt, domain, current_db, txn)?,
            ))),
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

    #[allow(clippy::too_many_arguments)]
/// Text of every `ON` condition in the statement (flat join chain and nested
/// `FROM` items), used to decide which coalesced USING columns stay reachable.
fn select_on_text(select: &sealdb_parser::ast::SelectStmt) -> String {
    fn from_item_ons(item: &sealdb_parser::ast::FromItem, out: &mut String) {
        if let sealdb_parser::ast::FromItem::Join {
            left, right, on, ..
        } = item
        {
            from_item_ons(left, out);
            from_item_ons(right, out);
            out.push_str(&on.restore_string());
            out.push(' ');
        }
    }
    let mut out = String::new();
    for join in &select.joins {
        out.push_str(&join.on.restore_string());
        out.push(' ');
    }
    if let Some(item) = &select.from_item {
        from_item_ons(item, &mut out);
    }
    out
}

    fn build_hash_join(
        tp: JoinType,
        left: &PhysicalPlan,
        right: &PhysicalPlan,
        left_table: &str,
        right_table: &str,
        on: &ExprNode,
        straight: bool,
        stmt: &Statement,
        domain: Arc<Domain>,
        current_db: &str,
        txn: Arc<TxnHandle>,
    ) -> Result<Box<dyn Executor>> {
        let using_cols = stmt
            .as_select()
            .map(|select| {
                select
                    .joins
                    .iter()
                    .find(|j| {
                        (j.left.name.eq_ignore_ascii_case(left_table)
                            && j.right.name.eq_ignore_ascii_case(right_table))
                            || (j.left.name.eq_ignore_ascii_case(right_table)
                                && j.right.name.eq_ignore_ascii_case(left_table))
                            // Chained join: plan left_table is prior right name.
                            || (j.right.name.eq_ignore_ascii_case(right_table)
                                && !j.using_cols.is_empty())
                    })
                    .map(|j| j.using_cols.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let join = sealdb_parser::ast::Join {
            left: sealdb_parser::ast::TableName::new(None, left_table.to_string()),
            right: sealdb_parser::ast::TableName::new(None, right_table.to_string()),
            tp: ast_join_type(tp),
            on: on.clone(),
            using_cols: using_cols.clone(),
            natural: false,
            straight,
        };
        let left_exec = Self::build(
            left,
            stmt,
            Arc::clone(&domain),
            current_db,
            Arc::clone(&txn),
        )?;
        let right_exec = Self::build(right, stmt, domain, current_db, txn)?;
        // The SELECT may still reference the dropped side's USING column
        // (`SELECT t1.a, t2.a … USING(a)` / `WHERE NOT (t1.a <=> t2.a)`); keep
        // the raw values as hidden columns for filter/order evaluation.
        let mut hidden_using: Vec<String> = Vec::new();
        let has_qualified_star = stmt.as_select().is_some_and(|select| {
            select.fields.iter().any(|f| {
                matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some())
            })
        });
        if !using_cols.is_empty() {
            if let Some(select) = stmt.as_select() {
                // Restored AST text backticks identifiers (`t2`.`a`), while
                // schema labels are plain `t2.a`; compare without quoting.
                let text = select.text.to_ascii_lowercase().replace('`', "");
                let all_ons = Self::select_on_text(select)
                    .to_ascii_lowercase()
                    .replace('`', "");
                // Columns coalesced away are those of the dropped side: the
                // left subtree for RIGHT JOIN (the preserved right side owns
                // the USING column), the right table otherwise.
                let dropped_tables: Vec<String> = if matches!(tp, JoinType::RightOuter) {
                    select
                        .join_tables()
                        .iter()
                        .map(|t| t.name.clone())
                        .filter(|t| !t.eq_ignore_ascii_case(right_table))
                        .collect()
                } else {
                    vec![right_table.to_string()]
                };
                for col in &using_cols {
                    for table in &dropped_tables {
                        let qual = format!("{table}.{col}").to_ascii_lowercase();
                        // The parent SELECT or any ON in the statement may need
                        // the raw value (chained `t1 JOIN t2 USING(a) RIGHT JOIN
                        // t3 USING(a)` evaluates `t2.a = t3.a` above this join).
                        if (text.contains(&qual) || all_ons.contains(&qual))
                            && !hidden_using
                                .iter()
                                .any(|h| h.eq_ignore_ascii_case(&qual))
                        {
                            hidden_using.push(format!("{table}.{col}"));
                        }
                    }
                }
            }
        }
        Ok(Box::new(
            HashJoinV1Exec::new(join, left_exec, right_exec)
                .with_hidden_using(hidden_using)
                .with_coalesce_using(!has_qualified_star),
        ))
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
            // Join schemas may keep `alias.col` for ORDER BY/WHERE; TiDB SELECT *
            // result headers are bare (duplicates allowed). Only strip a real
            // `alias.col` suffix — expressions like `t.c1 + t.c2` or `1.23`
            // must keep their original text.
            // `\u{1}`-prefixed entries are hidden USING columns used only to
            // evaluate parent filters; they never reach the result.
            let keep: Vec<usize> = exec
                .schema()
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.starts_with('\u{1}'))
                .map(|(i, _)| i)
                .collect();
            let hidden = keep.len() != exec.schema().len();
            let columns = keep
                .iter()
                .map(|&i| {
                    let c = &exec.schema()[i];
                    let Some((prefix, name)) = c.rsplit_once('.') else {
                        return c.clone();
                    };
                    let simple = |s: &str| {
                        !s.is_empty()
                            && s.chars()
                                .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
                    };
                    let ident_start = prefix
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_alphabetic() || ch == '_' || ch == '$');
                    if ident_start && simple(prefix) && simple(name) {
                        name.to_string()
                    } else {
                        c.clone()
                    }
                })
                .collect();
            let mut rows = Vec::new();
            while let Some(chunk) = exec.next()? {
                for i in 0..chunk.num_rows() {
                    let row = localize_timestamp_row(chunk.get_row(i));
                    if hidden {
                        let mut it = row.into_iter();
                        let mut kept = Vec::with_capacity(keep.len());
                        for (idx, cell) in it.by_ref().enumerate() {
                            if keep.contains(&idx) {
                                kept.push(cell);
                            }
                        }
                        rows.push(kept);
                    } else {
                        rows.push(row);
                    }
                }
            }
            exec.close()?;
            Ok(QueryResult {
                columns,
                rows,
                warnings: sealdb_expression::take_warnings(),
            })
        })
    }
}

/// TIMESTAMP is stored UTC; convert to session TZ for result rows (TiDB display).
fn localize_timestamp_row(row: Vec<sealdb_types::Datum>) -> Vec<sealdb_types::Datum> {
    row.into_iter()
        .map(|d| match d {
            sealdb_types::Datum::Time(t)
                if matches!(t.tp, sealdb_types::TimeType::Timestamp) =>
            {
                match sealdb_expression::utc_to_session_local(t) {
                    Ok(mut local) => {
                        local.tp = sealdb_types::TimeType::Timestamp;
                        sealdb_types::Datum::Time(local)
                    }
                    Err(_) => sealdb_types::Datum::Time(t),
                }
            }
            other => other,
        })
        .collect()
}

fn stmt_order_by(stmt: &Statement) -> Option<Vec<ByItem>> {
    match stmt {
        Statement::Select(select) if !select.order_by.is_empty() => Some(select.order_by.clone()),
        Statement::SetOpr(set) if !set.order_by.is_empty() => Some(set.order_by.clone()),
        _ => None,
    }
}

/// Synthetic Sort keys for MergeJoin sides: `by_info` holds TiDB join-key
/// labels (`select.a.x`). Resolve the bare column so SortExec can evaluate it.
fn order_by_from_plan_info(by_info: &str) -> Option<Vec<ByItem>> {
    let mut out = Vec::new();
    for part in by_info.split(',').map(str::trim) {
        if part.is_empty() {
            return None;
        }
        let name = part.rsplit('.').next().unwrap_or(part).trim();
        if name.is_empty() || name.contains('#') || name.contains('(') {
            return None;
        }
        out.push(ByItem {
            expr: ExprNode::Column(sealdb_parser::ast::expressions::ColumnNameExpr {
                schema: None,
                table: None,
                name: name.to_string(),
            }),
            desc: false,
        });
    }
    if out.is_empty() { None } else { Some(out) }
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
    if let Some(found) = find_table_in_select(select, table) {
        return Ok(found);
    }
    // Decorrelated EXISTS/IN over `WITH cte AS (SELECT … FROM t2 …)` keeps a
    // TableReader for `t2` that is not the outer FROM. Never fall back to the
    // outer table — that scanned t1 on both HashJoin sides (false EXISTS hits).
    if select
        .from
        .as_ref()
        .is_some_and(|from| from.name.eq_ignore_ascii_case(table))
    {
        return Ok(select.from.clone().unwrap());
    }
    Ok(TableName::new(None, table.to_string()))
}

fn find_table_in_result_set(query: &ResultSetNode, table: &str) -> Option<TableName> {
    query
        .selects()
        .into_iter()
        .find_map(|select| find_table_in_select(select, table))
}

/// Resolve a TableReader name against FROM, JOIN, WITH CTEs, or a subquery in
/// WHERE (EXISTS / IN rewritten to HashJoin Semi still need the inner table).
fn find_table_in_select(select: &SelectStmt, table: &str) -> Option<TableName> {
    if let Some(from) = &select.from
        && (from.name.eq_ignore_ascii_case(table)
            || from
                .resolve_name()
                .eq_ignore_ascii_case(table))
    {
        return Some(from.clone());
    }
    for join in &select.joins {
        if join.left.name.eq_ignore_ascii_case(table)
            || join.left.resolve_name().eq_ignore_ascii_case(table)
        {
            return Some(join.left.clone());
        }
        if join.right.name.eq_ignore_ascii_case(table)
            || join.right.resolve_name().eq_ignore_ascii_case(table)
        {
            return Some(join.right.clone());
        }
    }
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            if let Some(found) = find_table_in_select(cte.query.as_ref(), table) {
                return Some(found);
            }
            if let Some((_, rec)) = &cte.recursive
                && let Some(found) = find_table_in_select(rec, table)
            {
                return Some(found);
            }
            for (_, arm) in &cte.set_rest {
                if let Some(found) = find_table_in_select(arm, table) {
                    return Some(found);
                }
            }
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
        ExprNode::Case(c) => c
            .operand
            .as_ref()
            .and_then(|operand| find_table_in_expr(operand, table))
            .or_else(|| {
                c.when_then.iter().find_map(|(when, then)| {
                    find_table_in_expr(when, table).or_else(|| find_table_in_expr(then, table))
                })
            })
            .or_else(|| {
                c.else_expr
                    .as_ref()
                    .and_then(|branch| find_table_in_expr(branch, table))
            }),
        ExprNode::Window(w) => w.args.iter().find_map(|arg| find_table_in_expr(arg, table)),
        ExprNode::Row(r) => r.exprs.iter().find_map(|expr| find_table_in_expr(expr, table)),
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

/// Table factor that owns an index-scan plan node: match the plan's `table`
/// (physical name or alias) against the SELECT's FROM / JOIN operands.
fn index_reader_table(
    select: &sealdb_parser::ast::SelectStmt,
    table: &str,
) -> Option<sealdb_parser::ast::TableName> {
    let matches = |t: &sealdb_parser::ast::TableName| {
        t.name.eq_ignore_ascii_case(table) || t.physical.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(table))
    };
    if let Some(from) = select.from.as_ref().filter(|t| matches(t)) {
        return Some(from.clone());
    }
    for join in &select.joins {
        if matches(&join.right) {
            return Some(join.right.clone());
        }
    }
    select
        .from
        .clone()
        .or_else(|| select.joins.first().map(|j| j.right.clone()))
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
    // TIMESTAMP index keys are UTC; session-local string ranges (esp. DST gap)
    // cannot be pushed as byte seeks — full scan + Selection filter.
    let ts_index = index.columns.first().is_some_and(|c| {
        info.columns
            .get(c.offset as usize)
            .is_some_and(|col| col.field_type.tp == sealdb_types::TYPE_TIMESTAMP)
    });
    if !ts_index {
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

/// MySQL/TiDB `MaxAliasIdentifierLen` — truncate projection field names.
const MAX_ALIAS_IDENTIFIER_LEN: usize = 256;

fn field_output_name(field: &sealdb_parser::ast::SelectField) -> String {
    let raw = if let Some(name) = &field.as_name
        && !name.is_empty()
    {
        name.clone()
    } else {
        // TiDB `buildProjectionFieldNameFromExpressions` (logical_plan_builder.go).
        tidb_projection_field_name(&field.expr, field.orig_text.as_deref())
    };
    truncate_alias_identifier(raw)
}

fn truncate_alias_identifier(name: String) -> String {
    if name.len() <= MAX_ALIAS_IDENTIFIER_LEN {
        return name;
    }
    // Byte truncate like TiDB mysql.MaxAliasIdentifierLen (ASCII SQL text).
    name.chars()
        .take(MAX_ALIAS_IDENTIFIER_LEN)
        .collect::<String>()
}

fn is_star_expr(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Value(v) if matches!(&v.datum, sealdb_types::Datum::Bytes(b) if b == b"*")
    )
}

/// True when the plan's top executor is a `Window` (optionally under shuffle).
fn is_window_plan(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::Window { .. } => true,
        PhysicalPlan::Shuffle { child, .. } | PhysicalPlan::ShuffleReceiver { child } => {
            is_window_plan(child)
        }
        _ => false,
    }
}

/// True when the plan's top executor is an aggregation (optionally under
/// shuffle), i.e. the executor already emits the SELECT-list columns.
fn is_agg_plan(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::HashAgg { .. } | PhysicalPlan::StreamAgg { .. } => true,
        PhysicalPlan::Shuffle { child, .. } | PhysicalPlan::ShuffleReceiver { child } => {
            is_agg_plan(child)
        }
        // HAVING plans a Selection between the projection and the aggregation.
        PhysicalPlan::Selection { child, .. } => is_agg_plan(child),
        _ => false,
    }
}

/// Projection exprs generated purely for EXPLAIN shape (`… ->Column#N`): no
/// executor evaluates that text, so the node must not re-evaluate SELECT fields.
fn is_explain_only_projection(exprs: &[String]) -> bool {
    !exprs.is_empty() && exprs.iter().all(|expr| expr.contains("->Column#"))
}

/// Filter that references aggregate output placeholders (`Column#5`) instead of
/// real columns — TiDB `ExplainInfo` text that has no executor binding.
fn is_explain_only_filter(filter: &ExprNode) -> bool {
    let cols = filter.referenced_columns();
    !cols.is_empty()
        && cols.iter().all(|(_, name)| {
            name.strip_prefix("Column#")
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
}

/// `SELECT g, count(*) …` above an aggregate: the projection only renames the
/// aggregate output, so re-evaluating the field expressions would call
/// `count(*)` as a scalar function. Mixed expressions (`count(*)+1`) still need
/// the regular projection path.
fn is_agg_rename_projection(select: &sealdb_parser::ast::SelectStmt) -> bool {
    !select.fields.is_empty()
        && select.fields.iter().all(|field| match &field.expr {
            ExprNode::Column(_) => true,
            ExprNode::Func(f) => crate::aggfuncs::is_agg_name(&f.name),
            _ => false,
        })
}

/// `Column#8->Column#9` style projection emitted by the window plan builder.
fn is_identity_projection(exprs: &[String]) -> bool {
    !exprs.is_empty()
        && exprs.iter().all(|expr| {
            let mut parts = expr.split("->");
            let lhs = parts.next().unwrap_or("").trim();
            let rhs = parts.next().unwrap_or("").trim();
            parts.next().is_none()
                && lhs.starts_with("Column#")
                && rhs.starts_with("Column#")
                && lhs["Column#".len()..].chars().all(|c| c.is_ascii_digit())
                && rhs["Column#".len()..].chars().all(|c| c.is_ascii_digit())
        })
}

/// Expand select-list stars against the FROM schema (TiDB ResolveFields).
fn expand_projection_fields(
    select: &sealdb_parser::ast::SelectStmt,
    from_cols: &[String],
) -> (Vec<ExprNode>, Vec<String>) {
    let mut exprs = Vec::new();
    let mut names = Vec::new();
    for field in &select.fields {
        // `t1.*` — expand only that table's columns.
        let qualified_star = match &field.expr {
            ExprNode::Column(c) if c.name == "*" => c.table.clone(),
            _ => None,
        };
        if is_star_expr(&field.expr) || qualified_star.is_some() {
            let wanted = qualified_star.map(|t| t.to_ascii_lowercase());
            for col in from_cols {
                if let Some(wanted) = &wanted {
                    let owner = col
                        .rsplit_once('.')
                        .map(|(t, _)| t.to_ascii_lowercase())
                        .unwrap_or_default();
                    if &owner != wanted {
                        continue;
                    }
                }
                // `from_cols` entries are `alias.column`: keep the qualifier on
                // the expression so `t left join tt on ... left join t ttt`
                // resolves each repeated `a` to its own input, while the output
                // label stays bare (`a	b	a	b	a	b`).
                let (table, name) = match col.rsplit_once('.') {
                    Some((table, name)) => (Some(table.to_string()), name.to_string()),
                    None => (None, col.clone()),
                };
                exprs.push(ExprNode::Column(
                    sealdb_parser::ast::expressions::ColumnNameExpr {
                        schema: None,
                        table,
                        name: name.clone(),
                    },
                ));
                names.push(name);
            }
        } else {
            let expr = match &select.from {
                Some(from) if select.joins.is_empty() => {
                    // Physical table name for TiDB overflow labels (`cte.t1.c1`).
                    qualify_expr_table(field.expr.clone(), from.resolve_name())
                }
                _ => field.expr.clone(),
            };
            exprs.push(expr);
            names.push(field_output_name(field));
        }
    }
    (exprs, names)
}

/// Attach FROM table name onto bare columns so overflow messages match TiDB
/// (`db.table.col`).
fn qualify_expr_table(expr: ExprNode, table: &str) -> ExprNode {
    match expr {
        ExprNode::Column(mut c) if c.table.is_none() => {
            c.table = Some(table.to_string());
            ExprNode::Column(c)
        }
        ExprNode::Binary(mut b) => {
            b.left = Box::new(qualify_expr_table(*b.left, table));
            b.right = Box::new(qualify_expr_table(*b.right, table));
            ExprNode::Binary(b)
        }
        ExprNode::Unary(mut u) => {
            u.expr = Box::new(qualify_expr_table(*u.expr, table));
            ExprNode::Unary(u)
        }
        ExprNode::Func(mut f) => {
            f.args = f
                .args
                .into_iter()
                .map(|a| qualify_expr_table(a, table))
                .collect();
            ExprNode::Func(f)
        }
        ExprNode::Cast(mut c) => {
            c.expr = Box::new(qualify_expr_table(*c.expr, table));
            ExprNode::Cast(c)
        }
        other => other,
    }
}

/// Column names visible to `SELECT *` from the FROM/JOIN clause.
/// `USING (c)` coalesces `c` (emit once from the left spine).
fn select_from_column_names(
    select: &sealdb_parser::ast::SelectStmt,
    domain: &Domain,
    current_db: &str,
) -> Vec<String> {
    let is = domain.infoschema();
    // `WITH` CTEs are not in the infoschema; derive their output column names
    // from the CTE body so `SELECT *` over a CTE join expands them.
    let cte_cols = |name: &str| -> Vec<String> {
        let Some(with) = select.with.as_ref() else {
            return Vec::new();
        };
        let Some(cte) = with.find(name) else {
            return Vec::new();
        };
        if !cte.col_name_list.is_empty() {
            return cte.col_name_list.clone();
        }
        let mut out = Vec::new();
        for field in &cte.query.fields {
            if is_star_expr(&field.expr) {
                out.extend(
                    select_from_column_names(&cte.query, domain, current_db)
                        .into_iter()
                        .map(|c| c.rsplit_once('.').map(|(_, n)| n.to_string()).unwrap_or(c)),
                );
            } else {
                out.push(field_output_name(field));
            }
        }
        out
    };
    let table_cols = |table: &TableName| -> Vec<String> {
        let (schema, name) = table.qualified(current_db);
        // Visible qualifier: the FROM alias when present (`t dt1` → `dt1.c1`),
        // matching the child plan's schema names.
        let alias = table.name.as_str();
        let raw: Vec<String> = match is.table_by_name(&schema, &name) {
            Some(info) => info.columns.iter().map(|c| c.name.o().to_string()).collect(),
            None => cte_cols(&name),
        };
        raw.into_iter().map(|c| format!("{alias}.{c}")).collect()
    };
    let mut cols = Vec::new();
    // `t1.*, t2.*` keeps every table's raw columns (no USING coalescing).
    let qualified_star = select.fields.iter().any(|f| {
        matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some())
    });
    if qualified_star {
        for table in select.join_tables() {
            cols.extend(table_cols(table));
        }
        return cols;
    }
    // Nested join trees follow TiDB's `mergeSchema`: USING columns first, then
    // the leading side's remaining columns, then the other side's. Aliases are
    // the visible FROM names so the expressions bind to the child schema.
    if let Some(item @ sealdb_parser::ast::FromItem::Join { .. }) = &select.from_item
        && let Some(merged) =
            sealdb_planner::merged_star_visible_columns(item, Some(&is), current_db)
        && !merged.is_empty()
    {
        return merged
            .into_iter()
            .map(|(alias, col)| format!("{alias}.{col}"))
            .collect();
    }
    // Join trees whose sides are not all in the infoschema (CTE arms, derived
    // tables): fall back to FROM-order concatenation.
    if select.from_item.is_some() {
        for table in select.join_tables() {
            cols.extend(table_cols(table));
        }
        return cols;
    }
    if let Some(from) = &select.from {
        cols = table_cols(from);
    }
    for join in &select.joins {
        let right_cols = table_cols(&join.right);
        if join.using_cols.is_empty() {
            cols.extend(right_cols);
        } else {
            let drop: std::collections::HashSet<String> = join
                .using_cols
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            cols.extend(right_cols.into_iter().filter(|c| {
                let bare = c.rsplit_once('.').map(|(_, n)| n).unwrap_or(c);
                !drop.contains(&bare.to_ascii_lowercase())
            }));
        }
    }
    // RIGHT JOIN preserves its right side, so TiDB's `SELECT *` starts with
    // that table's columns (`t1(a,c) RIGHT JOIN t2(a,d) USING(a)` → a, d, c).
    if let Some(last) = select.joins.last()
        && matches!(last.tp, sealdb_parser::ast::JoinType::RightOuter)
        && !last.using_cols.is_empty()
    {
        // Only the non-preserved side of each USING join loses the column; a
        // third table that shares the name keeps its own (`t1, t2 NATURAL
        // RIGHT JOIN t3` → `t1.i, t3.i`).
        let drop =
            sealdb_planner::using_dropped_columns(select, Some(&is), current_db);
        let mut reordered = table_cols(&last.right);
        for table in select.join_tables() {
            if table.name == last.right.name {
                continue;
            }
            let table_name = table.name.to_ascii_lowercase();
            reordered.extend(table_cols(table).into_iter().filter(|c| {
                let bare = c.rsplit_once('.').map(|(_, n)| n).unwrap_or(c);
                !drop.contains(&(table_name.clone(), bare.to_ascii_lowercase()))
            }));
        }
        if !reordered.is_empty() {
            cols = reordered;
        }
    }
    cols
}

/// TiDB projection naming: unwrap unary `+` / parens, then literal rules or `field.Text()`.
fn tidb_projection_field_name(expr: &ExprNode, orig_text: Option<&str>) -> String {
    let inner = unwrap_paren_and_unary_plus(expr);
    match inner {
        ExprNode::Value(v) => match &v.datum {
            sealdb_types::Datum::Enum(e) => {
                // ENUM/SET literal: TiDB uses field.Text() (the label).
                let s = String::from_utf8_lossy(&e.label);
                s.trim_start_matches(|c: char| {
                    !(c.is_ascii_alphanumeric()
                        || (c as u32 > 0x20 && c as u32 <= 0x7e && !c.is_ascii_whitespace()))
                })
                .to_string()
            }
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
            // TIMESTAMP/DATE/TIME literals: TiDB uses field.Text().
            sealdb_types::Datum::Time(t) => {
                let raw = orig_text.unwrap_or("").trim();
                if raw.is_empty() {
                    t.to_string()
                } else {
                    raw.to_string()
                }
            }
        },
        ExprNode::Column(_) => tidb_expr_display_name(inner),
        _ => {
            // Non-literal: TiDB uses field.Text().
            if let Some(text) = orig_text {
                strip_inner_comment_terminators(text.trim())
            } else {
                tidb_expr_display_name(expr)
            }
        }
    }
}

/// TiDB `SelectField.Text()` keeps a comment that ends the text (`1 /* c */`)
/// but drops the `*/` terminator of a comment followed by more tokens:
/// `(1 /* c */ + 2)` renders as `(1 /* c  + 2)`.
fn strip_inner_comment_terminators(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("*/") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 2..];
        let has_open = rest[..idx].rfind("/*").is_some();
        let more_after = !after
            .trim_end_matches(|c: char| c.is_whitespace() || c == ')')
            .is_empty();
        if !(has_open && more_after) {
            out.push_str("*/");
        }
        rest = after;
    }
    out.push_str(rest);
    out
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
            sealdb_types::Datum::Time(t) => t.to_string(),
            sealdb_types::Datum::Decimal(d) => d.to_string(),
            sealdb_types::Datum::Enum(e) => String::from_utf8_lossy(&e.label).into_owned(),
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

/// `Apply`-style context for scalar subqueries in SELECT list / GROUP BY /
/// HAVING, where the aggregate executor evaluates expressions per input row.
fn agg_subquery_ctx(
    select: &sealdb_parser::ast::SelectStmt,
    domain: Arc<Domain>,
    current_db: &str,
    txn: Arc<TxnHandle>,
) -> Option<SubqueryEvalCtx> {
    let needs = select.fields.iter().any(|f| f.expr.has_subquery())
        || select.group_by.iter().any(|g| g.has_subquery())
        || select.having.as_ref().is_some_and(|h| h.has_subquery());
    if !needs {
        return None;
    }
    Some(SubqueryEvalCtx::new(
        domain,
        current_db.to_string(),
        txn,
        select.from.as_ref().map(|table| table.name.clone()),
    ))
}

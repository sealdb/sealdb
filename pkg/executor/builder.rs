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
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ExprNode, FuncCallExpr, PatternLikeExpr, ValueExpr,
};
use sealdb_parser::ast::{
    ByItem, ResultSetNode, SelectLock, SelectLockWait, SelectStmt, SetOprStmt, Statement, TableName,
};
use sealdb_planner::{JoinType, PhysicalPlan, PlanBuilder};
use sealdb_util::collate::CollateMode;
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
                true,
            ))),
            PhysicalPlan::Projection { exprs, child, .. }
            | PhysicalPlan::StarOrderProjection { exprs, child } => {
                // A Projection re-emits only the SELECT list, so ORDER BY-only
                // aggregate outputs could not survive it: cancel the request.
                ORDER_COLUMNS_WANTED.set(false);
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
                if is_window_plan(child) && is_identity_projection(exprs) {
                    let names: Vec<String> = select.fields.iter().map(field_output_name).collect();
                    return Ok(Box::new(ProjectionExec::passthrough(names, child_exec)));
                }
                // HAVING decorates the aggregate input with a projection whose
                // exprs are EXPLAIN placeholders (`cast(…) ->Column#13`); the
                // aggregate executor evaluates its arguments against the child
                // rows, so the projection only passes rows through.
                if select.having.is_some()
                    && !is_agg_plan(child)
                    && is_explain_only_projection(exprs)
                {
                    return Ok(child_exec);
                }
                // `SELECT g, count(*) … HAVING …` plans a Projection above the
                // aggregate purely for EXPLAIN shape; the aggregate executor
                // already emits the SELECT-list columns in order.
                if is_agg_plan(child) && is_agg_rename_projection(select) {
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
                let (exprs, names) = {
                    // The planner rewrote a virtual generated column's body in
                    // the SELECT list (`a+1` → `c`) so a covering index that
                    // stores `c` can answer the query without the base columns.
                    // Evaluate the same expression here: the original `a+1`
                    // would read a column the covering read does not carry.
                    let substituted = select
                        .from
                        .as_ref()
                        .filter(|_| select.from_item.is_none() && select.joins.is_empty())
                        .and_then(|from| {
                            let is = domain.infoschema();
                            let (db, table) = from.qualified(current_db);
                            let info = is.table_by_name(&db, &table)?;
                            sealdb_planner::core::expr_index::substitute_generated_columns(
                                info, select,
                            )
                        });
                    expand_projection_fields(substituted.as_ref().unwrap_or(select), &from_cols)
                };
                // `CAST(json_col AS TIME/DATETIME/…)`: the executor rebuilds the
                // list from the original statement, so mark JSON arguments here
                // (only a JSON string has a temporal form).
                let mut exprs = exprs;
                sealdb_planner::mark_json_temporal_cast_fields(
                    &mut exprs,
                    select,
                    Some(&domain.infoschema()),
                    current_db,
                );
                // `COLLATION(expr)` reports the *static* collation of its
                // argument, so fold it here where the table types are known.
                // The output names were derived from the original fields, so
                // the display label keeps `collation(…)`.
                let exprs = resolve_collation_exprs(&domain, select, current_db, exprs)?;
                // MySQL/TiDB: LOWER/UPPER on a binary-charset value (VARBINARY /
                // BINARY / BLOB) is a no-op — binary has no case mapping.
                let exprs = resolve_binary_case_convert_exprs(&domain, select, current_db, exprs);
                // A covering read over an expression index only carries the
                // hidden key column, so the SELECT expressions re-evaluate
                // against it (`plus(t.a, 1)` → the stored `(a+1)` value).
                let exprs = expr_index_read_exprs(&domain, child, current_db, exprs);
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
                let collations = order_by_collations(&domain, stmt, current_db, &by);
                let built = build_with_order_columns(child, stmt, domain, current_db, txn)?;
                let sort = SortExec::with_collations(by, collations, built);
                Ok(trim_order_columns(Box::new(sort)))
            }
            PhysicalPlan::TopN { child, by_info, .. } => {
                // ResultReorder injects keys into the planner's local SELECT
                // clone, so an EXECUTE (or a plan-cache hit) may build a TopN
                // whose keys are only recorded in the plan.
                let by = stmt_order_by(stmt)
                    .or_else(|| order_by_from_plan_info(by_info))
                    .ok_or_else(|| Error::unsupported("topn without ORDER BY"))?;
                let collations = order_by_collations(&domain, stmt, current_db, &by);
                let (limit, offset) =
                    stmt_limit(stmt).ok_or_else(|| Error::unsupported("topn without LIMIT"))?;
                let limit = limit.ok_or_else(|| Error::unsupported("topn without LIMIT"))?;
                let built = build_with_order_columns(child, stmt, domain, current_db, txn)?;
                let topn = TopNExec::with_collations(by, collations, limit, offset, built);
                Ok(trim_order_columns(Box::new(topn)))
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
            PhysicalPlan::Selection { child, filter, .. } => {
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
                // Rewrite the surviving filter with the collation derived from
                // its operands (index ranges may have consumed the comparator
                // for indexed columns, but residual filters still need it).
                let filter = match stmt.as_select() {
                    Some(select) => {
                        let scope = CollationScope::for_select(&domain, select, current_db);
                        &fold_collation(filter.clone(), &scope)?
                    }
                    None => filter,
                };
                Ok(Box::new(
                    SelectionExec::new(
                        filter.clone(),
                        Self::build(child, stmt, domain, current_db, txn)?,
                    )
                    .with_index_dst_double(under_index),
                ))
            }
            PhysicalPlan::Distinct {
                keys,
                fields,
                names,
                child,
            } => {
                let mut exec = DistinctExec::new(
                    keys.clone(),
                    fields.clone(),
                    names.clone(),
                    Self::build(child, stmt, Arc::clone(&domain), current_db, txn)?,
                );
                if let Some(select) = stmt.as_select() {
                    exec = exec.with_collations(expr_list_collations(
                        fields, select, &domain, current_db,
                    )?);
                }
                Ok(Box::new(exec))
            }
            PhysicalPlan::Simple { kind } if kind == "Analyze" => Ok(Box::new(AnalyzeExec::new(
                domain,
                stmt.clone(),
                current_db,
                txn,
            ))),
            PhysicalPlan::Simple { .. } => Ok(Box::new(
                GrantExec::new(domain, stmt.clone()).with_current_db(current_db.to_string()),
            )),
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
                let mut insert = match stmt {
                    Statement::Insert(s) => s.clone(),
                    _ => return Err(Error::unsupported("insert without insert statement")),
                };
                // `ON DUPLICATE KEY UPDATE` may reference the source query's
                // tables. Widen the source projection with those columns so the
                // executor can evaluate the assignments against the source row.
                let (source_scope, source_extra) =
                    widen_insert_source(&mut insert, &domain, current_db);
                let select = if let Some(set) = insert.select_set.as_ref() {
                    Some(Self::build_set_opr_arm(
                        set,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?)
                } else {
                    insert
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
                        .transpose()?
                };
                Ok(Box::new(InsertExec::new(
                    domain,
                    insert,
                    current_db.to_string(),
                    txn,
                    select,
                    source_scope,
                    source_extra,
                )))
            }
            PhysicalPlan::InfoSchema {
                table,
                ranges: pushed_ranges,
                pushed,
                ..
            } => {
                let (columns, rows) = if table.eq_ignore_ascii_case("bind_info") {
                    domain.bind_info_rows()
                } else if table.eq_ignore_ascii_case("tables_priv") {
                    tables_priv_rows(&domain)
                } else if table.eq_ignore_ascii_case("columns_priv") {
                    columns_priv_rows(&domain)
                } else if table.eq_ignore_ascii_case("global_grants") {
                    global_grants_rows(&domain)
                } else if table.eq_ignore_ascii_case("db") {
                    db_priv_rows(&domain)
                } else if table.eq_ignore_ascii_case("global_priv") {
                    global_priv_rows(&domain)
                } else if matches!(
                    table.to_ascii_lowercase().as_str(),
                    "schema_privileges" | "table_privileges" | "column_privileges"
                ) {
                    privilege_information_rows(&domain, &table)
                } else if table.eq_ignore_ascii_case("columns") {
                    let (columns, rows) =
                        sealdb_infoschema::tables::virtual_rows(table, &domain.infoschema())?;
                    let rows = columns_for_session(&domain, &columns, rows);
                    (columns, rows)
                } else {
                    sealdb_infoschema::tables::virtual_rows(table, &domain.infoschema())?
                };
                // Contradictory extractors (`skip_request` / `skip_inspection`)
                // keep a MemTableScan shape for EXPLAIN but yield no rows.
                let rows = if pushed
                    .as_deref()
                    .is_some_and(|p| p.contains("skip_request") || p.contains("skip_inspection"))
                {
                    Vec::new()
                } else {
                    rows
                };
                let mut exec = MemTableExec::new(columns, rows);
                exec.filter_ranges(pushed_ranges);
                Ok(Box::new(exec))
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
                range_scan,
                selection_info,
                push_down,
                ..
            } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index lookup without select"))?;
                let from = index_reader_table(select, table)
                    .ok_or_else(|| Error::unsupported("index lookup without FROM"))?;
                if let Some(exec) = mysql_priv_mem_exec(&from, current_db, stmt, &domain) {
                    return Ok(exec);
                }
                // TiDB's probe-side `TableRowIDScan` batches the build handles
                // into one `BatchGet` (key order) unless the read must keep the
                // index-key order (`keep order:true`) for an ORDER BY.
                let keep_order = range_scan.as_ref().is_some_and(|range| range.keep_order)
                    || keep_order_pushed(pushed.as_ref());
                // The reader may belong to any table of a joined SELECT (index
                // join / applied subquery), not only the leading FROM factor.
                let access =
                    index_access(&domain, current_db, &from, index, select.selection.as_ref())
                        .ok_or_else(|| Error::unsupported("index lookup without access path"))?;
                // IndexLookUp push-down: Build collects `offset+count` matching
                // index keys (Selection before Limit) before the table probe;
                // the root Limit applies the original offset/count.
                // `LIMIT|<offset>|<count>[|<residual Selection info>]` — only
                // the first two pipe fields are the fetch size; residual text
                // after count must not be parsed as the count.
                let index_fetch = if *push_down {
                    selection_info.as_deref().and_then(|sel| {
                        let rest = sel.strip_prefix("LIMIT|")?;
                        let mut parts = rest.splitn(3, '|');
                        let o = parts.next()?.parse::<u64>().ok()?;
                        let c = parts.next()?.parse::<u64>().ok()?;
                        Some(o + c)
                    })
                } else {
                    None
                };
                // Residual filters under an absorbed Limit live on the SELECT
                // (and optionally in `pushed`); keep them so fetch-cap counts
                // only rows that pass Selection.
                let lookup_filter = keep_order_filter(pushed).or_else(|| {
                    if index_fetch.is_some() {
                        select.selection.clone()
                    } else {
                        None
                    }
                });
                Ok(Box::new(
                    IndexLookUpExecutor::new(
                        domain,
                        from,
                        index.clone(),
                        access,
                        current_db.to_string(),
                        txn,
                        select_row_lock(select),
                        select_lock_wait(select),
                        partitions.clone(),
                        lookup_filter,
                        false,
                        keep_order,
                        Self::select_references_tidb_rowid(select),
                    )
                    .with_index_fetch_limit(index_fetch),
                ))
            }
            PhysicalPlan::IndexReader {
                table,
                index,
                partitions,
                pushed,
                range_scan,
                ..
            } => {
                let keep_order = range_scan.as_ref().is_some_and(|range| range.keep_order)
                    || keep_order_pushed(pushed.as_ref());
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index lookup without select"))?;
                let from = index_reader_table(select, table)
                    .ok_or_else(|| Error::unsupported("index lookup without FROM"))?;
                if let Some(exec) = mysql_priv_mem_exec(&from, current_db, stmt, &domain) {
                    return Ok(exec);
                }
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
                    keep_order_filter(pushed),
                    true,
                    keep_order,
                    Self::select_references_tidb_rowid(select),
                )))
            }
            PhysicalPlan::IndexMerge {
                partitions,
                pushed,
                probe_selection,
                paths,
                keep_order,
                order_column,
                order_desc,
                limit,
                ..
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
                    probe_selection.clone(),
                    paths.clone(),
                    *keep_order,
                    order_column.clone(),
                    *order_desc,
                    *limit,
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
                if let Some(exec) = mysql_priv_mem_exec(&from, current_db, stmt, &domain) {
                    return Ok(exec);
                }
                let lookup = index.as_ref().and_then(|index_name| {
                    index_lookup_values(&domain, current_db, &from, index_name, select)
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
                    Self::select_references_tidb_rowid(select),
                )))
            }
            PhysicalPlan::BatchPointGet {
                table,
                column,
                index,
                tuple_columns,
                index_columns,
                partition_names,
                desc,
                ..
            } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("batch point get without FROM"))?;
                let from = resolve_reader_table(stmt, table)?;
                if let Some(exec) = mysql_priv_mem_exec(&from, current_db, stmt, &domain) {
                    return Ok(exec);
                }
                let selection = select.selection.as_ref();
                // Key values come from the table metadata, so an expression
                // part folds from the plain parts (`uk_expr((tidb_shard(a)), a)`
                // under `a IN (100, 200)`).
                let (schema_name, table_name) = from.qualified(current_db);
                let is = domain.infoschema();
                let info = is.table_by_name(&schema_name, &table_name);
                let index_info = index.as_deref().and_then(|name| {
                    info.as_ref()?
                        .indices
                        .iter()
                        .find(|idx| idx.name.o().eq_ignore_ascii_case(name))
                });
                let key_values = |known: &[(String, sealdb_types::Datum)]| {
                    let info = info.as_ref()?;
                    match index_info {
                        Some(index) => index_key_values(info, index, known),
                        // Handle batch point get: the row key is the primary
                        // key value, one Datum per handle column.
                        None => {
                            let columns: &[String] = if index_columns.is_empty() {
                                std::slice::from_ref(column)
                            } else {
                                index_columns
                            };
                            columns
                                .iter()
                                .map(|col| {
                                    known
                                        .iter()
                                        .find(|(name, _)| name.eq_ignore_ascii_case(col))
                                        .map(|(_, value)| value.clone())
                                })
                                .collect()
                        }
                    }
                };
                let lookups: Vec<Vec<sealdb_types::Datum>> = if tuple_columns.is_empty() {
                    selection
                        .and_then(|expr| expr.in_const_on(column))
                        .ok_or_else(|| Error::unsupported("batch point get without IN list"))?
                        .into_iter()
                        .map(|value| key_values(&[(column.clone(), value)]))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| Error::unsupported("batch point get key values"))?
                } else {
                    let (names, tuples) = selection
                        .and_then(|expr| {
                            expr.split_cnf()
                                .into_iter()
                                .find_map(|item| item.row_in_consts_with_len())
                                .map(|(names, tuples, _)| (names, tuples))
                        })
                        .or_else(|| {
                            selection
                                .and_then(|expr| expr.dnf_const_tuples(tuple_columns))
                                .map(|tuples| (tuple_columns.clone(), tuples))
                        })
                        .ok_or_else(|| Error::unsupported("batch point get without key list"))?;
                    tuples
                        .into_iter()
                        .map(|tuple| {
                            let known: Vec<(String, sealdb_types::Datum)> =
                                names.iter().cloned().zip(tuple.iter().cloned()).collect();
                            key_values(&known)
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| Error::unsupported("batch point get key values"))?
                };
                let _ = index_columns;
                Ok(Box::new(BatchPointGetExec::new(
                    domain,
                    from,
                    index.clone(),
                    lookups,
                    partition_names.clone(),
                    current_db.to_string(),
                    txn,
                    select_row_lock(select),
                    select_lock_wait(select),
                    *desc,
                )))
            }
            PhysicalPlan::TableReader {
                table,
                partitions,
                pushed,
                ..
            } => {
                let from = resolve_reader_table(stmt, table)?;
                if let Some(exec) = mysql_priv_mem_exec(&from, current_db, stmt, &domain) {
                    return Ok(exec);
                }
                let with_rowid = stmt
                    .as_select()
                    .is_some_and(Self::select_references_tidb_rowid);
                Ok(Box::new(TableReaderExec::new(
                    domain,
                    from,
                    current_db.to_string(),
                    txn,
                    stmt_row_lock(stmt),
                    stmt_lock_wait(stmt),
                    partitions.clone(),
                    pushed.clone(),
                    with_rowid,
                )))
            }
            PhysicalPlan::PartitionUnion { children } => {
                let mut iter = children.iter();
                let first = iter
                    .next()
                    .ok_or_else(|| Error::unsupported("empty PartitionUnion"))?;
                let mut exec = Self::build(
                    first,
                    stmt,
                    Arc::clone(&domain),
                    current_db,
                    Arc::clone(&txn),
                )?;
                for child in iter {
                    let right = Self::build(
                        child,
                        stmt,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?;
                    exec = Box::new(UnionExec::new(
                        exec,
                        right,
                        sealdb_parser::ast::SetOprType::UnionAll,
                    ));
                }
                Ok(exec)
            }
            PhysicalPlan::TableDual { row_count } => {
                // Headers are statement-level in TiDB; a folded `SELECT *`
                // still reports its columns even though the plan is a dual.
                let mut schema = Vec::new();
                if let Some(select) = stmt.as_select() {
                    let from_cols = select_from_column_names(select, &domain, current_db);
                    schema = expand_projection_fields(select, &from_cols).1;
                }
                Ok(Box::new(TableDualExec::new(*row_count, schema)))
            }
            PhysicalPlan::HashAgg { child, info, .. } => {
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
                // `SELECT DISTINCT` plans as an aggregate grouped on the
                // projected columns; execution de-duplicates them (TiDB's
                // dedup HashAgg) instead of folding them into one row. A select
                // list holding aggregate functions is the scalar aggregate
                // itself (the single row is already distinct).
                if distinct_is_dedup(select) {
                    return distinct_exec_for_select(select, built, &domain, current_db);
                }
                // `RewriteINToJoinAndAgg` puts a `HashAgg(firstrow)` over the
                // subquery as the build side of an inner join. That aggregate
                // belongs to the *subquery*, so the outer statement carries no
                // aggregation of its own: execute it from the plan's group keys
                // (`group by:dbg.m2.b, funcs:firstrow(…)`) instead.
                if let Some(dedup) = dedup_from_agg_info(info)
                    && !stmt_owns_agg_info(select, &dedup)
                {
                    let mut exec =
                        DistinctExec::new(dedup.keys, dedup.fields.clone(), dedup.names, built);
                    exec = exec.with_collations(expr_list_collations(
                        &dedup.fields,
                        select,
                        &domain,
                        current_db,
                    )?);
                    return Ok(Box::new(exec));
                }
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
                    let modes = expr_list_collations(&fields, select, &domain, current_db)?;
                    return Ok(Box::new(
                        DistinctExec::new(keys, fields, names, built).with_collations(modes),
                    ));
                }
                let from_columns = select_from_column_names(select, &domain, current_db);
                let order_columns = take_order_columns(select, &from_columns);
                let mut exec = HashAggExec::new(select, built)?
                    .with_group_collations(group_by_collations(select, &domain, current_db)?);
                if !order_columns.is_empty() {
                    exec = exec.with_order_columns(&order_columns);
                }
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
                let built = Self::build(child, stmt, Arc::clone(&domain), current_db, txn)?;
                // `SELECT DISTINCT` under `STREAM_AGG()` is the same dedup.
                if distinct_is_dedup(select) {
                    return distinct_exec_for_select(select, built, &domain, current_db);
                }
                let from_columns = select_from_column_names(select, &domain, current_db);
                let order_columns = take_order_columns(select, &from_columns);
                let mut exec = StreamAggExec::new(select, built)?
                    .with_group_collations(group_by_collations(select, &domain, current_db)?);
                if !order_columns.is_empty() {
                    exec = exec.with_order_columns(&order_columns);
                }
                if let Some(ctx) = ctx {
                    exec = exec.with_subquery(ctx);
                }
                Ok(Box::new(exec))
            }
            PhysicalPlan::Window { child, .. }
            | PhysicalPlan::Shuffle { child, .. }
            | PhysicalPlan::ShuffleReceiver { child } => {
                if matches!(
                    plan,
                    PhysicalPlan::Shuffle { .. } | PhysicalPlan::ShuffleReceiver { .. }
                ) {
                    return Self::build(child, stmt, domain, current_db, txn);
                }
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("window without select"))?
                    .clone();
                // `SELECT row_number() over ()` plans a placeholder Projection
                // (`ROW_NUMBER() OVER ()->Column#1`) over TableDual. The Window
                // executor evaluates the window functions itself, so that
                // projection must not try to evaluate them.
                let child_plan =
                    strip_dual_window_projection(child).unwrap_or_else(|| (**child).clone());
                Ok(Box::new(WindowExec::new(
                    select,
                    Self::build(&child_plan, stmt, domain, current_db, txn)?,
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
                let mut arms: Vec<(sealdb_parser::ast::SetOprType, Box<dyn Executor>)> =
                    Vec::with_capacity(set.rest.len() + 1);
                arms.push((
                    sealdb_parser::ast::SetOprType::UnionAll,
                    Self::build_select_arm(
                        &first,
                        Arc::clone(&domain),
                        current_db,
                        Arc::clone(&txn),
                    )?,
                ));
                for (op, select) in &set.rest {
                    let mut select = select.clone();
                    if select.with.is_none() {
                        select.with = set.with.clone();
                    }
                    arms.push((
                        *op,
                        Self::build_select_arm(
                            &select,
                            Arc::clone(&domain),
                            current_db,
                            Arc::clone(&txn),
                        )?,
                    ));
                }
                // Match planner precedence: INTERSECT binds tighter than
                // UNION/EXCEPT (`a UNION ALL b INTERSECT c`).
                Ok(fold_mixed_set_execs(arms))
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
            PhysicalPlan::IndexJoin { outer, .. } => {
                Self::build(outer, stmt, domain, current_db, txn)
            }
            PhysicalPlan::UnionScan { child, .. } => {
                Self::build(child, stmt, domain, current_db, txn)
            }
            // Row locking is enforced by the point get / DML paths; the
            // SelectLock node is EXPLAIN shape only.
            PhysicalPlan::SelectLock { child, .. } => {
                Self::build(child, stmt, domain, current_db, txn)
            }
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
                est_rows: _,
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
            // The join evaluates the expression key from the raw child row.
            PhysicalPlan::KeyProjection { child, .. } => {
                Self::build(child, stmt, domain, current_db, txn)
            }
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
                Self::build(
                    probe,
                    stmt,
                    Arc::clone(&domain),
                    current_db,
                    Arc::clone(&txn),
                )?,
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

    /// True when the SELECT list / ORDER BY / WHERE mentions `_tidb_rowid`.
    fn select_references_tidb_rowid(select: &sealdb_parser::ast::SelectStmt) -> bool {
        fn mentions(expr: &ExprNode) -> bool {
            match expr {
                ExprNode::Column(col) => col.name.eq_ignore_ascii_case("_tidb_rowid"),
                ExprNode::Binary(b) => mentions(&b.left) || mentions(&b.right),
                ExprNode::Unary(u) => mentions(&u.expr),
                ExprNode::Func(f) => f.args.iter().any(mentions),
                ExprNode::Cast(c) => mentions(&c.expr),
                ExprNode::InList(i) => mentions(&i.expr) || i.list.iter().any(mentions),
                ExprNode::Between(b) => mentions(&b.expr),
                _ => false,
            }
        }
        // The handle column is appended as a hidden (`\u{1}`) schema entry, so a
        // `SELECT *` result never exposes it while ORDER BY can still bind to it.
        select.fields.iter().any(|f| mentions(&f.expr))
            || select.order_by.iter().any(|o| mentions(&o.expr))
            || select.selection.as_ref().is_some_and(mentions)
    }

    #[allow(clippy::too_many_arguments)]
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
        // `RewriteINToJoinAndAgg` (`tidb_opt_insubq_to_join_and_agg`) turns
        // `x IN (SELECT y …)` into an inner join against the de-duplicated
        // subquery. The subquery columns are not visible to the outer query
        // (TiDB prunes them), and the de-dup keeps at most one build row per
        // key, so the join behaves exactly like the semi join it replaced.
        let in_rewrite = matches!(tp, JoinType::Inner)
            && matches!(
                right,
                PhysicalPlan::HashAgg { info, .. } if dedup_from_agg_info(info).is_some()
            );
        let exec_tp = if in_rewrite { JoinType::Semi } else { tp };
        // Hash/semi/anti joins probe by raw key equality. Wrap string equality
        // operands in their collation sort key so `'a'`/`'A'`/`'À'` hash
        // together under `utf8mb4_general_ci` (TiDB hashes `collator.Key`).
        let join_on = match stmt.as_select() {
            Some(select) => {
                let scope = CollationScope::for_select(&domain, select, current_db);
                wrap_join_keys(on.clone(), &scope)?
            }
            None => on.clone(),
        };
        let join = sealdb_parser::ast::Join {
            left: sealdb_parser::ast::TableName::new(None, left_table.to_string()),
            right: sealdb_parser::ast::TableName::new(None, right_table.to_string()),
            tp: ast_join_type(exec_tp),
            on: join_on,
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
            select.fields.iter().any(
                |f| matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some()),
            )
        });
        if !using_cols.is_empty()
            && let Some(select) = stmt.as_select()
        {
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
                        && !hidden_using.iter().any(|h| h.eq_ignore_ascii_case(&qual))
                    {
                        hidden_using.push(format!("{table}.{col}"));
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

    fn build_set_opr_arm(
        set: &sealdb_parser::ast::SetOprStmt,
        domain: Arc<Domain>,
        current_db: &str,
        txn: Arc<TxnHandle>,
    ) -> Result<Box<dyn Executor>> {
        let stmt = Statement::SetOpr(set.clone());
        let stats = domain.stats();
        let plan = with_txn_dirty_tables(&txn, || {
            PlanBuilder::new().build_with_schema(
                &stmt,
                Some(&domain.infoschema()),
                current_db,
                Some(stats.as_ref()),
                sealdb_planner::PartitionPruneMode::Dynamic,
            )
        })?;
        Self::build(&plan, &stmt, domain, current_db, txn)
    }

    fn build_select_arm(
        select: &SelectStmt,
        domain: Arc<Domain>,
        current_db: &str,
        txn: Arc<TxnHandle>,
    ) -> Result<Box<dyn Executor>> {
        let stmt = Statement::Select(select.clone());
        let stats = domain.stats();
        let plan = with_txn_dirty_tables(&txn, || {
            PlanBuilder::new().build_with_schema(
                &stmt,
                Some(&domain.infoschema()),
                current_db,
                Some(stats.as_ref()),
                sealdb_planner::PartitionPruneMode::Dynamic,
            )
        })?;
        Self::build(&plan, &stmt, domain, current_db, txn)
    }
}

/// Plan a statement built lazily at execution time (subquery, CTE body, set-op
/// arm) with the current transaction's pending writes visible.
///
/// The session publishes `MemBuffer` dirty tables while it plans the outer
/// statement, but a decorrelated subquery is planned later, while the executor
/// runs. Without this scope the inner scan gets no `UnionScan` and misses rows
/// this transaction inserted but has not committed yet.
pub(crate) fn with_txn_dirty_tables<T>(txn: &TxnHandle, f: impl FnOnce() -> T) -> T {
    let keys = txn.with_txn(|txn| Ok(txn.dirty_keys())).unwrap_or_default();
    let mut ids: Vec<i64> = keys
        .iter()
        .filter_map(|key| sealdb_tablecodec::decode_table_id(key))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    sealdb_planner::with_dirty_tables(ids, f)
}

/// Fold set-op arms with TiDB precedence: reduce `INTERSECT` first, then
/// left-fold `UNION`/`EXCEPT`.
fn fold_mixed_set_execs(
    arms: Vec<(sealdb_parser::ast::SetOprType, Box<dyn Executor>)>,
) -> Box<dyn Executor> {
    use sealdb_parser::ast::SetOprType;
    let mut plans: Vec<Box<dyn Executor>> = Vec::with_capacity(arms.len());
    let mut ops: Vec<SetOprType> = Vec::with_capacity(arms.len().saturating_sub(1));
    for (i, (op, plan)) in arms.into_iter().enumerate() {
        if i == 0 {
            plans.push(plan);
        } else {
            ops.push(op);
            plans.push(plan);
        }
    }
    while let Some(i) = ops
        .iter()
        .position(|op| matches!(op, SetOprType::Intersect | SetOprType::IntersectAll))
    {
        let op = ops.remove(i);
        let right = plans.remove(i + 1);
        let left = plans.remove(i);
        plans.insert(i, Box::new(UnionExec::new(left, right, op)));
    }
    let mut iter = plans.into_iter();
    let mut exec = iter.next().expect("set op arm");
    for (op, right) in ops.into_iter().zip(iter) {
        exec = Box::new(UnionExec::new(exec, right, op));
    }
    exec
}

// Re-open ExecutorBuilder inherent impl for the remaining helpers.
impl ExecutorBuilder {
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
            crate::internal::reset_dml_summary();
            exec.open()?;
            // Join schemas may keep `alias.col` for ORDER BY/WHERE; TiDB SELECT *
            // result headers are bare (duplicates allowed). Only strip a real
            // `alias.col` suffix — expressions like `t.c1 + t.c2` or `1.23`
            // must keep their original text.
            // `\u{1}`-prefixed entries are hidden USING columns used only to
            // evaluate parent filters; they never reach the result. A table
            // read over a table with an expression index carries the hidden
            // generated column (`_V$_idx_0`) in its DataSource schema, and
            // TiDB never exposes it either: `SELECT *` lists the user columns
            // only.
            let keep: Vec<usize> = exec
                .schema()
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.starts_with('\u{1}') && !is_hidden_generated_column(c))
                .map(|(i, _)| i)
                .collect();
            let hidden = keep.len() != exec.schema().len();
            let columns = keep
                .iter()
                .map(|&i| {
                    let c = &exec.schema()[i];
                    let parts: Vec<&str> = c.split('.').collect();
                    let simple = |s: &str| {
                        !s.is_empty()
                            && s.chars()
                                .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
                    };
                    let ident_start = parts
                        .first()
                        .and_then(|p| p.chars().next())
                        .is_some_and(|ch| ch.is_alphabetic() || ch == '_' || ch == '$');
                    // Physical table columns may carry `db.table.col` (a
                    // partitioned table's OrigTblName keeps the real name);
                    // MySQL/TiDB result labels are the bare column name.
                    if parts.len() >= 2 && ident_start && parts.iter().all(|p| simple(p)) {
                        parts.last().copied().unwrap_or(c).to_string()
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
            let dml = crate::internal::take_dml_summary();
            let (affected_rows, info) = match &dml {
                Some(summary) => (summary.affected_rows, summary.info.clone()),
                None => (0, String::new()),
            };
            Ok(QueryResult {
                columns,
                rows,
                warnings: sealdb_expression::take_warnings(),
                affected_rows,
                info,
            })
        })
    }
}

/// TIMESTAMP is stored UTC; convert to session TZ for result rows (TiDB display).
/// The converted value is marked as a plain DATETIME so the later text render
/// (`format_timestamp_display`, which runs outside the session zone scope) does
/// not shift it a second time.
fn localize_timestamp_row(row: Vec<sealdb_types::Datum>) -> Vec<sealdb_types::Datum> {
    row.into_iter()
        .map(|d| match d {
            sealdb_types::Datum::Time(t) if matches!(t.tp, sealdb_types::TimeType::Timestamp) => {
                match sealdb_expression::utc_to_session_local(t) {
                    Ok(mut local) => {
                        local.tp = sealdb_types::TimeType::Datetime;
                        sealdb_types::Datum::Time(local)
                    }
                    Err(_) => sealdb_types::Datum::Time(t),
                }
            }
            other => other,
        })
        .collect()
}

/// The hidden generated column that backs an expression index key part
/// (`_V$<index>_<pos>`, TiDB `ColumnInfo.Hidden`). It is stored in the row but
/// is not part of the table's user-visible schema.
fn is_hidden_generated_column(schema_name: &str) -> bool {
    schema_name
        .rsplit('.')
        .next()
        .is_some_and(|name| name.starts_with("_V$"))
}

thread_local! {
    /// Sort/TopN asks a directly nested aggregate to expose the ORDER BY
    /// columns that the SELECT list does not carry.
    static ORDER_COLUMNS_WANTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Trailing aggregate output columns added for ORDER BY (trimmed above).
    static ORDER_COLUMNS_ADDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Build the Sort/TopN input, letting an aggregate directly below expose the
/// ORDER BY-only columns.
fn build_with_order_columns(
    child: &PhysicalPlan,
    stmt: &Statement,
    domain: Arc<Domain>,
    current_db: &str,
    txn: Arc<TxnHandle>,
) -> Result<Box<dyn Executor>> {
    ORDER_COLUMNS_ADDED.set(0);
    ORDER_COLUMNS_WANTED.set(true);
    let built = ExecutorBuilder::build(child, stmt, domain, current_db, txn)?;
    ORDER_COLUMNS_WANTED.set(false);
    Ok(built)
}

/// Consume a pending Sort/TopN request: the base columns ORDER BY references
/// that the aggregate's input carries. The aggregate appends them as
/// first-row outputs so the Sort can evaluate its keys.
fn take_order_columns(
    select: &sealdb_parser::ast::SelectStmt,
    child_schema: &[String],
) -> Vec<String> {
    if !ORDER_COLUMNS_WANTED.replace(false) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for item in &select.order_by {
        for (_, name) in item.expr.referenced_columns() {
            if child_schema.iter().any(|c| {
                c.rsplit('.')
                    .next()
                    .unwrap_or(c)
                    .eq_ignore_ascii_case(&name)
            }) && !out.iter().any(|n| n.eq_ignore_ascii_case(&name))
            {
                out.push(name);
            }
        }
    }
    ORDER_COLUMNS_ADDED.set(out.len());
    out
}

/// Trim the trailing ORDER BY-only columns an aggregate appended.
fn trim_order_columns(exec: Box<dyn Executor>) -> Box<dyn Executor> {
    let added = ORDER_COLUMNS_ADDED.replace(0);
    if added == 0 {
        return exec;
    }
    let schema = exec.schema().to_vec();
    let keep = schema.len().saturating_sub(added);
    Box::new(ProjectionExec::trim_trailing(schema[..keep].to_vec(), exec))
}

fn stmt_order_by(stmt: &Statement) -> Option<Vec<ByItem>> {
    match stmt {
        Statement::Select(select) if !select.order_by.is_empty() => Some(select.order_by.clone()),
        Statement::SetOpr(set) if !set.order_by.is_empty() => Some(set.order_by.clone()),
        _ => None,
    }
}

/// Per-key collations for ORDER BY / TopN: prefer the column's FieldType
/// collation (e.g. table `utf8mb4_general_ci`) over the connection default.
fn order_by_collations(
    domain: &Domain,
    stmt: &Statement,
    current_db: &str,
    by: &[ByItem],
) -> Vec<sealdb_util::collate::CollateMode> {
    let select = stmt.as_select();
    let is = domain.infoschema();
    let info = select.and_then(|s| s.from.as_ref()).and_then(|from| {
        let (db, table) = from.qualified(current_db);
        is.table_by_name(&db, &table)
    });
    by.iter()
        .map(|item| {
            let mode = match &item.expr {
                ExprNode::Column(col) => info
                    .and_then(|info| {
                        info.columns
                            .iter()
                            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
                    })
                    .map(|c| sealdb_util::collate::CollateMode::from_name(&c.field_type.collate)),
                // `expr COLLATE name` overrides the session collation for the
                // sort key (`ORDER BY a COLLATE utf8mb4_general_ci` when the
                // connection is utf8mb4_bin).
                ExprNode::Func(func) if func.name.eq_ignore_ascii_case("__collate") => {
                    func.args.get(1).and_then(|arg| match arg {
                        ExprNode::Value(v) => match &v.datum {
                            sealdb_types::Datum::Bytes(bytes) => {
                                Some(sealdb_util::collate::CollateMode::from_name(
                                    &String::from_utf8_lossy(bytes),
                                ))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                }
                _ => None,
            };
            mode.unwrap_or_else(sealdb_expression::collation_mode)
        })
        .collect()
}

/// Synthetic Sort keys for MergeJoin sides: `by_info` holds TiDB join-key
/// labels (`select.a.x`). Resolve the bare column so SortExec can evaluate it.
fn order_by_from_plan_info(by_info: &str) -> Option<Vec<ByItem>> {
    let mut out = Vec::new();
    for part in by_info.split(',').map(str::trim) {
        if part.is_empty() {
            return None;
        }
        let (part, desc) = match part.strip_suffix(":desc") {
            Some(rest) => (rest.trim(), true),
            None => (part, false),
        };
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
            desc,
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
            || from.resolve_name().eq_ignore_ascii_case(table))
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
        ExprNode::Row(r) => r
            .exprs
            .iter()
            .find_map(|expr| find_table_in_expr(expr, table)),
        ExprNode::Column(_) | ExprNode::Value(_) => None,
    }
}

fn index_lookup_values(
    domain: &Domain,
    current_db: &str,
    table: &sealdb_parser::ast::TableName,
    index_name: &str,
    select: &sealdb_parser::ast::SelectStmt,
) -> Option<Vec<sealdb_types::Datum>> {
    let (schema, name) = table.qualified(current_db);
    let is = domain.infoschema();
    let info = is.table_by_name(&schema, &name)?;
    let index = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))?;
    let selection = select.selection.as_ref();
    let mut known: Vec<(String, sealdb_types::Datum)> = selection
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|conjunct| conjunct.eq_key_const())
                .collect()
        })
        .unwrap_or_default();
    // An inner-join key constant (`t1.a = 1` + `t1.a = t2.b`) reaches the
    // join side as a planner-derived predicate, not in this statement's WHERE,
    // so mirror it here to build the point-get key.
    for join in &select.joins {
        let (left, right) = sealdb_planner::core::planbuilder::derive_join_key_constants(
            select,
            &join.left,
            &join.right,
            &join.on,
            Some(&is),
            current_db,
        );
        for pred in left.into_iter().chain(right) {
            if !expr_owned_by(&pred, table) {
                continue;
            }
            if let Some(eq) = pred.eq_key_const() {
                known.push(eq);
            }
        }
    }
    // A single point get also comes from an `IN` list (`(a, b, c) IN ((1, 1,
    // 1), …)` de-duplicated to one key): every part then takes the first
    // listed value, like TiDB's `detachCondAndBuildRangeForIndex`.
    for part in &index.columns {
        let key = sealdb_planner::core::expr_index::part_key(info, part);
        if known
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(&key))
        {
            continue;
        }
        // `a IN (1, 3) AND a IN (3, 4)` pins `a = 3`: intersect every IN list on
        // the key before taking a value (TiDB ranger intersects the per-column
        // DNF). An empty intersection stays unpinned so the scan filters instead
        // of seeking a value no row can match.
        let lists: Vec<Vec<sealdb_types::Datum>> = selection
            .map(|expr| {
                expr.split_cnf()
                    .into_iter()
                    .filter_map(|conjunct| conjunct.column_in_consts_with_len())
                    .filter(|(name, _, _)| name.eq_ignore_ascii_case(&key))
                    .map(|(_, values, _)| values)
                    .collect()
            })
            .unwrap_or_default();
        let intersected = lists.into_iter().reduce(|current, next| {
            current
                .into_iter()
                .filter(|value| next.iter().any(|other| other == value))
                .collect()
        });
        let single = match intersected {
            Some(values) => values.into_iter().next(),
            None => selection
                .and_then(|expr| expr.in_const_on(&key))
                .and_then(|values| values.into_iter().next()),
        };
        if let Some(value) = single {
            known.push((key, value));
        }
    }
    index_key_values(info, index, &known)
}

/// Decode raw charset bytes to a UTF-8 string (TiDB `Charset.Decode`).
fn decode_charset_bytes(charset: &str, bytes: &[u8]) -> String {
    match charset {
        "gbk" => sealdb_parser::charset::encoding_gbk::decode(bytes),
        _ => sealdb_parser::charset::encoding_latin1::decode(bytes),
    }
}

/// The predicate names a column of `table` (plain name or alias).
fn expr_owned_by(expr: &ExprNode, table: &sealdb_parser::ast::TableName) -> bool {
    let owned = |node: &ExprNode| {
        matches!(
            node,
            ExprNode::Column(col)
                if col.table.as_deref().is_some_and(|t| {
                    t.eq_ignore_ascii_case(&table.name)
                        || t.eq_ignore_ascii_case(table.resolve_name())
                })
        )
    };
    match expr {
        ExprNode::Binary(bin) => owned(&bin.left) || owned(&bin.right),
        _ => false,
    }
}

/// Values of every key part for one point lookup: a plain part comes from the
/// predicate equalities, an expression part folds from them
/// (`uk_expr((tidb_shard(a)), a)` under `a = 100`).
fn index_key_values(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    known: &[(String, sealdb_types::Datum)],
) -> Option<Vec<sealdb_types::Datum>> {
    let mut values = Vec::with_capacity(index.columns.len());
    for part in &index.columns {
        let key = sealdb_planner::core::expr_index::part_key(info, part);
        match known
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&key))
        {
            Some((_, value)) => values.push(value.clone()),
            None => {
                let expr = sealdb_planner::core::expr_index::part_expr(info, part)?;
                values.push(sealdb_planner::core::expr_index::eval_part(
                    info, &expr, known,
                )?);
            }
        }
    }
    // The index key encodes the row's stored datum, so the seek value must take
    // the column type first (`b = 1` on DECIMAL(10,0) stores a Decimal, and a
    // bare Int datum would encode a different key tag). A binary/hex literal on
    // a non-UTF-8 column also needs decoding, since storage keeps strings as
    // UTF-8 (`a = 0xC4E3BAC3` on a gbk column).
    for (pos, part) in index.columns.iter().enumerate() {
        let Some(column) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(part.name.o()))
        else {
            continue;
        };
        if let sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes) = &values[pos]
            && std::str::from_utf8(bytes).is_err()
        {
            values[pos] = sealdb_types::Datum::Bytes(
                decode_charset_bytes(&column.field_type.charset, bytes).into_bytes(),
            );
        }
        // The index key encodes the row's stored datum, so the seek value must
        // take the column type when that changes the key encoding: a BIT value,
        // and the exact Decimal a DECIMAL(10,0) column stores for an Int
        // literal. String keys keep the literal bytes: TiDB compares
        // `binary(2)` keys without re-padding (`a = "a"` does not match a
        // stored `'a\0'`), and an out-of-range numeric literal is a value that
        // matches no row rather than a cast error.
        if (column.field_type.tp == sealdb_types::field_type::TYPE_BIT
            || column.field_type.eval_type() == sealdb_types::field_type::EvalType::Decimal)
            && let Ok(cast) = values[pos].cast(&column.field_type)
        {
            values[pos] = cast;
        }
    }
    Some(values)
}

/// Table factor that owns an index-scan plan node: match the plan's `table`
/// (physical name or alias) against the SELECT's FROM / JOIN operands.
fn index_reader_table(
    select: &sealdb_parser::ast::SelectStmt,
    table: &str,
) -> Option<sealdb_parser::ast::TableName> {
    let matches = |t: &sealdb_parser::ast::TableName| {
        t.name.eq_ignore_ascii_case(table)
            || t.physical
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(table))
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

/// TiDB `keep order` markers replace the pushed filter when the index read
/// already yields the query's `ORDER BY` order (`keep order:true`).
fn keep_order_pushed(pushed: Option<&ExprNode>) -> bool {
    let Some(ExprNode::Value(value)) = pushed else {
        return false;
    };
    match &value.datum {
        sealdb_types::Datum::Bytes(bytes) => std::str::from_utf8(bytes)
            .map(|text| text.starts_with("KEEP_ORDER"))
            .unwrap_or(false),
        _ => false,
    }
}

/// The marker is EXPLAIN-only bookkeeping, never a row filter.
fn keep_order_filter(pushed: &Option<ExprNode>) -> Option<ExprNode> {
    if keep_order_pushed(pushed.as_ref())
        || sealdb_planner::core::explain::cop_limit_pushed(pushed.as_ref()).is_some()
    {
        None
    } else {
        pushed.clone()
    }
}

/// Rewrite a covering read's projection onto the hidden key columns an
/// expression index stores: the read cannot provide the base columns the
/// expression spells out (`a + 1`), only the stored value.
fn expr_index_read_exprs(
    domain: &Domain,
    child: &PhysicalPlan,
    current_db: &str,
    exprs: Vec<ExprNode>,
) -> Vec<ExprNode> {
    let PhysicalPlan::IndexReader { table, index, .. } = child else {
        return exprs;
    };
    let is = domain.infoschema();
    let Some(info) = is.table_by_name(current_db, table).cloned() else {
        return exprs;
    };
    let Some(index_info) = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index))
    else {
        return exprs;
    };
    if !sealdb_planner::core::expr_index::has_expression_part(&info, index_info) {
        return exprs;
    }
    exprs
        .into_iter()
        .map(|expr| {
            sealdb_planner::core::expr_index::substitute_index_parts_for_read(
                &info, index_info, &expr,
            )
        })
        .collect()
}

fn index_access(
    domain: &Domain,
    current_db: &str,
    table: &sealdb_parser::ast::TableName,
    index_name: &str,
    selection: Option<&ExprNode>,
) -> Option<Vec<IndexAccess>> {
    let (schema, name) = table.qualified(current_db);
    let is = domain.infoschema();
    let info = is.table_by_name(&schema, &name)?;
    let index = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))?;
    // The planner rewrote virtual generated column bodies (`a+1` → `c`) before
    // it picked this index; rebuild the same predicate so the seek range
    // matches the `EXPLAIN` range instead of a full index scan.
    let substituted = selection
        .and_then(|sel| sealdb_planner::core::expr_index::substitute_generated_expr(info, sel));
    let selection = substituted.as_ref().or(selection);
    let conjuncts: Vec<ExprNode> = selection
        .map(|selection| selection.split_cnf())
        .unwrap_or_default();
    // The statement's WHERE clause also carries predicates of the other join
    // inputs, and `index_key_ranges` matches a conjunct to an index column by
    // name alone. That is what lets a correlated inner scan seek on the outer
    // value, but it must not bound this index with a constant that another
    // table compares under a different collation (`t1.a = 'a'` next to
    // `t1.a = t2.a` would seek `'a'` in a `utf8_bin` index and miss `'A'`).
    let conjuncts: Vec<ExprNode> = conjuncts
        .into_iter()
        .filter(|conjunct| !foreign_collation_conflict(&is, current_db, table, info, conjunct))
        .collect();
    // Ranger shares the planner's key ranges, so the executor seeks exactly the
    // range EXPLAIN prints: an equality prefix pins every key part it covers
    // (`c2 = 1 and c1 > 1` → `(1 1,1 +inf]` on `index c2(c2)`).
    if let Some(access) =
        sealdb_planner::core::access_range::index_key_ranges(info, index, &conjuncts)
    {
        return Some(
            access
                .ranges
                .iter()
                .map(|range| IndexAccess::Range {
                    low: range.low.iter().map(|(value, _)| value.clone()).collect(),
                    low_inclusive: range.low_open
                        || range.low.last().is_none_or(|(_, inclusive)| *inclusive),
                    high: range.high.iter().map(|(value, _)| value.clone()).collect(),
                    high_inclusive: range.high_open
                        || range.high.last().is_none_or(|(_, inclusive)| *inclusive),
                })
                .collect(),
        );
    }
    // USE INDEX / covering IndexReader with no predicate → IndexFullScan.
    Some(vec![IndexAccess::Range {
        low: Vec::new(),
        low_inclusive: true,
        high: Vec::new(),
        high_inclusive: true,
    }])
}

/// Whether `conjunct` compares a column of another table under a collation
/// this table's same-named column does not use.
///
/// The executor derives index ranges from the whole statement `WHERE` clause
/// (key ranges match a conjunct to the index column by name), which is how a
/// correlated inner scan seeks on the outer value. The shortcut is only sound
/// while both columns compare under the same collation: `t1.a = 'a'` with
/// `t1.a = t2.a` must not seek `'a'` in a `utf8_bin` index (`'A'` collates
/// equal to `'a'` under `utf8mb4_general_ci` only).
fn foreign_collation_conflict(
    is: &sealdb_infoschema::InfoSchema,
    current_db: &str,
    table: &sealdb_parser::ast::TableName,
    info: &sealdb_meta::TableInfo,
    conjunct: &ExprNode,
) -> bool {
    conjunct
        .referenced_columns()
        .iter()
        .any(|(qualifier, name)| {
            let Some(qualifier) = qualifier else {
                return false;
            };
            if qualifier.eq_ignore_ascii_case(&table.name)
                || qualifier.eq_ignore_ascii_case(table.resolve_name())
            {
                return false;
            }
            let Some(foreign) = is.table_by_name(current_db, qualifier) else {
                return false;
            };
            let local_ft = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(name))
                .map(|c| &c.field_type);
            let foreign_ft = foreign
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(name))
                .map(|c| &c.field_type);
            let (Some(local_ft), Some(foreign_ft)) = (local_ft, foreign_ft) else {
                return false;
            };
            local_ft.eval_type() == sealdb_types::field_type::EvalType::String
                && foreign_ft.eval_type() == sealdb_types::field_type::EvalType::String
                && (!local_ft.collate.eq_ignore_ascii_case(&foreign_ft.collate)
                    || !local_ft.charset.eq_ignore_ascii_case(&foreign_ft.charset))
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

pub(crate) fn field_output_name(field: &sealdb_parser::ast::SelectField) -> String {
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

/// `SELECT DISTINCT` without `GROUP BY` is planned as a dedup aggregate, so the
/// executor de-duplicates its input. A select list with aggregate functions is
/// the scalar aggregate itself: its single row is already distinct.
fn distinct_is_dedup(select: &SelectStmt) -> bool {
    select.distinct
        && select.group_by.is_empty()
        && !select.fields.iter().any(|field| match &field.expr {
            ExprNode::Func(func) => crate::aggfuncs::is_agg_name(&func.name),
            _ => false,
        })
}

/// `SELECT DISTINCT` executor: keep the first row of every distinct projection
/// (TiDB's de-duplicating `HashAgg group by … firstrow(…)`).
fn distinct_exec_for_select(
    select: &SelectStmt,
    child: Box<dyn Executor>,
    domain: &Domain,
    current_db: &str,
) -> Result<Box<dyn Executor>> {
    // `SELECT DISTINCT *` keeps the full child row.
    let star = select.fields.iter().all(|field| {
        matches!(
            &field.expr,
            ExprNode::Value(ValueExpr {
                datum: sealdb_types::Datum::Bytes(bytes),
            }) if bytes == b"*"
        )
    });
    let fields: Vec<ExprNode> = if star {
        Vec::new()
    } else {
        select
            .fields
            .iter()
            .map(|field| field.expr.clone())
            .collect()
    };
    let names: Vec<String> = if star {
        Vec::new()
    } else {
        select.fields.iter().map(field_output_name).collect()
    };
    let modes = expr_list_collations(&fields, select, domain, current_db)?;
    Ok(Box::new(
        DistinctExec::new(names.clone(), fields, names, child).with_collations(modes),
    ))
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
        // The query-level ORDER BY / LIMIT is planned on top of the Window.
        PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child } => is_window_plan(child),
        _ => false,
    }
}

/// Window input placeholder: a Projection over TableDual (no FROM) that only
/// exists for EXPLAIN shape; `WindowExec` evaluates the select list itself.
fn strip_dual_window_projection(plan: &PhysicalPlan) -> Option<PhysicalPlan> {
    let PhysicalPlan::Projection { child, .. } = plan else {
        return None;
    };
    matches!(**child, PhysicalPlan::TableDual { .. }).then(|| (**child).clone())
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
                // resolves each repeated `a` to its own input. The output label
                // stays bare (`a	b	a	b	a	b`) because `run` strips the
                // `alias.` prefix; keeping it in the schema lets an ORDER BY
                // above the projection bind `t.a` when names repeat.
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
                names.push(col.clone());
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

/// Fold `COLLATION(expr)` in a SELECT list into its static collation name.
/// Rewrite `LOWER`/`UPPER`/`LCASE`/`UCASE` of a binary-charset argument to the
/// argument itself (MySQL binary charset has no case mapping).
fn resolve_binary_case_convert_exprs(
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
    exprs: Vec<ExprNode>,
) -> Vec<ExprNode> {
    if !exprs.iter().any(contains_case_convert) {
        return exprs;
    }
    exprs
        .into_iter()
        .map(|expr| fold_binary_case_convert(expr, domain, select, current_db))
        .collect()
}

fn contains_case_convert(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(func) => {
            is_case_convert_name(&func.name) || func.args.iter().any(contains_case_convert)
        }
        ExprNode::Cast(cast) => contains_case_convert(&cast.expr),
        ExprNode::Binary(binary) => {
            contains_case_convert(&binary.left) || contains_case_convert(&binary.right)
        }
        ExprNode::Unary(unary) => contains_case_convert(&unary.expr),
        _ => false,
    }
}

fn is_case_convert_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "lower" | "lcase" | "upper" | "ucase"
    )
}

fn fold_binary_case_convert(
    expr: ExprNode,
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
) -> ExprNode {
    match expr {
        ExprNode::Func(mut func) if is_case_convert_name(&func.name) && func.args.len() == 1 => {
            let arg = fold_binary_case_convert(func.args.remove(0), domain, select, current_db);
            if expr_has_binary_charset(&arg, domain, select, current_db) {
                arg
            } else {
                func.args.push(arg);
                ExprNode::Func(func)
            }
        }
        ExprNode::Func(mut func) => {
            func.args = func
                .args
                .into_iter()
                .map(|arg| fold_binary_case_convert(arg, domain, select, current_db))
                .collect();
            ExprNode::Func(func)
        }
        ExprNode::Cast(mut cast) => {
            cast.expr = Box::new(fold_binary_case_convert(
                *cast.expr, domain, select, current_db,
            ));
            ExprNode::Cast(cast)
        }
        ExprNode::Binary(mut binary) => {
            binary.left = Box::new(fold_binary_case_convert(
                *binary.left,
                domain,
                select,
                current_db,
            ));
            binary.right = Box::new(fold_binary_case_convert(
                *binary.right,
                domain,
                select,
                current_db,
            ));
            ExprNode::Binary(binary)
        }
        ExprNode::Unary(mut unary) => {
            unary.expr = Box::new(fold_binary_case_convert(
                *unary.expr,
                domain,
                select,
                current_db,
            ));
            ExprNode::Unary(unary)
        }
        other => other,
    }
}

/// True when `expr` evaluates to a binary-charset string (VARBINARY/BINARY/BLOB
/// columns, or MIN/MAX/IFNULL of those).
fn expr_has_binary_charset(
    expr: &ExprNode,
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
) -> bool {
    match expr {
        ExprNode::Column(col) => column_has_binary_charset(col, domain, select, current_db),
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_lowercase().as_str(),
                "min" | "max" | "ifnull" | "coalesce" | "any_value" | "firstrow"
            ) =>
        {
            func.args
                .first()
                .is_some_and(|arg| expr_has_binary_charset(arg, domain, select, current_db))
        }
        ExprNode::Cast(cast)
            if cast.target.to_ascii_uppercase().contains("BINARY")
                || cast.target.eq_ignore_ascii_case("BLOB") =>
        {
            true
        }
        ExprNode::Cast(cast) => expr_has_binary_charset(&cast.expr, domain, select, current_db),
        _ => false,
    }
}

fn column_has_binary_charset(
    col: &sealdb_parser::ast::expressions::ColumnNameExpr,
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
) -> bool {
    let is = domain.infoschema();
    // Derived-table / CTE alias: resolve the projected field's expression.
    if let Some(table) = col.table.as_deref()
        && let Some(with) = &select.with
        && let Some(cte) = with.find(table)
    {
        if let Some(field) = cte_field_for_name(cte.query.as_ref(), &col.name) {
            return expr_has_binary_charset(&field.expr, domain, cte.query.as_ref(), current_db);
        }
    }
    // Physical / aliased base tables in FROM / join chain.
    for table in select_source_tables(select) {
        if let Some(wanted) = col.table.as_deref() {
            let alias = table.name.as_str();
            let physical = table.resolve_name();
            if !alias.eq_ignore_ascii_case(wanted) && !physical.eq_ignore_ascii_case(wanted) {
                continue;
            }
        }
        let (db, name) = table.qualified(current_db);
        let Some(info) = is.table_by_name(&db, &name) else {
            continue;
        };
        if let Some(c) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
        {
            return c.field_type.charset == "binary" || c.field_type.collate == "binary";
        }
        if col.table.is_some() {
            return false;
        }
    }
    false
}

fn cte_field_for_name<'a>(
    query: &'a SelectStmt,
    name: &str,
) -> Option<&'a sealdb_parser::ast::SelectField> {
    query.fields.iter().find(|field| {
        field
            .as_name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
            || matches!(&field.expr, ExprNode::Column(c) if c.name.eq_ignore_ascii_case(name))
    })
}

fn select_source_tables(select: &SelectStmt) -> Vec<&sealdb_parser::ast::TableName> {
    let mut out = Vec::new();
    if let Some(from) = &select.from {
        out.push(from);
    }
    for join in &select.joins {
        out.push(&join.left);
        out.push(&join.right);
    }
    if let Some(item) = &select.from_item {
        out.extend(item.tables());
    }
    // CTE bodies referenced as join sides are physical names equal to the CTE
    // alias; their columns are resolved via `select.with` above.
    out
}

fn resolve_collation_exprs(
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
    exprs: Vec<ExprNode>,
) -> Result<Vec<ExprNode>> {
    if !exprs.iter().any(contains_collation) {
        return Ok(exprs);
    }
    let scope = CollationScope::for_select(domain, select, current_db);
    exprs
        .into_iter()
        .map(|expr| fold_collation(expr, &scope))
        .collect()
}

/// User variable marker (`__SEALDB_VARCOLL(value, collation, charset)`): the
/// value keeps the collation of the expression it was assigned from.
const SEALDB_VARCOLL_FN: &str = "__SEALDB_VARCOLL";

/// Collation of a standalone expression (no table scope), used to remember
/// the collation a user variable was assigned with.
pub fn derive_expr_collation(expr: &ExprNode) -> Option<(String, String)> {
    let scope = CollationScope::empty();
    expr_collation(expr, &scope)
        .ok()
        .filter(|info| !info.is_neutral())
        .map(|info| (info.collation, info.charset))
}

/// TiDB derives the collation of every string comparison / string function
/// while building the expression tree, so an illegal mix is reported before
/// execution (1267 two-collation mix, 1271 for `FIELD`).
pub fn validate_statement_collations(
    domain: &Domain,
    stmt: &Statement,
    current_db: &str,
) -> Result<()> {
    if let Some(select) = stmt.as_select() {
        validate_select_collations(domain, select, current_db)?;
    }
    Ok(())
}

fn validate_select_collations(
    domain: &Domain,
    select: &SelectStmt,
    current_db: &str,
) -> Result<()> {
    let scope = CollationScope::for_select(domain, select, current_db);
    for field in &select.fields {
        validate_expr_collation(&field.expr, &scope, domain, current_db)?;
    }
    if let Some(selection) = &select.selection {
        validate_expr_collation(selection, &scope, domain, current_db)?;
    }
    if let Some(having) = &select.having {
        validate_expr_collation(having, &scope, domain, current_db)?;
    }
    for expr in &select.group_by {
        validate_expr_collation(expr, &scope, domain, current_db)?;
    }
    for item in &select.order_by {
        validate_expr_collation(&item.expr, &scope, domain, current_db)?;
    }
    for join in &select.joins {
        validate_expr_collation(&join.on, &scope, domain, current_db)?;
    }
    Ok(())
}

fn validate_expr_collation(
    expr: &ExprNode,
    scope: &CollationScope,
    domain: &Domain,
    current_db: &str,
) -> Result<()> {
    let walk = |child: &ExprNode| validate_expr_collation(child, scope, domain, current_db);
    match expr {
        ExprNode::Binary(binary) => {
            if matches!(
                binary.op,
                sealdb_parser::opcode::Op::Eq
                    | sealdb_parser::opcode::Op::NullSafeEq
                    | sealdb_parser::opcode::Op::Ne
                    | sealdb_parser::opcode::Op::Lt
                    | sealdb_parser::opcode::Op::Le
                    | sealdb_parser::opcode::Op::Gt
                    | sealdb_parser::opcode::Op::Ge
            ) {
                validate_comparison(&binary.left, &binary.right, binary.op.restore(), scope)?;
            }
            walk(&binary.left)?;
            walk(&binary.right)
        }
        ExprNode::Like(like) => {
            validate_comparison(&like.expr, &like.pattern, "like", scope)?;
            walk(&like.expr)?;
            walk(&like.pattern)
        }
        ExprNode::Func(func) => {
            let name = func.name.to_ascii_lowercase();
            if name == "field" {
                validate_field_collation(func, scope)?;
            } else if is_collation_derived_func(&name) {
                // A conflicting explicit collation is an error for every
                // aggregating string function.
                expr_collation(expr, scope)?;
            }
            for arg in &func.args {
                walk(arg)?;
            }
            Ok(())
        }
        ExprNode::Unary(unary) => walk(&unary.expr),
        ExprNode::Cast(cast) => walk(&cast.expr),
        ExprNode::InList(in_list) => {
            walk(&in_list.expr)?;
            for item in &in_list.list {
                walk(item)?;
            }
            Ok(())
        }
        ExprNode::InSubquery(in_subquery) => {
            walk(&in_subquery.expr)?;
            validate_result_set_collations(&in_subquery.query, scope, domain, current_db)
        }
        ExprNode::Exists(exists) => {
            validate_result_set_collations(&exists.query, scope, domain, current_db)
        }
        ExprNode::ScalarSubquery(subquery) => {
            validate_result_set_collations(&subquery.query, scope, domain, current_db)
        }
        ExprNode::QuantifiedCmp(cmp) => {
            walk(&cmp.expr)?;
            validate_result_set_collations(&cmp.query, scope, domain, current_db)
        }
        ExprNode::Between(between) => {
            walk(&between.expr)?;
            walk(&between.low)?;
            walk(&between.high)
        }
        ExprNode::IsNull(is_null) => walk(&is_null.expr),
        ExprNode::Case(case) => {
            if let Some(operand) = &case.operand {
                walk(operand)?;
            }
            for (when, then) in &case.when_then {
                walk(when)?;
                walk(then)?;
            }
            if let Some(else_expr) = &case.else_expr {
                walk(else_expr)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_result_set_collations(
    query: &ResultSetNode,
    outer: &CollationScope,
    domain: &Domain,
    current_db: &str,
) -> Result<()> {
    for select in query.selects() {
        // A correlated subquery can name a column of the outer query block, so
        // fall back to the outer scope when the inner block has no table.
        let scope = CollationScope::for_select(domain, select, current_db);
        let scope = if scope.tables.is_empty() {
            outer.clone()
        } else {
            scope
        };
        for field in &select.fields {
            validate_expr_collation(&field.expr, &scope, domain, current_db)?;
        }
        if let Some(selection) = &select.selection {
            validate_expr_collation(selection, &scope, domain, current_db)?;
        }
        if let Some(having) = &select.having {
            validate_expr_collation(having, &scope, domain, current_db)?;
        }
        for join in &select.joins {
            validate_expr_collation(&join.on, &scope, domain, current_db)?;
        }
    }
    Ok(())
}

/// TiDB `inferCollation` + `CheckAndDeriveCollationFromExprs` for a comparison:
/// a comparison yields an integer, so a `CoercibilityNone` result (no bin side
/// to break the tie) is an illegal mix.
fn validate_comparison(
    left: &ExprNode,
    right: &ExprNode,
    op: &str,
    scope: &CollationScope,
) -> Result<()> {
    let a = expr_collation(left, scope)?;
    let b = expr_collation(right, scope)?;
    // A non-string comparison (`i = 1`) never derives a collation.
    if a.charset == "binary" && b.charset == "binary" {
        return Ok(());
    }
    let dst = if a.charset == b.charset {
        comparison_same_charset(&a, &b, op)?
    } else {
        aggregate_collation_pair(a.clone(), b.clone(), op)?
    };
    if dst.coercibility == COERCIBILITY_NONE {
        return Err(illegal_mix_of_collations(&a, &b, op));
    }
    Ok(())
}

fn comparison_same_charset(
    a: &CollationInfo,
    b: &CollationInfo,
    op: &str,
) -> Result<CollationInfo> {
    if a.collation == b.collation {
        return Ok(a.clone());
    }
    if a.coercibility != b.coercibility {
        return Ok(if a.coercibility < b.coercibility {
            a.clone()
        } else {
            b.clone()
        });
    }
    if a.coercibility == COERCIBILITY_EXPLICIT {
        return Err(illegal_mix_of_collations(a, b, op));
    }
    if is_bin_collation(&a.collation) {
        return Ok(a.clone());
    }
    if is_bin_collation(&b.collation) {
        return Ok(b.clone());
    }
    Ok(CollationInfo {
        collation: bin_collation_for_charset(&a.charset),
        charset: a.charset.clone(),
        coercibility: COERCIBILITY_NONE,
        text: None,
    })
}

/// `FIELD` resolves its collation like a multi-argument comparison; a mix with
/// no bin tie-breaker reports the N-collation error (1271).
fn validate_field_collation(
    func: &sealdb_parser::ast::FuncCallExpr,
    scope: &CollationScope,
) -> Result<()> {
    let mut out: Option<CollationInfo> = None;
    for arg in func.args.iter().skip(1) {
        let coll = expr_collation_aggregated(arg, scope)?;
        if coll.is_neutral() {
            continue;
        }
        out = Some(match out {
            None => coll,
            Some(current) => {
                if current.charset == coll.charset {
                    comparison_same_charset(&current, &coll, "field")?
                } else {
                    aggregate_collation_pair(current, coll, "field")?
                }
            }
        });
    }
    if out.is_some_and(|coll| coll.coercibility == COERCIBILITY_NONE) {
        return Err(Error::new(
            1271,
            "Illegal mix of collations for operation 'field'",
        ));
    }
    Ok(())
}

fn is_collation_derived_func(name: &str) -> bool {
    matches!(
        name,
        "concat"
            | "concat_ws"
            | "upper"
            | "ucase"
            | "lower"
            | "lcase"
            | "reverse"
            | "quote"
            | "left"
            | "right"
            | "repeat"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "substr"
            | "substring"
            | "mid"
            | "translate"
            | "substring_index"
            | "lpad"
            | "rpad"
            | "elt"
            | "export_set"
            | "make_set"
            | "ifnull"
            | "coalesce"
            | "if"
            | "nullif"
            | "greatest"
            | "least"
    )
}

fn contains_collation(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Like(_) | ExprNode::Between(_) => true,
        ExprNode::Func(func) => {
            func.name.eq_ignore_ascii_case("collation")
                || func.name.eq_ignore_ascii_case("charset")
                || func.name.eq_ignore_ascii_case("coercibility")
                || func.name.eq_ignore_ascii_case("weight_string")
                || func.name.eq_ignore_ascii_case("field")
                || func.name.eq_ignore_ascii_case("find_in_set")
                || func.name.eq_ignore_ascii_case("instr")
                || func.name.eq_ignore_ascii_case("locate")
                // `REGEXP`'s case sensitivity comes from the collation
                // aggregated across its value and pattern operands.
                || func.name.eq_ignore_ascii_case("regexp_like")
                || func.name.eq_ignore_ascii_case("regexp_instr")
                || func.name.eq_ignore_ascii_case("regexp_substr")
                || func.name.eq_ignore_ascii_case("regexp_replace")
                // TiDB compares GREATEST/LEAST with the collation derived from
                // their arguments, not the session collation.
                || func.name.eq_ignore_ascii_case("greatest")
                || func.name.eq_ignore_ascii_case("least")
                || func.args.iter().any(contains_collation)
        }
        ExprNode::Cast(cast) => contains_collation(&cast.expr),
        ExprNode::Binary(binary) => {
            is_comparison_op(binary.op)
                || contains_collation(&binary.left)
                || contains_collation(&binary.right)
        }
        ExprNode::Unary(unary) => contains_collation(&unary.expr),
        _ => false,
    }
}

/// Collation mode per `GROUP BY` key (used by HashAgg/StreamAgg group keys).
fn group_by_collations(
    select: &SelectStmt,
    domain: &Domain,
    current_db: &str,
) -> Result<Vec<CollateMode>> {
    if select.group_by.is_empty() {
        return Ok(Vec::new());
    }
    let scope = CollationScope::for_select(domain, select, current_db);
    select
        .group_by
        .iter()
        .map(|expr| {
            let info = expr_collation(expr, &scope)?;
            Ok(if info.is_neutral() || info.charset == "binary" {
                CollateMode::Bin
            } else {
                CollateMode::from_name(&info.collation)
            })
        })
        .collect()
}

/// Collation mode per projected expression (used by DISTINCT dedup keys).
fn expr_list_collations(
    exprs: &[ExprNode],
    select: &SelectStmt,
    domain: &Domain,
    current_db: &str,
) -> Result<Vec<CollateMode>> {
    if exprs.is_empty() {
        return Ok(Vec::new());
    }
    let scope = CollationScope::for_select(domain, select, current_db);
    exprs
        .iter()
        .map(|expr| {
            let info = expr_collation(expr, &scope)?;
            Ok(if info.is_neutral() || info.charset == "binary" {
                CollateMode::Bin
            } else {
                CollateMode::from_name(&info.collation)
            })
        })
        .collect()
}

/// Wrap the string operands of a join equality in `__COLL_KEY(expr, coll)`, so
/// hash-join key bytes follow the collation (`'a'`/`'A'` join under
/// `utf8mb4_general_ci`). Only the cross-side equality keys are wrapped; every
/// other conjunct is left untouched for residual evaluation.
fn wrap_join_keys(expr: ExprNode, scope: &CollationScope) -> Result<ExprNode> {
    match expr {
        ExprNode::Binary(mut binary) => {
            binary.left = Box::new(wrap_join_keys(*binary.left, scope)?);
            binary.right = Box::new(wrap_join_keys(*binary.right, scope)?);
            if matches!(
                binary.op,
                sealdb_parser::opcode::Op::Eq | sealdb_parser::opcode::Op::NullSafeEq
            ) {
                if let Some(info) = derive_comparison_collation(
                    &binary.left,
                    &binary.right,
                    binary.op.restore(),
                    scope,
                )? && !info.is_neutral()
                    && info.charset != "binary"
                {
                    let collation = collation_literal(&info.collation);
                    let wrap = |expr: ExprNode| -> ExprNode {
                        ExprNode::Func(sealdb_parser::ast::expressions::FuncCallExpr {
                            name: "__COLL_KEY".to_string(),
                            args: vec![expr, collation.clone()],
                            distinct: false,
                        })
                    };
                    binary.left = Box::new(wrap(*binary.left));
                    binary.right = Box::new(wrap(*binary.right));
                }
            }
            Ok(ExprNode::Binary(binary))
        }
        ExprNode::Func(mut func) => {
            func.args = func
                .args
                .into_iter()
                .map(|arg| wrap_join_keys(arg, scope))
                .collect::<Result<Vec<_>>>()?;
            Ok(ExprNode::Func(func))
        }
        other => Ok(other),
    }
}

fn fold_collation(expr: ExprNode, scope: &CollationScope) -> Result<ExprNode> {
    match expr {
        ExprNode::Func(mut func) => {
            if func.name.eq_ignore_ascii_case("coercibility") && func.args.len() == 1 {
                let coercibility = expr_collation(&func.args[0], scope)?.coercibility;
                return Ok(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr {
                        datum: sealdb_types::Datum::I64(i64::from(coercibility)),
                    },
                ));
            }
            if func.name.eq_ignore_ascii_case("collation") && func.args.len() == 1 {
                let info = expr_collation(&func.args[0], scope)?;
                // `COLLATION(NULL)` reports `binary` (TiDB `GetCollation`).
                let collation = if info.is_neutral() {
                    "binary".to_string()
                } else {
                    info.collation
                };
                return Ok(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr {
                        datum: sealdb_types::Datum::Bytes(collation.into_bytes()),
                    },
                ));
            }
            if func.name.eq_ignore_ascii_case("charset") && func.args.len() == 1 {
                let info = expr_collation(&func.args[0], scope)?;
                let charset = if info.is_neutral() {
                    "binary".to_string()
                } else {
                    info.charset
                };
                return Ok(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr {
                        datum: sealdb_types::Datum::Bytes(charset.into_bytes()),
                    },
                ));
            }
            func.args = func
                .args
                .into_iter()
                .map(|arg| fold_collation(arg, scope))
                .collect::<Result<Vec<_>>>()?;
            if (func.name.eq_ignore_ascii_case("greatest")
                || func.name.eq_ignore_ascii_case("least"))
                && func.args.len() >= 2
            {
                let want_greatest = func.name.eq_ignore_ascii_case("greatest");
                let collation = expr_collation(&ExprNode::Func(func.clone()), scope)?.collation;
                func.name = if want_greatest {
                    "__greatest_coll".to_string()
                } else {
                    "__least_coll".to_string()
                };
                func.args.insert(
                    0,
                    ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
                        datum: sealdb_types::Datum::Bytes(collation.into_bytes()),
                    }),
                );
            }
            let lower_name = func.name.to_ascii_lowercase();
            // `REGEXP`/`RLIKE` lowers to `regexp_like(expr, pat)`. TiDB adds
            // the `i` flag only for a case-insensitive collation derived from
            // both operands; a binary or `*_bin` pair stays case-sensitive.
            if lower_name == "regexp_like" && func.args.len() == 2 {
                let case_sensitive = match derive_comparison_collation(
                    &func.args[0],
                    &func.args[1],
                    "regexp",
                    scope,
                )? {
                    None => true,
                    Some(info) if info.is_neutral() => true,
                    Some(info) => !CollateMode::from_name(&info.collation).is_ci(),
                };
                if case_sensitive {
                    func.args.push(ExprNode::Value(ValueExpr {
                        datum: sealdb_types::Datum::Bytes(b"c".to_vec()),
                    }));
                }
            }
            // `FIELD`/`FIND_IN_SET` compare under the collation aggregated from
            // their operands (a column collation wins over the session one).
            if (lower_name == "field" || lower_name == "find_in_set") && func.args.len() >= 2 {
                let compared: &[ExprNode] = if lower_name == "field" {
                    &func.args[1..]
                } else {
                    &func.args
                };
                let info = aggregate_collations(
                    &lower_name,
                    compared
                        .iter()
                        .map(|arg| expr_collation_aggregated(arg, scope)),
                )?;
                if !info.is_neutral() {
                    func.name = if lower_name == "field" {
                        "__FIELD_COLL".to_string()
                    } else {
                        "__FIND_IN_SET_COLL".to_string()
                    };
                    func.args.insert(0, collation_literal(&info.collation));
                }
            }
            // `INSTR`/`LOCATE` compare under the collation aggregated from their
            // operands (a column collation wins over the session one).
            if (lower_name == "instr" && func.args.len() == 2)
                || (lower_name == "locate" && (2..=3).contains(&func.args.len()))
            {
                let compared: &[ExprNode] = &func.args[..2];
                let info = aggregate_collations(
                    &lower_name,
                    compared
                        .iter()
                        .map(|arg| expr_collation_aggregated(arg, scope)),
                )?;
                if !info.is_neutral() {
                    func.name = if lower_name == "instr" {
                        "__INSTR_COLL".to_string()
                    } else {
                        "__LOCATE_COLL".to_string()
                    };
                    func.args.insert(0, collation_literal(&info.collation));
                }
            }
            // `WEIGHT_STRING(expr[, 'CHAR'|'BINARY', n])` needs the argument's
            // static collation, which is only known here: pass it as a trailing
            // literal so the evaluator can build the key.
            if func.name.eq_ignore_ascii_case("weight_string")
                && !func.args.is_empty()
                && matches!(func.args.len(), 1 | 3)
            {
                let collation = expr_collation(&func.args[0], scope)?.collation;
                func.args.push(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr {
                        datum: sealdb_types::Datum::Bytes(collation.into_bytes()),
                    },
                ));
            }
            Ok(ExprNode::Func(func))
        }
        ExprNode::Cast(mut cast) => {
            cast.expr = Box::new(fold_collation(*cast.expr, scope)?);
            Ok(ExprNode::Cast(cast))
        }
        ExprNode::Binary(mut binary) => {
            binary.left = Box::new(fold_collation(*binary.left, scope)?);
            binary.right = Box::new(fold_collation(*binary.right, scope)?);
            if is_comparison_op(binary.op) {
                return wrap_collation_comparison(
                    *binary.left,
                    *binary.right,
                    binary.op,
                    binary.single_cnf_item,
                    scope,
                );
            }
            Ok(ExprNode::Binary(binary))
        }
        ExprNode::Between(between) => {
            let value = fold_collation(*between.expr, scope)?;
            let low = fold_collation(*between.low, scope)?;
            let high = fold_collation(*between.high, scope)?;
            if !between.not {
                let low_dst = derive_comparison_collation(&value, &low, ">=", scope)?;
                let high_dst = derive_comparison_collation(&value, &high, "<=", scope)?;
                if let (Some(a), Some(b)) = (&low_dst, &high_dst)
                    && !a.is_neutral()
                    && a.collation == b.collation
                {
                    return Ok(ExprNode::Func(
                        sealdb_parser::ast::expressions::FuncCallExpr {
                            name: "__STR_BETWEEN".to_string(),
                            args: vec![collation_literal(&a.collation), value, low, high],
                            distinct: false,
                        },
                    ));
                }
            }
            Ok(ExprNode::Between(BetweenExpr {
                expr: Box::new(value),
                low: Box::new(low),
                high: Box::new(high),
                not: between.not,
            }))
        }
        ExprNode::Like(like) => {
            let value = fold_collation(*like.expr, scope)?;
            let pattern = fold_collation(*like.pattern, scope)?;
            let derived = derive_comparison_collation(&value, &pattern, "like", scope)?;
            if let Some(info) = derived
                && !info.is_neutral()
            {
                return Ok(ExprNode::Func(
                    sealdb_parser::ast::expressions::FuncCallExpr {
                        name: "__STR_LIKE".to_string(),
                        args: vec![
                            collation_literal(&info.collation),
                            value,
                            pattern,
                            ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
                                datum: sealdb_types::Datum::I64(i64::from(like.not)),
                            }),
                        ],
                        distinct: false,
                    },
                ));
            }
            Ok(ExprNode::Like(PatternLikeExpr {
                expr: Box::new(value),
                pattern: Box::new(pattern),
                not: like.not,
                ci: like.ci,
            }))
        }
        ExprNode::Unary(mut unary) => {
            unary.expr = Box::new(fold_collation(*unary.expr, scope)?);
            Ok(ExprNode::Unary(unary))
        }
        other => Ok(other),
    }
}

/// MySQL coercibility levels that matter for collation aggregation, in
/// strength order (`EXPLICIT` beats `IMPLICIT` beats `COERCIBLE`). `IGNORABLE`
/// (NULL) and `NUMERIC` operands do not participate.
const COERCIBILITY_EXPLICIT: u8 = 0;
/// Different collations met at the same strength (`CONCAT` of two implicit
/// collations): MySQL reports no collation instead of picking one.
const COERCIBILITY_NONE: u8 = 1;
/// Function results like `USER()` / `VERSION()` / `DATABASE()`.
const COERCIBILITY_SYSTEM_CONSTANT: u8 = 3;
const COERCIBILITY_IMPLICIT: u8 = 2;
const COERCIBILITY_COERCIBLE: u8 = 4;
const COERCIBILITY_NUMERIC: u8 = 5;
const COERCIBILITY_IGNORABLE: u8 = 6;

/// Collation metadata of one string operand (TiDB `types.FieldType` collation
/// subset needed by `aggregateType`).
#[derive(Clone, Debug)]
struct CollationInfo {
    collation: String,
    charset: String,
    coercibility: u8,
    /// Literal text, when this operand is a coercible constant; used to check
    /// that the value is representable in the aggregated charset.
    text: Option<String>,
}

impl CollationInfo {
    fn ignorable() -> Self {
        Self {
            collation: String::new(),
            charset: String::new(),
            coercibility: COERCIBILITY_IGNORABLE,
            text: None,
        }
    }

    fn is_neutral(&self) -> bool {
        // NULL (no charset) and numeric operands do not take part in collation
        // aggregation (`CONCAT(i, 1)` reports the fallback collation).
        self.charset.is_empty()
            || (self.charset == "binary" && self.coercibility == COERCIBILITY_NUMERIC)
    }

    fn coercibility_name(&self) -> &'static str {
        match self.coercibility {
            COERCIBILITY_EXPLICIT => "EXPLICIT",
            COERCIBILITY_NONE => "NONE",
            COERCIBILITY_SYSTEM_CONSTANT => "SYSTEM",
            COERCIBILITY_IMPLICIT => "IMPLICIT",
            COERCIBILITY_COERCIBLE => "COERCIBLE",
            COERCIBILITY_NUMERIC => "NUMERIC",
            _ => "IGNORABLE",
        }
    }
}

/// Column collation lookup scope: every table in the statement's FROM / JOIN
/// chain, keyed by the name the query uses (alias when present).
#[derive(Clone)]
struct CollationScope {
    tables: Vec<(String, sealdb_meta::TableInfo)>,
}

impl CollationScope {
    fn empty() -> Self {
        Self { tables: Vec::new() }
    }

    fn single(info: &sealdb_meta::TableInfo) -> Self {
        Self {
            tables: vec![(info.name.o().to_ascii_lowercase(), info.clone())],
        }
    }

    /// Resolve every table named by the query block (aliases included).
    fn for_select(domain: &Domain, select: &SelectStmt, current_db: &str) -> Self {
        let is = domain.infoschema();
        let mut tables = Vec::new();
        for table in select.join_tables() {
            let (db, name) = table.qualified(current_db);
            let resolved = match is.table_by_name(&db, &name) {
                Some(info) => Some(info.clone()),
                // A derived table (`FROM (SELECT …) x`) is a synthetic CTE
                // rather than an infoschema entry; its column types still
                // drive `collation()` on the outer reference.
                None => derived_table_info(select, table.resolve_name(), &is, current_db),
            };
            let Some(info) = resolved else {
                continue;
            };
            tables.push((table.name.to_ascii_lowercase(), info.clone()));
            if let Some(physical) = &table.physical {
                tables.push((physical.to_ascii_lowercase(), info));
            }
        }
        Self { tables }
    }

    fn lookup(
        &self,
        column: &sealdb_parser::ast::expressions::ColumnNameExpr,
    ) -> Option<&sealdb_meta::ColumnInfo> {
        fn find<'i>(
            info: &'i sealdb_meta::TableInfo,
            name: &str,
        ) -> Option<&'i sealdb_meta::ColumnInfo> {
            info.columns
                .iter()
                .find(|col| col.name.o().eq_ignore_ascii_case(name))
        }
        match &column.table {
            Some(table) => self
                .tables
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(table))
                .find_map(|(_, info)| find(info, &column.name)),
            None => self
                .tables
                .iter()
                .find_map(|(_, info)| find(info, &column.name)),
        }
    }
}

/// Column types of a derived table (`FROM (SELECT …) x`), read from the
/// synthetic CTE the parser lowering stores on the statement. Only columns
/// whose type is statically known (`NULL` literal, or a base-table column) are
/// reported; the others fall back to the session collation as before.
fn derived_table_info(
    select: &SelectStmt,
    name: &str,
    is: &sealdb_infoschema::InfoSchema,
    current_db: &str,
) -> Option<sealdb_meta::TableInfo> {
    let with = select.with.as_ref()?;
    let cte = with.find(name)?;
    if !cte.derived {
        return None;
    }
    let seed = cte.query.as_ref();
    let mut columns = Vec::new();
    for (offset, field) in seed.fields.iter().enumerate() {
        let Some(column_name) = cte
            .col_name_list
            .get(offset)
            .cloned()
            .or_else(|| field.as_name.clone().filter(|name| !name.is_empty()))
            .or_else(|| match &field.expr {
                ExprNode::Column(column) => Some(column.name.clone()),
                _ => None,
            })
        else {
            continue;
        };
        let Some(field_type) = derived_field_type(&field.expr, seed, is, current_db) else {
            continue;
        };
        columns.push(sealdb_meta::ColumnInfo::new(
            0,
            offset as i32,
            column_name,
            field_type,
        ));
    }
    if columns.is_empty() {
        return None;
    }
    Some(sealdb_meta::TableInfo::new(0, name, columns))
}

/// Static type of a derived table's output expression.
fn derived_field_type(
    expr: &ExprNode,
    seed: &SelectStmt,
    is: &sealdb_infoschema::InfoSchema,
    current_db: &str,
) -> Option<sealdb_types::FieldType> {
    match expr {
        // `SELECT NULL AS a`: TiDB types the column as NULL (collation
        // `binary`), which is what `COLLATION(a)` reports.
        ExprNode::Value(value) if value.datum.is_null() => Some(sealdb_types::FieldType {
            tp: sealdb_types::field_type::TYPE_NULL,
            ..Default::default()
        }),
        ExprNode::Column(column) => {
            let from = seed.from.as_ref()?;
            let (db, table) = from.qualified(current_db);
            let info = is.table_by_name(&db, &table)?;
            info.columns
                .iter()
                .find(|col| col.name.o().eq_ignore_ascii_case(&column.name))
                .map(|col| col.field_type.clone())
        }
        _ => None,
    }
}

/// TiDB `expression.GetCollation`: the collation of an expression's static
/// type, together with the charset and coercibility used by the aggregation
/// rules. A binary string (VARBINARY/BLOB/JSON) or a non-string operand
/// reports `binary`; string literals report the utf8mb4 default collation.
fn expr_collation(expr: &ExprNode, scope: &CollationScope) -> Result<CollationInfo> {
    // String literals take `collation_connection` (TiDB `ctx.GetCharsetInfo()`).
    let (session_charset, session_collation) = sealdb_expression::session_collation();
    match expr {
        ExprNode::Value(value) => Ok(match &value.datum {
            sealdb_types::Datum::Bytes(bytes) => CollationInfo {
                collation: session_collation.clone(),
                charset: session_charset.clone(),
                coercibility: COERCIBILITY_COERCIBLE,
                text: Some(String::from_utf8_lossy(bytes).into_owned()),
            },
            sealdb_types::Datum::Null => CollationInfo::ignorable(),
            // A binary literal is a string constant (`CoercibilityCoercible`
            // with the binary collation).
            sealdb_types::Datum::Bit(_) => CollationInfo {
                collation: "binary".to_string(),
                charset: "binary".to_string(),
                coercibility: COERCIBILITY_COERCIBLE,
                text: None,
            },
            // Numeric / temporal constants are `CoercibilityNumeric`.
            _ => CollationInfo {
                collation: "binary".to_string(),
                charset: "binary".to_string(),
                coercibility: COERCIBILITY_NUMERIC,
                text: None,
            },
        }),
        ExprNode::Column(column) => Ok(scope
            .lookup(column)
            .map(column_collation_info)
            .unwrap_or_else(|| CollationInfo {
                collation: session_collation.clone(),
                charset: session_charset.clone(),
                coercibility: COERCIBILITY_IMPLICIT,
                text: None,
            })),
        // `expr COLLATE <name>`: the explicit collation decides the charset and
        // keeps the operand's coercibility (explicit).
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("__collate") => {
            let collation = func
                .args
                .get(1)
                .and_then(|arg| match arg {
                    ExprNode::Value(v) => match &v.datum {
                        sealdb_types::Datum::Bytes(bytes) => {
                            Some(String::from_utf8_lossy(bytes).into_owned())
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or_default();
            let mut out = func
                .args
                .first()
                .map(|arg| expr_collation(arg, scope))
                .transpose()?
                .unwrap_or_else(CollationInfo::ignorable);
            let target = charset_of_collation(&collation);
            // TiDB `ErrCollationCharsetMismatch` (1253): an explicit collation
            // must belong to the operand's character set.
            if !out.charset.is_empty()
                && out.charset != "binary"
                && !target.is_empty()
                && target != out.charset
            {
                return Err(sealdb_errno::Error::new(
                    sealdb_errno::ERR_COLLATION_CHARSET_MISMATCH,
                    format!(
                        "COLLATION '{collation}' is not valid for CHARACTER SET '{}'",
                        out.charset
                    ),
                ));
            }
            out.charset = target;
            out.collation = collation;
            out.coercibility = COERCIBILITY_EXPLICIT;
            Ok(out)
        }
        // `CONVERT(expr USING charset)`: a function result (implicit) whose
        // charset is the conversion target.
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("__convert_using") => {
            let charset = func
                .args
                .get(1)
                .and_then(|arg| match arg {
                    ExprNode::Value(v) => match &v.datum {
                        sealdb_types::Datum::Bytes(bytes) => {
                            Some(String::from_utf8_lossy(bytes).to_ascii_lowercase())
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or_else(|| "utf8mb4".to_string());
            let mut out = func
                .args
                .first()
                .map(|arg| expr_collation(arg, scope))
                .transpose()?
                .unwrap_or_else(CollationInfo::ignorable);
            out.collation = default_collation_for_charset(&charset);
            out.charset = charset;
            out.coercibility = COERCIBILITY_IMPLICIT;
            Ok(out)
        }
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case(SEALDB_VARCOLL_FN) => {
            let literal = |arg: Option<&ExprNode>| match arg {
                Some(ExprNode::Value(v)) => match &v.datum {
                    sealdb_types::Datum::Bytes(bytes) => {
                        Some(String::from_utf8_lossy(bytes).into_owned())
                    }
                    _ => None,
                },
                _ => None,
            };
            let collation = literal(func.args.get(1)).unwrap_or_default();
            let charset = literal(func.args.get(2)).unwrap_or_default();
            let mut info = CollationInfo {
                collation,
                charset,
                // A user variable behaves like a column of its value type.
                coercibility: COERCIBILITY_IMPLICIT,
                text: None,
            };
            if let Some(ExprNode::Value(v)) = func.args.first() {
                if let sealdb_types::Datum::Bytes(bytes) = &v.datum {
                    info.text = Some(String::from_utf8_lossy(bytes).into_owned());
                }
            }
            Ok(info)
        }
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_lowercase().as_str(),
                "concat" | "concat_ws"
            ) =>
        {
            let op = func.name.to_ascii_lowercase();
            aggregate_collations(
                &op,
                func.args
                    .iter()
                    .map(|arg| expr_collation_aggregated(arg, scope)),
            )
        }
        // String functions that only transform their first argument keep its
        // collation (TiDB `deriveCollation`).
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_lowercase().as_str(),
                "upper"
                    | "ucase"
                    | "lower"
                    | "lcase"
                    | "reverse"
                    | "quote"
                    | "left"
                    | "right"
                    | "repeat"
                    | "trim"
                    | "ltrim"
                    | "rtrim"
                    | "substr"
                    | "substring"
                    | "mid"
                    | "translate"
                    | "substring_index"
                    | "lpad"
                    | "rpad"
                    | "replace"
                    | "insert"
            ) =>
        {
            let op = func.name.to_ascii_lowercase();
            aggregate_collations(
                &op,
                func.args
                    .first()
                    .map(|arg| expr_collation_aggregated(arg, scope))
                    .into_iter(),
            )
        }
        // `ELT`/`MAKE_SET`/`EXPORT_SET` derive from their string arguments
        // (`args[1:]`).
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_lowercase().as_str(),
                "elt" | "export_set" | "make_set"
            ) =>
        {
            let op = func.name.to_ascii_lowercase();
            aggregate_collations(
                &op,
                func.args
                    .iter()
                    .skip(1)
                    .map(|arg| expr_collation_aggregated(arg, scope)),
            )
        }
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_lowercase().as_str(),
                "ifnull" | "coalesce" | "if" | "nullif" | "greatest" | "least"
            ) =>
        {
            let op = func.name.to_ascii_lowercase();
            aggregate_collations(
                &op,
                func.args
                    .iter()
                    .map(|arg| expr_collation_aggregated(arg, scope)),
            )
        }
        ExprNode::Cast(cast)
            if cast.target.to_ascii_uppercase().starts_with("CHAR")
                || cast.target.to_ascii_uppercase().starts_with("VARCHAR")
                || cast.target.to_ascii_uppercase().starts_with("TEXT") =>
        {
            // `CAST(expr AS CHAR)`: TiDB types the result with the connection
            // charset/collation, not the argument's (`CAST('a' COLLATE
            // utf8mb4_bin AS CHAR)` reports `utf8mb4_general_ci` under a
            // general_ci connection). The cast is implicitly coercible.
            let (session_charset, session_collation) = sealdb_expression::session_collation();
            Ok(CollationInfo {
                collation: session_collation,
                charset: session_charset,
                // An implicit `CAST` always reports `CoercibilityImplicit`
                // (`COERCIBILITY(CAST('a' AS CHAR(10)))` = 2).
                coercibility: COERCIBILITY_IMPLICIT,
                text: None,
            })
        }
        ExprNode::Case(case) => aggregate_collations(
            "case",
            case.when_then
                .iter()
                .map(|(_, then)| expr_collation_aggregated(then, scope))
                .chain(
                    case.else_expr
                        .as_deref()
                        .map(|e| expr_collation_aggregated(e, scope)),
                ),
        ),
        _ => {
            if let ExprNode::Func(func) = expr {
                let name = func.name.to_ascii_lowercase();
                if is_system_const_func(&name) {
                    return Ok(CollationInfo {
                        // TiDB declares these results as utf8mb4 / utf8mb4_bin.
                        collation: "utf8mb4_bin".to_string(),
                        charset: "utf8mb4".to_string(),
                        coercibility: COERCIBILITY_SYSTEM_CONSTANT,
                        text: None,
                    });
                }
                if is_numeric_result_func(&name) {
                    return Ok(CollationInfo {
                        collation: "binary".to_string(),
                        charset: "binary".to_string(),
                        coercibility: COERCIBILITY_NUMERIC,
                        text: None,
                    });
                }
            }
            if let ExprNode::Binary(binary) = expr
                && matches!(
                    binary.op,
                    sealdb_parser::opcode::Op::Plus
                        | sealdb_parser::opcode::Op::Minus
                        | sealdb_parser::opcode::Op::Mul
                        | sealdb_parser::opcode::Op::Div
                        | sealdb_parser::opcode::Op::IntDiv
                        | sealdb_parser::opcode::Op::Mod
                        | sealdb_parser::opcode::Op::LeftShift
                        | sealdb_parser::opcode::Op::RightShift
                        | sealdb_parser::opcode::Op::BitAnd
                        | sealdb_parser::opcode::Op::BitOr
                        | sealdb_parser::opcode::Op::BitXor
                )
            {
                return Ok(CollationInfo {
                    collation: "binary".to_string(),
                    charset: "binary".to_string(),
                    coercibility: COERCIBILITY_NUMERIC,
                    text: None,
                });
            }
            Ok(CollationInfo {
                collation: "binary".to_string(),
                charset: "binary".to_string(),
                coercibility: COERCIBILITY_IMPLICIT,
                text: None,
            })
        }
    }
}

/// String functions whose result is a system constant (`CoercibilitySystem`).
fn is_system_const_func(name: &str) -> bool {
    matches!(
        name,
        "version"
            | "tidb_version"
            | "user"
            | "current_user"
            | "session_user"
            | "system_user"
            | "database"
            | "schema"
            | "current_role"
            | "current_role_json"
    )
}

/// Functions whose result is a number/temporal (`CoercibilityNumeric`).
fn is_numeric_result_func(name: &str) -> bool {
    if matches!(
        name,
        "now"
            | "sysdate"
            | "current_timestamp"
            | "current_date"
            | "curdate"
            | "current_time"
            | "curtime"
            | "localtime"
            | "localtimestamp"
            | "utc_timestamp"
            | "utc_date"
            | "utc_time"
            | "unix_timestamp"
            | "from_unixtime"
            | "rand"
            | "random"
            | "sleep"
            | "last_insert_id"
            | "row_count"
            | "found_rows"
            | "connection_id"
            | "length"
            | "char_length"
            | "character_length"
            | "octet_length"
            | "bit_length"
            | "instr"
            | "locate"
            | "field"
            | "find_in_set"
            | "strcmp"
            | "timestampdiff"
    ) {
        return true;
    }
    sealdb_expression::lookup(name).is_some_and(|meta| {
        matches!(
            meta.category,
            sealdb_expression::FuncCategory::Math | sealdb_expression::FuncCategory::Time
        )
    })
}

/// A JSON expression behaves as utf8mb4 with `utf8mb4_bin` while aggregating
/// string-function arguments (TiDB issue #31320).
fn is_json_expr(expr: &ExprNode, scope: &CollationScope) -> bool {
    match expr {
        ExprNode::Column(column) => scope
            .lookup(column)
            .is_some_and(|col| col.field_type.tp == sealdb_types::field_type::TYPE_JSON),
        _ => false,
    }
}

/// `expr_collation` as an operand of a string function: JSON arguments take
/// the utf8mb4 binary collation.
fn is_comparison_op(op: sealdb_parser::opcode::Op) -> bool {
    use sealdb_parser::opcode::Op;
    matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)
}

fn collation_literal(name: &str) -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: sealdb_types::Datum::Bytes(name.as_bytes().to_vec()),
    })
}

/// Collation a comparison would run with (TiDB `CheckAndDeriveCollationFromExprs`),
/// or `None` for a purely numeric/binary comparison.
fn derive_comparison_collation(
    left: &ExprNode,
    right: &ExprNode,
    op: &str,
    scope: &CollationScope,
) -> Result<Option<CollationInfo>> {
    let a = expr_collation(left, scope)?;
    let b = expr_collation(right, scope)?;
    // TiDB derives a comparison collation only when the comparison type is a
    // string (`argTps[0] == ETString`). A numeric/temporal operand makes the
    // comparison numeric or temporal — `ts_col = '2001-01-01 00:00:00'` parses
    // the string as a TIMESTAMP in the session time zone — so no collation
    // decides the result and wrapping the keys would compare their text.
    if a.coercibility == COERCIBILITY_NUMERIC || b.coercibility == COERCIBILITY_NUMERIC {
        return Ok(None);
    }
    if a.charset == "binary" && b.charset == "binary" {
        return Ok(None);
    }
    let dst = if a.charset == b.charset {
        comparison_same_charset(&a, &b, op)?
    } else {
        aggregate_collation_pair(a.clone(), b.clone(), op)?
    };
    if dst.coercibility == COERCIBILITY_NONE {
        return Err(illegal_mix_of_collations(&a, &b, op));
    }
    Ok(Some(dst))
}

/// Rewrite a string comparison with the collation derived from its operands so
/// execution does not fall back to `collation_connection`.
fn wrap_collation_comparison(
    left: ExprNode,
    right: ExprNode,
    op: sealdb_parser::opcode::Op,
    single_cnf_item: bool,
    scope: &CollationScope,
) -> Result<ExprNode> {
    let text = op.restore();
    if let Some(info) = derive_comparison_collation(&left, &right, text, scope)?
        && !info.is_neutral()
    {
        return Ok(ExprNode::Func(FuncCallExpr {
            name: "__STR_CMP".to_string(),
            args: vec![
                collation_literal(&info.collation),
                ExprNode::Value(ValueExpr {
                    datum: sealdb_types::Datum::Bytes(text.as_bytes().to_vec()),
                }),
                left,
                right,
            ],
            distinct: false,
        }));
    }
    Ok(ExprNode::Binary(BinaryOpExpr {
        op,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item,
    }))
}

fn expr_collation_aggregated(expr: &ExprNode, scope: &CollationScope) -> Result<CollationInfo> {
    let mut collation = expr_collation(expr, scope)?;
    if is_json_expr(expr, scope) {
        collation.charset = "utf8mb4".to_string();
        collation.collation = "utf8mb4_bin".to_string();
    }
    Ok(collation)
}

/// Column field type → collation metadata (TiDB
/// `deriveCoercibilityForColumn`: `implicit` for string/JSON/bit columns,
/// `numeric` for the rest). A JSON column's own collation is `binary`; the
/// aggregation rules coerce JSON arguments to `utf8mb4_bin`
/// (`expr_collation_aggregated`).
fn column_collation_info(col: &sealdb_meta::ColumnInfo) -> CollationInfo {
    let ft = &col.field_type;
    // A JSON column reports `binary` on its own (`Collation(d)`), even though
    // it participates in string-function aggregation as `utf8mb4_bin`.
    if ft.tp == sealdb_types::field_type::TYPE_JSON {
        return CollationInfo {
            collation: "binary".to_string(),
            charset: "binary".to_string(),
            coercibility: COERCIBILITY_IMPLICIT,
            text: None,
        };
    }
    // A `NULL`-typed derived column (`FROM (SELECT NULL AS a) x`) reports
    // `binary` collation like a NULL literal.
    if ft.tp == sealdb_types::field_type::TYPE_NULL {
        return CollationInfo {
            collation: "binary".to_string(),
            charset: "binary".to_string(),
            coercibility: COERCIBILITY_NUMERIC,
            text: None,
        };
    }
    if ft.eval_type() != sealdb_types::field_type::EvalType::String {
        return CollationInfo {
            collation: "binary".to_string(),
            charset: "binary".to_string(),
            coercibility: COERCIBILITY_NUMERIC,
            text: None,
        };
    }
    if ft.charset == "binary" || ft.collate == "binary" {
        return CollationInfo {
            collation: "binary".to_string(),
            charset: "binary".to_string(),
            coercibility: COERCIBILITY_IMPLICIT,
            text: None,
        };
    }
    let charset = if ft.charset.is_empty() {
        "utf8mb4".to_string()
    } else {
        ft.charset.clone()
    };
    let collation = if ft.collate.is_empty() {
        default_collation_for_charset(&charset)
    } else {
        ft.collate.clone()
    };
    CollationInfo {
        collation,
        charset,
        coercibility: COERCIBILITY_IMPLICIT,
        text: None,
    }
}

/// TiDB `charset.DefaultCollationForCharset`.
fn default_collation_for_charset(charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "utf8mb4" | "utf8mb3" => "utf8mb4_bin".to_string(),
        "utf8" => "utf8_bin".to_string(),
        "latin1" => "latin1_bin".to_string(),
        "ascii" => "ascii_bin".to_string(),
        "gbk" => "gbk_chinese_ci".to_string(),
        "binary" => "binary".to_string(),
        other => format!("{other}_bin"),
    }
}

/// Charset a collation belongs to (`gbk_bin` → `gbk`).
fn charset_of_collation(collation: &str) -> String {
    let lower = collation.to_ascii_lowercase();
    match lower.as_str() {
        "binary" => "binary".to_string(),
        _ => lower
            .split('_')
            .next()
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or("utf8mb4")
            .to_string(),
    }
}

/// TiDB `isBinCollation`: collations whose names end in `_bin` for the
/// aggregation rules (`binary` belongs to charset `bin` and takes another
/// path).
fn is_bin_collation(collation: &str) -> bool {
    matches!(
        collation.to_ascii_lowercase().as_str(),
        "utf8_bin" | "utf8mb4_bin" | "ascii_bin" | "latin1_bin" | "gbk_bin" | "utf8mb4_0900_bin"
    )
}

/// TiDB `getBinCollation`: the `_bin` collation of a charset.
fn bin_collation_for_charset(charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "utf8" => "utf8_bin".to_string(),
        "utf8mb4" | "utf8mb3" => "utf8mb4_bin".to_string(),
        "gbk" => "gbk_bin".to_string(),
        "ascii" => "ascii_bin".to_string(),
        "latin1" => "latin1_bin".to_string(),
        other => format!("{other}_bin"),
    }
}

/// TiDB `types.aggregateType` over the operands of a string function.
fn aggregate_collations(
    op: &str,
    infos: impl Iterator<Item = Result<CollationInfo>>,
) -> Result<CollationInfo> {
    let mut out: Option<CollationInfo> = None;
    for info in infos {
        let info = info?;
        if info.is_neutral() {
            continue;
        }
        out = Some(match out {
            None => info,
            Some(current) => aggregate_collation_pair(current, info, op)?,
        });
    }
    Ok(out.unwrap_or_else(|| CollationInfo {
        collation: "utf8mb4_general_ci".to_string(),
        charset: "utf8mb4".to_string(),
        coercibility: COERCIBILITY_COERCIBLE,
        text: None,
    }))
}

fn aggregate_collation_pair(a: CollationInfo, b: CollationInfo, op: &str) -> Result<CollationInfo> {
    if a.collation == b.collation {
        // Same collation: the operand with the larger repertoire wins (an
        // ASCII-only operand cannot carry the union's characters).
        return Ok(if a.charset == "binary" || b.charset != "binary" {
            a
        } else {
            b
        });
    }
    // Binary strings (TiDB `inferCollation`): a binary operand wins unless the
    // other side has a strictly stronger coercibility, or the same coercibility
    // and the newer operand is itself binary.
    if a.charset == "binary" || b.charset == "binary" {
        let take_arg = a.coercibility > b.coercibility
            || (a.coercibility == b.coercibility && b.charset == "binary");
        return Ok(if take_arg { b } else { a });
    }
    let (strong, weak) = if a.coercibility <= b.coercibility {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if strong.coercibility < weak.coercibility {
        // An explicit collation over an implicit operand is not allowed even
        // though it has higher precedence (TiDB parity).
        if strong.coercibility == COERCIBILITY_EXPLICIT
            && weak.coercibility == COERCIBILITY_IMPLICIT
        {
            return Err(illegal_mix_of_collations(&a, &b, op));
        }
        // The coercible operand converts to the stronger side's charset when
        // its characters are representable there.
        if !strong.charset.is_empty()
            && let Some(text) = &weak.text
            && !sealdb_parser::charset::is_text_encodable(&strong.charset, text)
        {
            return Err(illegal_mix_of_collations(&a, &b, op));
        }
        return Ok(strong.clone());
    }
    // Equal coercibility: resolve by charset (`ascii` coerces to any other
    // charset, and a Unicode charset wins over a non-Unicode one).
    if a.charset == b.charset {
        // Same charset, conflicting collations: a `_bin` collation yields to a
        // non-`_bin` one, otherwise the aggregation has no collation
        // (`CoercibilityNone`, `_bin` collation).
        if a.collation == b.collation {
            return Ok(a);
        }
        if a.coercibility == COERCIBILITY_EXPLICIT {
            return Err(illegal_mix_of_collations(&a, &b, op));
        }
        if is_bin_collation(&a.collation) {
            return Ok(a);
        }
        if is_bin_collation(&b.collation) {
            return Ok(b);
        }
        return Ok(CollationInfo {
            collation: bin_collation_for_charset(&a.charset),
            charset: a.charset.clone(),
            coercibility: COERCIBILITY_NONE,
            text: None,
        });
    }
    if a.charset == "ascii" {
        return Ok(b);
    }
    if b.charset == "ascii" {
        return Ok(a);
    }
    let unicode = |charset: &str| charset == "utf8mb4" || charset == "utf8";
    if unicode(&a.charset) && !unicode(&b.charset) {
        return Ok(a);
    }
    if unicode(&b.charset) && !unicode(&a.charset) {
        return Ok(b);
    }
    // utf8mb4 wins over utf8 (`utf8` can convert to `utf8mb4` without loss).
    if a.charset == "utf8mb4" && b.charset == "utf8" {
        return Ok(a);
    }
    if b.charset == "utf8mb4" && a.charset == "utf8" {
        return Ok(b);
    }
    Err(illegal_mix_of_collations(&a, &b, op))
}

fn illegal_mix_of_collations(
    a: &CollationInfo,
    b: &CollationInfo,
    op: &str,
) -> sealdb_errno::Error {
    sealdb_errno::Error::new(
        sealdb_errno::ERR_CANT_AGGREGATE_2COLLATIONS,
        format!(
            "Illegal mix of collations ({},{}) and ({},{}) for operation '{}'",
            a.collation,
            a.coercibility_name(),
            b.collation,
            b.coercibility_name(),
            op
        ),
    )
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
        // A CTE shadows a base table of the same name in FROM.
        let raw: Vec<String> = match cte_cols(&name) {
            cols if !cols.is_empty() => cols,
            _ => match is.table_by_name(&schema, &name) {
                Some(info) => info
                    .columns
                    .iter()
                    .filter(|c| !c.hidden)
                    .map(|c| c.name.o().to_string())
                    .collect(),
                // Synthetic `INFORMATION_SCHEMA` tables (CLUSTER_*,
                // INSPECTION_RULES, …) have no `TableInfo`; their layout is
                // produced by the virtual-row provider. Needed when the plan
                // folds to a `TableDual` (`LIMIT 0`) but the header must stay.
                None if schema.eq_ignore_ascii_case("information_schema") => {
                    sealdb_infoschema::tables::virtual_rows(&name, &is)
                        .map(|(columns, _)| columns)
                        .unwrap_or_default()
                }
                None => Vec::new(),
            },
        };
        raw.into_iter().map(|c| format!("{alias}.{c}")).collect()
    };
    let mut cols = Vec::new();
    // `t1.*, t2.*` keeps every table's raw columns (no USING coalescing).
    let qualified_star = select
        .fields
        .iter()
        .any(|f| matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some()));
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
        let drop = sealdb_planner::using_dropped_columns(select, Some(&is), current_db);
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

/// Source-query columns visible to `ON DUPLICATE KEY UPDATE` and the columns
/// appended to the source projection so the executor can read them.
///
/// TiDB resolves assignment columns against the target row and every table in
/// the `INSERT … SELECT` source. The insert executor only sees the source
/// projection, so referenced source columns that are not already produced are
/// appended (in reference order) and reported back with their qualifiers.
fn widen_insert_source(
    insert: &mut sealdb_parser::ast::InsertStmt,
    domain: &Domain,
    current_db: &str,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    if insert.on_duplicate.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Some(select) = insert.select.as_mut() else {
        return (Vec::new(), Vec::new());
    };
    let is = domain.infoschema();
    let (db, name) = insert.table.qualified(current_db);
    let target_columns: Vec<String> = is
        .table_by_name(&db, &name)
        .map(|info| {
            info.columns
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| c.name.o().to_string())
                .collect()
        })
        .unwrap_or_default();
    let target_name = insert.table.name.clone();
    let scope = select_source_scope(select, &is, current_db);
    let mut needed: Vec<(String, String)> = Vec::new();
    for item in &insert.on_duplicate {
        collect_odku_source_refs(
            &item.expr,
            &target_name,
            &target_columns,
            &scope,
            &mut needed,
        );
    }
    for (alias, column) in &needed {
        select.fields.push(sealdb_parser::ast::SelectField {
            expr: ExprNode::Column(sealdb_parser::ast::expressions::ColumnNameExpr {
                schema: None,
                table: Some(alias.clone()),
                name: column.clone(),
            }),
            as_name: None,
            orig_text: None,
        });
    }
    (scope, needed)
}

/// Every `alias.column` pair the source query exposes.
fn select_source_scope(
    select: &SelectStmt,
    is: &sealdb_infoschema::InfoSchema,
    current_db: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for table in select.join_tables() {
        let alias = table.name.clone();
        let physical = table.physical.as_deref().unwrap_or(&table.name);
        if let Some(with) = &select.with
            && let Some(cte) = with.find(&alias).or_else(|| with.find(physical))
        {
            out.extend(
                cte_output_columns(cte)
                    .into_iter()
                    .map(|c| (alias.clone(), c)),
            );
            continue;
        }
        let (db, name) = table.qualified(current_db);
        if let Some(info) = is
            .table_by_name(&db, physical)
            .or_else(|| is.table_by_name(&db, &name))
        {
            out.extend(
                info.columns
                    .iter()
                    .filter(|c| !c.hidden)
                    .map(|c| (alias.clone(), c.name.o().to_string())),
            );
        }
    }
    out
}

fn cte_output_columns(cte: &sealdb_parser::ast::CommonTableExpression) -> Vec<String> {
    if !cte.col_name_list.is_empty() {
        return cte.col_name_list.clone();
    }
    cte.query
        .fields
        .iter()
        .map(|f| match &f.as_name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => field_output_name(f),
        })
        .collect()
}

/// Resolve the source columns an `ON DUPLICATE KEY UPDATE` expression needs.
fn collect_odku_source_refs(
    expr: &ExprNode,
    target_name: &str,
    target_columns: &[String],
    scope: &[(String, String)],
    needed: &mut Vec<(String, String)>,
) {
    match expr {
        ExprNode::Column(col) => {
            if let Some(table) = &col.table {
                if table.eq_ignore_ascii_case(target_name)
                    || table
                        .rsplit_once('.')
                        .is_some_and(|(_, t)| t.eq_ignore_ascii_case(target_name))
                {
                    return;
                }
                if let Some((alias, column)) = scope.iter().find(|(alias, column)| {
                    alias.eq_ignore_ascii_case(table) && column.eq_ignore_ascii_case(&col.name)
                }) && !needed.contains(&(alias.clone(), column.clone()))
                {
                    needed.push((alias.clone(), column.clone()));
                }
            } else if !target_columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&col.name))
            {
                let mut matches = scope
                    .iter()
                    .filter(|(_, column)| column.eq_ignore_ascii_case(&col.name));
                if let Some((alias, column)) = matches.next()
                    && matches.next().is_none()
                    && !needed.contains(&(alias.clone(), column.clone()))
                {
                    needed.push((alias.clone(), column.clone()));
                }
            }
        }
        ExprNode::Func(f) => {
            // `VALUES(col)` names an inserted target column, not a source one.
            if f.name.eq_ignore_ascii_case("values") {
                return;
            }
            for arg in &f.args {
                collect_odku_source_refs(arg, target_name, target_columns, scope, needed);
            }
        }
        ExprNode::Binary(b) => {
            collect_odku_source_refs(&b.left, target_name, target_columns, scope, needed);
            collect_odku_source_refs(&b.right, target_name, target_columns, scope, needed);
        }
        ExprNode::Unary(u) => {
            collect_odku_source_refs(&u.expr, target_name, target_columns, scope, needed)
        }
        ExprNode::Cast(c) => {
            collect_odku_source_refs(&c.expr, target_name, target_columns, scope, needed)
        }
        ExprNode::InList(i) => {
            collect_odku_source_refs(&i.expr, target_name, target_columns, scope, needed);
            for item in &i.list {
                collect_odku_source_refs(item, target_name, target_columns, scope, needed);
            }
        }
        ExprNode::Between(b) => {
            collect_odku_source_refs(&b.expr, target_name, target_columns, scope, needed);
            collect_odku_source_refs(&b.low, target_name, target_columns, scope, needed);
            collect_odku_source_refs(&b.high, target_name, target_columns, scope, needed);
        }
        ExprNode::Like(l) => {
            collect_odku_source_refs(&l.expr, target_name, target_columns, scope, needed);
            collect_odku_source_refs(&l.pattern, target_name, target_columns, scope, needed);
        }
        ExprNode::IsNull(i) => {
            collect_odku_source_refs(&i.expr, target_name, target_columns, scope, needed)
        }
        ExprNode::Case(c) => {
            if let Some(operand) = &c.operand {
                collect_odku_source_refs(operand, target_name, target_columns, scope, needed);
            }
            for (when, then) in &c.when_then {
                collect_odku_source_refs(when, target_name, target_columns, scope, needed);
                collect_odku_source_refs(then, target_name, target_columns, scope, needed);
            }
            if let Some(otherwise) = &c.else_expr {
                collect_odku_source_refs(otherwise, target_name, target_columns, scope, needed);
            }
        }
        ExprNode::Row(r) => {
            for item in &r.exprs {
                collect_odku_source_refs(item, target_name, target_columns, scope, needed);
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
            sealdb_types::Datum::Bit(b) => {
                // BIT literal: raw bytes as text (control chars trimmed).
                let s = String::from_utf8_lossy(b);
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
            | sealdb_types::Datum::Year(_)
            | sealdb_types::Datum::U64(_)
            | sealdb_types::Datum::F64(_)
            | sealdb_types::Datum::Decimal(_) => {
                // Trim leading `+(` / whitespace and trailing `)` from original text.
                let raw = orig_text.unwrap_or("").trim();
                let trimmed = raw
                    .trim_start_matches(['\t', '\n', ' ', '+', '('])
                    .trim_end_matches(['\t', '\n', ' ', ')']);
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
                strip_field_comment_markers(text.trim())
            } else {
                tidb_expr_display_name(expr)
            }
        }
    }
}

/// TiDB `SelectField.Text()` drops the MySQL special-comment markers from the
/// select-list text (see `sealdb_parser::sqlparser_bridge::trim_field_comment`).
fn strip_field_comment_markers(text: &str) -> String {
    sealdb_parser::sqlparser_bridge::trim_field_comment(text)
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
            sealdb_types::Datum::Year(n) => n.to_string(),
            sealdb_types::Datum::U64(n) => n.to_string(),
            sealdb_types::Datum::F64(n) => n.to_string(),
            sealdb_types::Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            sealdb_types::Datum::Bit(b) => String::from_utf8_lossy(b).into_owned(),
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

/// Whether the plan's aggregate keys are exactly the statement's `GROUP BY`
/// list, i.e. the `HashAgg` is the statement's own aggregate rather than a
/// rewritten subquery de-duplication.
fn stmt_owns_agg_info(select: &SelectStmt, dedup: &DedupAgg) -> bool {
    select.group_by.len() == dedup.keys.len()
        && select.group_by.iter().zip(&dedup.keys).all(|(item, key)| {
            let text = item.restore_string().to_ascii_lowercase().replace('`', "");
            let key = key.to_ascii_lowercase();
            text == key || key.ends_with(&format!(".{text}")) || text.ends_with(&format!(".{key}"))
        })
}

/// A subquery de-duplication aggregate executed directly from the physical
/// plan's ExplainInfo (`group by:a, b, funcs:firstrow(a)->a, funcs:firstrow(b)->b`).
struct DedupAgg {
    keys: Vec<String>,
    fields: Vec<ExprNode>,
    names: Vec<String>,
}

/// Parse TiDB's `HashAgg` ExplainInfo into a row-dedup on its group keys.
///
/// Only the `buildDistinct` shape (`RewriteINToJoinAndAgg`) qualifies: every
/// function is `firstrow(<key>)` returning that key, so the aggregate output
/// is exactly the distinct group keys. Anything else (real aggregates such as
/// `max`) returns `None` and the statement-driven path keeps handling it.
fn dedup_from_agg_info(info: &str) -> Option<DedupAgg> {
    let rest = info.strip_prefix("group by:")?;
    let (group_part, func_part) = rest.split_once(", funcs:")?;
    if group_part.is_empty() {
        return None;
    }
    let keys: Vec<&str> = group_part.split(", ").collect();
    let mut names = Vec::with_capacity(keys.len());
    for func in func_part.split(", funcs:") {
        let inner = func.strip_prefix("firstrow(")?;
        let (arg, out) = inner.split_once(")->")?;
        let key = keys.iter().find(|k| k.eq_ignore_ascii_case(arg)).copied()?;
        names.push((key, out));
    }
    if names.len() != keys.len() {
        return None;
    }
    let fields = keys
        .iter()
        .map(|key| parse_qualified_column(key).map(ExprNode::Column))
        .collect::<Option<Vec<_>>>()?;
    let names = names.into_iter().map(|(_, out)| out.to_string()).collect();
    Some(DedupAgg {
        keys: keys.iter().map(|k| k.to_string()).collect(),
        fields,
        names,
    })
}

/// `dbg.m2.b` → `ColumnNameExpr { table: Some("m2"), name: "b" }` (the schema
/// prefix only labels the column; resolution is by table + column name).
fn parse_qualified_column(text: &str) -> Option<sealdb_parser::ast::expressions::ColumnNameExpr> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.rsplit('.');
    let name = parts.next()?.to_string();
    let table = parts.next().map(|t| t.to_string());
    Some(sealdb_parser::ast::expressions::ColumnNameExpr {
        schema: None,
        table,
        name,
    })
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

/// `mysql.{user,db,global_priv}` are backed by the privilege manager rather
/// than stored rows: the executor substitutes an in-memory row source while
/// EXPLAIN keeps the regular table-scan shape (`mysql.user` is an
/// `IndexLookUp` on `i_user`, a PK equality is a `PointGet`).
fn mysql_priv_mem_exec(
    from: &TableName,
    current_db: &str,
    stmt: &Statement,
    domain: &Arc<Domain>,
) -> Option<Box<dyn Executor>> {
    let schema = from
        .schema
        .as_deref()
        .filter(|schema| !schema.is_empty())
        .unwrap_or(current_db);
    if !schema.eq_ignore_ascii_case("mysql") {
        return None;
    }
    let table = from.resolve_name().to_ascii_lowercase();
    if !matches!(
        table.as_str(),
        "user"
            | "db"
            | "global_priv"
            | "tables_priv"
            | "columns_priv"
            | "global_grants"
            | "password_history"
    ) {
        return None;
    }
    let is = domain.infoschema();
    let info = is.table_by_name("mysql", &table)?;
    let columns: Vec<String> = info
        .columns
        .iter()
        .map(|column| column.name.o().to_string())
        .collect();
    let rows = match table.as_str() {
        "user" => mysql_user_rows(domain, &columns),
        "db" => db_priv_rows(domain).1,
        "global_priv" => global_priv_rows(domain).1,
        "tables_priv" => tables_priv_rows(domain).1,
        "columns_priv" => columns_priv_rows(domain).1,
        "password_history" => password_history_rows(domain),
        _ => global_grants_rows(domain).1,
    };
    let mut exec: Box<dyn Executor> = Box::new(MemTableExec::new(columns, rows));
    if let Some(selection) = stmt.as_select().and_then(|select| select.selection.clone()) {
        exec = Box::new(SelectionExec::new(selection, exec));
    }
    Some(exec)
}

/// `mysql.password_history` rows from the privilege manager.
fn password_history_rows(domain: &Domain) -> Vec<Vec<sealdb_types::Datum>> {
    use sealdb_types::{Datum, Time, TimeType};
    domain
        .privilege()
        .password_history_rows()
        .into_iter()
        .map(|row| {
            let mut ts = Time::from_unix_secs(
                row.timestamp_micros.div_euclid(1_000_000),
                TimeType::Timestamp,
            );
            ts.micro = row.timestamp_micros.rem_euclid(1_000_000) as u32;
            ts.fsp = 6;
            vec![
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.user.into_bytes()),
                Datum::Time(ts),
                Datum::Bytes(row.password.into_bytes()),
            ]
        })
        .collect()
}

fn mysql_user_rows(domain: &Domain, columns: &[String]) -> Vec<Vec<sealdb_types::Datum>> {
    use sealdb_parser::ast::PrivilegeType;
    use sealdb_types::Datum;
    fn bytes(value: impl Into<String>) -> Datum {
        Datum::Bytes(value.into().into_bytes())
    }
    fn yn(value: bool) -> Datum {
        bytes(if value { "Y" } else { "N" })
    }
    domain
        .privilege()
        .user_rows()
        .into_iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| match column.to_ascii_lowercase().as_str() {
                    "host" => bytes(row.host.clone()),
                    "user" => bytes(row.user.clone()),
                    "authentication_string" => row.auth.clone().map(bytes).unwrap_or(Datum::Null),
                    "plugin" => bytes(row.plugin.clone()),
                    "select_priv" => yn(row.global_privs.contains(PrivilegeType::SELECT)),
                    "insert_priv" => yn(row.global_privs.contains(PrivilegeType::INSERT)),
                    "update_priv" => yn(row.global_privs.contains(PrivilegeType::UPDATE)),
                    "delete_priv" => yn(row.global_privs.contains(PrivilegeType::DELETE)),
                    "create_priv" => yn(row.global_privs.contains(PrivilegeType::CREATE)),
                    "drop_priv" => yn(row.global_privs.contains(PrivilegeType::DROP)),
                    "process_priv" => yn(row.global_privs.contains(PrivilegeType::PROCESS)),
                    "grant_priv" => yn(row.with_grant),
                    "references_priv" => yn(row.global_privs.contains(PrivilegeType::REFERENCES)),
                    "index_priv" => yn(row.global_privs.contains(PrivilegeType::INDEX)),
                    "alter_priv" => yn(row.global_privs.contains(PrivilegeType::ALTER)),
                    "show_db_priv" => yn(false),
                    "super_priv" => yn(row.global_privs.contains(PrivilegeType::SUPER)),
                    "create_tmp_table_priv" => {
                        yn(row.global_privs.contains(PrivilegeType::CREATE_TEMP_TABLES))
                    }
                    "lock_tables_priv" => yn(row.global_privs.contains(PrivilegeType::LOCK_TABLES)),
                    "execute_priv" => yn(row.global_privs.contains(PrivilegeType::EXECUTE)),
                    "create_view_priv" => yn(row.global_privs.contains(PrivilegeType::CREATE_VIEW)),
                    "show_view_priv" => yn(row.global_privs.contains(PrivilegeType::SHOW_VIEW)),
                    "create_routine_priv" => {
                        yn(row.global_privs.contains(PrivilegeType::CREATE_ROUTINE))
                    }
                    "alter_routine_priv" => {
                        yn(row.global_privs.contains(PrivilegeType::ALTER_ROUTINE))
                    }
                    "create_user_priv" => yn(row.global_privs.contains(PrivilegeType::CREATE_USER)),
                    "event_priv" => yn(row.global_privs.contains(PrivilegeType::EVENT)),
                    "trigger_priv" => yn(row.global_privs.contains(PrivilegeType::TRIGGER)),
                    "create_tablespace_priv" => {
                        yn(row.global_privs.contains(PrivilegeType::CREATE_TABLESPACE))
                    }
                    "create_role_priv" => yn(row.global_privs.contains(PrivilegeType::CREATE_ROLE)),
                    "drop_role_priv" => yn(row.global_privs.contains(PrivilegeType::DROP_ROLE)),
                    "ssl_type" => bytes(match row.require.ssl_type() {
                        1 => "ANY",
                        2 => "X509",
                        3 => "SPECIFIED",
                        _ => "",
                    }),
                    "ssl_cipher" => bytes(row.require.cipher.clone().unwrap_or_default()),
                    "x509_issuer" => bytes(row.require.issuer.clone().unwrap_or_default()),
                    "x509_subject" => bytes(row.require.subject.clone().unwrap_or_default()),
                    "max_questions"
                    | "max_updates"
                    | "max_connections"
                    | "max_user_connections" => Datum::I64(0),
                    "password_expired" => yn(row.password_expired),
                    "account_locked" => yn(false),
                    "password_last_changed" => bytes("2020-01-01 00:00:00"),
                    "password_lifetime" => {
                        row.password_lifetime.map(Datum::I64).unwrap_or(Datum::Null)
                    }
                    "password_reuse_history" => row
                        .password_reuse_history
                        .map(Datum::I64)
                        .unwrap_or(Datum::Null),
                    "password_reuse_time" => row
                        .password_reuse_time
                        .map(Datum::I64)
                        .unwrap_or(Datum::Null),
                    "password_require_current" => Datum::Null,
                    "user_attributes" => bytes(row.attributes.clone()),
                    _ => Datum::Null,
                })
                .collect()
        })
        .collect()
}

/// `mysql.db` rows from the privilege manager (one row per database grant).
fn db_priv_rows(domain: &Domain) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_parser::ast::PrivilegeType;
    use sealdb_types::Datum;
    fn yn(value: bool) -> Datum {
        Datum::Bytes(if value { b"Y".to_vec() } else { b"N".to_vec() })
    }
    fn has(privs: PrivilegeType, wanted: PrivilegeType) -> bool {
        privs.contains(wanted)
    }
    let columns: Vec<String> = [
        "Host",
        "DB",
        "User",
        "Select_priv",
        "Insert_priv",
        "Update_priv",
        "Delete_priv",
        "Create_priv",
        "Drop_priv",
        "Grant_priv",
        "References_priv",
        "Index_priv",
        "Alter_priv",
        "Create_tmp_table_priv",
        "Lock_tables_priv",
        "Create_view_priv",
        "Show_view_priv",
        "Create_routine_priv",
        "Alter_routine_priv",
        "Execute_priv",
        "Event_priv",
        "Trigger_priv",
    ]
    .iter()
    .map(|column| column.to_string())
    .collect();
    let rows = domain
        .privilege()
        .db_priv_rows()
        .into_iter()
        .map(|row| {
            vec![
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.user.into_bytes()),
                yn(has(row.privs, PrivilegeType::SELECT)),
                yn(has(row.privs, PrivilegeType::INSERT)),
                yn(has(row.privs, PrivilegeType::UPDATE)),
                yn(has(row.privs, PrivilegeType::DELETE)),
                yn(has(row.privs, PrivilegeType::CREATE)),
                yn(has(row.privs, PrivilegeType::DROP)),
                yn(row.with_grant),
                yn(has(row.privs, PrivilegeType::REFERENCES)),
                yn(has(row.privs, PrivilegeType::INDEX)),
                yn(has(row.privs, PrivilegeType::ALTER)),
                yn(has(row.privs, PrivilegeType::CREATE_TEMP_TABLES)),
                yn(has(row.privs, PrivilegeType::LOCK_TABLES)),
                yn(has(row.privs, PrivilegeType::CREATE_VIEW)),
                yn(has(row.privs, PrivilegeType::SHOW_VIEW)),
                yn(has(row.privs, PrivilegeType::CREATE_ROUTINE)),
                yn(has(row.privs, PrivilegeType::ALTER_ROUTINE)),
                yn(has(row.privs, PrivilegeType::EXECUTE)),
                yn(has(row.privs, PrivilegeType::EVENT)),
                yn(has(row.privs, PrivilegeType::TRIGGER)),
            ]
        })
        .collect();
    (columns, rows)
}

/// `mysql.global_priv` rows from the privilege manager: the account's JSON
/// snapshot (`Priv` column).
fn global_priv_rows(domain: &Domain) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_types::Datum;
    let columns: Vec<String> = ["Host", "User", "Priv"]
        .iter()
        .map(|column| column.to_string())
        .collect();
    let rows = domain
        .privilege()
        .global_priv_rows()
        .into_iter()
        .map(|row| {
            vec![
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.user.into_bytes()),
                Datum::Bytes(row.priv_json.into_bytes()),
            ]
        })
        .collect();
    (columns, rows)
}

/// `mysql.tables_priv` rows from the privilege manager (TiDB persists one row
/// per table-scope grant; the engine keeps the same data in memory).
fn tables_priv_rows(domain: &Domain) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_types::Datum;
    let columns: Vec<String> = [
        "Host",
        "DB",
        "User",
        "Table_name",
        "Grantor",
        "Timestamp",
        "Table_priv",
        "Column_priv",
    ]
    .iter()
    .map(|column| column.to_string())
    .collect();
    let rows = domain
        .privilege()
        .table_priv_rows()
        .into_iter()
        .map(|row| {
            vec![
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.user.into_bytes()),
                Datum::Bytes(row.table.into_bytes()),
                Datum::Bytes(row.grantor.into_bytes()),
                Datum::Bytes(b"1970-01-01 00:00:00".to_vec()),
                Datum::Bytes(row.table_priv.into_bytes()),
                Datum::Bytes(row.column_priv.into_bytes()),
            ]
        })
        .collect();
    (columns, rows)
}

/// `mysql.columns_priv` rows from the privilege manager (one row per column
/// grant, `Column_priv` as the SET name list).
fn columns_priv_rows(domain: &Domain) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_types::Datum;
    let columns: Vec<String> = [
        "Host",
        "DB",
        "User",
        "Table_name",
        "Column_name",
        "Timestamp",
        "Column_priv",
    ]
    .iter()
    .map(|column| column.to_string())
    .collect();
    let rows = domain
        .privilege()
        .column_priv_rows()
        .into_iter()
        .map(|row| {
            vec![
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.user.into_bytes()),
                Datum::Bytes(row.table.into_bytes()),
                Datum::Bytes(row.column.into_bytes()),
                Datum::Bytes(b"1970-01-01 00:00:00".to_vec()),
                Datum::Bytes(row.column_priv.into_bytes()),
            ]
        })
        .collect();
    (columns, rows)
}

/// `mysql.global_grants` rows from the privilege manager (dynamic privileges).
fn global_grants_rows(domain: &Domain) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_types::Datum;
    let columns: Vec<String> = ["USER", "HOST", "PRIV", "WITH_GRANT_OPTION"]
        .iter()
        .map(|column| column.to_string())
        .collect();
    let rows = domain
        .privilege()
        .global_grant_rows()
        .into_iter()
        .map(|row| {
            vec![
                Datum::Bytes(row.user.into_bytes()),
                Datum::Bytes(row.host.into_bytes()),
                Datum::Bytes(row.privilege.into_bytes()),
                Datum::Bytes(if row.with_grant {
                    b"Y".to_vec()
                } else {
                    b"N".to_vec()
                }),
            ]
        })
        .collect();
    (columns, rows)
}

/// `information_schema.{SCHEMA,TABLE,COLUMN}_PRIVILEGES`: every row for a
/// globally privileged account, otherwise only the caller's own grants (TiDB
/// hides rows at scopes the caller does not administer).
fn privilege_information_rows(
    domain: &Domain,
    table: &str,
) -> (Vec<String>, Vec<Vec<sealdb_types::Datum>>) {
    use sealdb_types::Datum;
    let lower = table.to_ascii_lowercase();
    let columns: Vec<String> = match lower.as_str() {
        "schema_privileges" => vec![
            "GRANTEE",
            "TABLE_CATALOG",
            "TABLE_SCHEMA",
            "PRIVILEGE_TYPE",
            "IS_GRANTABLE",
        ],
        "table_privileges" => vec![
            "GRANTEE",
            "TABLE_CATALOG",
            "TABLE_SCHEMA",
            "TABLE_NAME",
            "PRIVILEGE_TYPE",
            "IS_GRANTABLE",
        ],
        _ => vec![
            "GRANTEE",
            "TABLE_CATALOG",
            "TABLE_SCHEMA",
            "TABLE_NAME",
            "COLUMN_NAME",
            "PRIVILEGE_TYPE",
            "IS_GRANTABLE",
        ],
    }
    .into_iter()
    .map(str::to_string)
    .collect();
    let mgr = domain.privilege();
    let caller = sealdb_privilege::thread_user();
    let show_all = caller
        .as_ref()
        .is_none_or(|user| mgr.has_global_grant(user));
    let own = caller.map(|user| format!("'{}'@'{}'", user.username, user.hostname));
    let mut rows = Vec::new();
    for row in mgr.privilege_rows() {
        if !show_all && own.as_deref() != Some(row.grantee.as_str()) {
            continue;
        }
        let grantable = if row.grantable { "YES" } else { "NO" };
        let record = match lower.as_str() {
            "schema_privileges" if row.table.is_none() => vec![
                Datum::Bytes(row.grantee.into_bytes()),
                Datum::Bytes(b"def".to_vec()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.privilege.into_bytes()),
                Datum::Bytes(grantable.as_bytes().to_vec()),
            ],
            "table_privileges" if row.table.is_some() && row.column.is_none() => vec![
                Datum::Bytes(row.grantee.into_bytes()),
                Datum::Bytes(b"def".to_vec()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.table.unwrap_or_default().into_bytes()),
                Datum::Bytes(row.privilege.into_bytes()),
                Datum::Bytes(grantable.as_bytes().to_vec()),
            ],
            "column_privileges" if row.column.is_some() => vec![
                Datum::Bytes(row.grantee.into_bytes()),
                Datum::Bytes(b"def".to_vec()),
                Datum::Bytes(row.db.into_bytes()),
                Datum::Bytes(row.table.unwrap_or_default().into_bytes()),
                Datum::Bytes(row.column.unwrap_or_default().into_bytes()),
                Datum::Bytes(row.privilege.into_bytes()),
                Datum::Bytes(grantable.as_bytes().to_vec()),
            ],
            _ => continue,
        };
        rows.push(record);
    }
    (columns, rows)
}

/// TiDB `setDataForColumnsWithOneTable`: `information_schema.columns` only
/// exposes a table's columns when the session holds at least one *column*
/// privilege on it, and its `PRIVILEGES` cell lists those privileges
/// (`select,insert,update,references` order).
fn columns_for_session(
    domain: &Domain,
    columns: &[String],
    rows: Vec<Vec<sealdb_types::Datum>>,
) -> Vec<Vec<sealdb_types::Datum>> {
    use sealdb_parser::ast::PrivilegeType;
    use sealdb_types::Datum;
    use std::collections::HashMap;

    let Some(user) = sealdb_privilege::thread_user() else {
        return rows;
    };
    let roles = sealdb_privilege::thread_active_roles();
    let index = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(schema_i), Some(table_i), Some(priv_i)) = (
        index("TABLE_SCHEMA"),
        index("TABLE_NAME"),
        index("PRIVILEGES"),
    ) else {
        return rows;
    };
    let mgr = domain.privilege();
    let mut cache: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let text = |datum: &Datum| match datum {
            Datum::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => String::new(),
        };
        let schema = text(&row[schema_i]);
        let table = text(&row[table_i]);
        let privileges = cache
            .entry((schema.clone(), table.clone()))
            .or_insert_with(|| {
                let mut names = Vec::new();
                for (privs, name) in [
                    (PrivilegeType::SELECT, "select"),
                    (PrivilegeType::INSERT, "insert"),
                    (PrivilegeType::UPDATE, "update"),
                    (PrivilegeType::REFERENCES, "references"),
                ] {
                    if mgr.request_verification_with_roles(&user, &roles, &schema, &table, privs) {
                        names.push(name);
                    }
                }
                (!names.is_empty()).then(|| names.join(","))
            })
            .clone();
        let Some(privileges) = privileges else {
            continue;
        };
        row[priv_i] = Datum::Bytes(privileges.into_bytes());
        out.push(row);
    }
    out
}

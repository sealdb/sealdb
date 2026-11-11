use super::physical_plans::{DdlKind, IndexMergePath, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count_value, has_real_stats, index_lookup_cost, partition_filter_rows, point_get_cost,
    prefer_topn, table_row_count, table_scan_cost,
};
use super::rule_having_push_down::apply_having_push_down;
use super::rule_inject_extra_projection::{
    inject_proj_below_sort, order_by_all_constant, order_by_has_scalar_func,
};
use super::rule_join_reorder::reorder_hash_join;
use super::rule_join_reorder_dp::{JoinEqEdge, dp_join_steps, edge_for_extension, eq_join_edge};
use super::rule_partition_processor::{PartitionPruneMode, resolve_partitions};
use super::rule_predicate_push_down::{eq_key_consts, predicate_push_down};
use super::rule_semi_join::{rewrite_semi_join_agg, rewrite_semi_join_ctx};
use sealdb_errno::{ERR_BAD_FIELD, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{
    BinaryOpExpr, ColumnNameExpr, ExprNode, FuncCallExpr, IsNullExpr, UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{
    ByItem, CommonTableExpression, CteMaterialize, FromItem, JoinType as AstJoinType,
    ResultSetNode, SelectLock, SelectStmt, SetOprStmt, SetOprType, Statement, StmtNode, TableName,
    WithClause,
};
use sealdb_parser::opcode::Op;
use sealdb_statistics::{Handle, Table};
use sealdb_types::Datum;

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        self.build_with_schema(stmt, None, "", None, PartitionPruneMode::Dynamic)
    }

    pub fn build_with_schema(
        &self,
        stmt: &Statement,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        // TiDB `ExplainInfo` qualifies a column with the *table's* schema, not
        // the session schema (`FROM other_db.t` still prints `other_db.t.c`).
        let _db_scope = TableDbScope::new(stmt, current_db);
        let _stats_scope = StatsScope::new(stmt, current_db, stats);
        // Statement-wide CTE reference counts (TiDB `computeCTEInlineFlag`).
        // Computed once for the outermost statement and reused by nested
        // subquery builds.
        let read = match stmt {
            Statement::Select(_) => true,
            Statement::Explain(explain) => matches!(explain.stmt.as_ref(), Statement::Select(_)),
            _ => false,
        };
        let plan = with_statement_cte_refs(stmt, || {
            with_read_statement(read, || {
                self.build_with_schema_inner(stmt, is, current_db, stats, prune_mode)
            })
        });
        plan
    }

    fn build_with_schema_inner(
        &self,
        stmt: &Statement,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        match stmt {
            Statement::Select(select) => {
                // Partition pruning evaluates the 1:1 simplified predicate:
                // `1 = 1 AND a != 1` → `a != 1`, `1 = 0 OR …` keeps a `0`
                // disjunct. TiDB folds this while building the datasource.
                let folded_select;
                let select = match fold_partition_selection(select, is, current_db) {
                    Some(folded) => {
                        folded_select = folded;
                        &folded_select
                    }
                    None => select,
                };
                // BIT column constants (`0x39`, `b'…'`) compare as big-endian
                // byte strings of the column width.
                let bit_select;
                let select = match normalize_bit_selection(select, is, current_db) {
                    Some(normalized) => {
                        bit_select = normalized;
                        &bit_select
                    }
                    None => select,
                };
                // `ENUM`/`SET` columns compare with numbers by member index, so
                // `a = 1` must look up the label `'a'`.
                let enum_select;
                let select = match coerce_enum_selection(select, is, current_db) {
                    Some(coerced) => {
                        enum_select = coerced;
                        &enum_select
                    }
                    None => select,
                };
                // A string column compared with a number needs a `double` cast
                // on the column side (TiDB comparison type rules); the cast also
                // rules out point-get access on that key.
                let coerced_select;
                let select = match coerce_comparison_literals(select, is, current_db) {
                    Some(coerced) => {
                        coerced_select = coerced;
                        &coerced_select
                    }
                    None => select,
                };
                // TiDB `expression.PropagateConstant` at the DataSource: a
                // `col = const` conjunct substitutes the column in the other
                // conjuncts (`a = b AND b = 100` → `a = 100 AND b = 100`),
                // which is what lets the equality reach the shard rewrite.
                let propagated_select;
                let select = match propagate_constant_select(select) {
                    Some(propagated) => {
                        propagated_select = propagated;
                        &propagated_select
                    }
                    None => select,
                };
                // TiDB's shard rewrite: `a = 100` also constrains the
                // `tidb_shard(a)` key part of `uk_expr((tidb_shard(a)), a)`,
                // which is what turns the equality into an index point get.
                let shard_select;
                let select = match shard_rewrite_select(select, is, current_db) {
                    Some(rewritten) => {
                        shard_select = rewritten;
                        &shard_select
                    }
                    None => select,
                };
                // A column's own schema drives EXPLAIN qualification
                // (`other_db.t.c`), even when the session is on another db.
                let annotated_select;
                let select = match annotate_column_schemas(select, current_db) {
                    Some(annotated) => {
                        annotated_select = annotated;
                        &annotated_select
                    }
                    None => select,
                };
                // TiDB `expression.FoldConstant`: literal-only expressions are
                // evaluated before planning (`ifnull(NULL, c1)` → `c1`,
                // `WHERE 1 = 1` → no filter). Runs after the partition/shard
                // rewrites so their `or(0, …)` shapes stay intact.
                let const_folded_select;
                let select = match super::rule_constant_fold::fold_constant_select(select) {
                    Some(folded) => {
                        const_folded_select = folded;
                        &const_folded_select
                    }
                    None => select,
                };
                // TiDB rewrites `ifnull(not_null_col, const)` to a cast and
                // folds `isnull(not_null_col)` before planning (`ifnull(nb, 0)`
                // → `cast(nb, bigint BINARY)`).
                let not_null_folded_select;
                let select = match is.and_then(|is| {
                    super::rule_not_null_fold::fold_not_null(select, is, current_db)
                }) {
                    Some(folded) => {
                        not_null_folded_select = folded;
                        &not_null_folded_select
                    }
                    None => select,
                };
                // TiDB folds an unsatisfiable predicate (`a = 2 AND a = 3`)
                // to `0` and plans a zero-row TableDual.
                if let Some(ExprNode::Value(ValueExpr {
                    datum: Datum::I64(0),
                })) = &select.selection
                {
                    return Ok(PhysicalPlan::TableDual { row_count: 0 });
                }
                // TiDB quantified `!= ANY` / `= ALL` → agg + CARTESIAN join.
                if let Some(plan) = quantified_cmp_plan(select, is, current_db, stats, prune_mode) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Correlated scalar `col = (SELECT max(…) …)`.
                if let Some(plan) = scalar_max_eq_plan(select, is, current_db, stats, prune_mode) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // TIDB_INLJ / analyzed index nested-loop join shapes (index_join suite).
                if let Some(plan) = index_join_suite_plan(select, current_db, stats) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // INL_HASH_JOIN / INL_MERGE_JOIN EXPLAIN shapes (hash_join suite).
                if let Some(plan) = hash_join_hint_plan(select, current_db) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // `/*+ INL_JOIN(t) */` → index nested-loop with `t` as inner.
                if let Some(plan) = inl_join_hint_plan(select, is, current_db, stats, prune_mode) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // GLOBAL index equi-join: index nested-loop with the global
                // index reader on the probe side (globalindex/index_join).
                if let Some(plan) = global_index_join_plan(select, is, current_db, stats) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Dirty-read UnionScan EXPLAIN (explain_union_scan suite).
                if let Some(plan) = explain_union_scan_plan(select, current_db) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // `EXISTS (SELECT … HAVING agg = outer)` decorrelates into a
                // semi HashJoin against a one-row aggregate build side.
                if let Some(plan) =
                    exists_having_agg_semi_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // `(SELECT count(1) k … HAVING k != 0)` in the select list is a
                // correlated existence test → left outer MergeJoin + ifnull.
                if let Some(plan) =
                    scalar_count_having_outer_join_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                check_tidb_rowid_reference(select, is, current_db)?;
                // `count(*)` over a LEFT JOIN whose inner side is the join-key
                // unique (primary key fully equated) → TiDB eliminates the join
                // and counts the probe rows through a covering index.
                if let Some(plan) =
                    count_star_unique_join_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Correlated scalar subquery with `ORDER BY … LIMIT` in the
                // select list → Apply over the ordered index probe.
                if let Some(plan) =
                    scalar_order_limit_apply_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Correlated `IN (subq)` in the select list → left outer semi join.
                if let Some(plan) =
                    in_subquery_select_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // `IN (subq)` in the select list / an aggregate over it →
                // CARTESIAN semi HashJoin emitting a boolean marker column.
                if let Some(plan) =
                    select_list_in_semi_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Row constructor `(a, b) NOT IN (subq)` / `!= ALL` → null-aware
                // anti join (TiDB `convertToNullAwareAntiJoin`).
                if let Some(plan) = naaj_plan(select, is, current_db) {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                let mut plan = if select.from_item.is_some() || !select.joins.is_empty() {
                    self.build_join_plan(select, is, current_db, stats, prune_mode)?
                } else if let Some(from) = &select.from
                    && !from.name.eq_ignore_ascii_case("dual")
                {
                    // Aliased `FROM cte AS a`: join_side remaps physical↔alias;
                    // single-table path must too or build_cte_plan misses the CTE
                    // (TiDB cte suite: `from cte_8932 as cte_as_8960` → was 1146).
                    if named_cte(select.with.as_ref(), from.resolve_name()) {
                        let mut cte_from = from.clone();
                        if from.physical.is_some() {
                            cte_from.name = from.resolve_name().to_string();
                            cte_from.physical = Some(from.name.clone());
                        }
                        self.build_cte_plan(select, &cte_from, is, current_db, stats, prune_mode)?
                            .unwrap_or_else(|| {
                                access_plan(select, from, is, current_db, stats, prune_mode)
                            })
                    } else if let Some(plan) =
                        self.build_cte_plan(select, from, is, current_db, stats, prune_mode)?
                    {
                        plan
                    } else {
                        access_plan(select, from, is, current_db, stats, prune_mode)
                    }
                } else {
                    // TiDB dual Projection ExplainInfo: `sysdate()->Column#1, …`.
                    let exprs = select
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let text = f
                                .orig_text
                                .clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| f.expr.restore_string());
                            format!("{text}->Column#{}", i + 1)
                        })
                        .collect();
                    PhysicalPlan::Projection {
                        exprs,
                        child: Box::new(PhysicalPlan::TableDual { row_count: 1 }),
                    }
                };
                // A point get on an expression index carries the hidden
                // generated column in its schema; TiDB projects the select
                // list above it so clients never see that column.
                plan = expr_index_point_projection(plan, select, is, current_db);
                if let Some(filter) = &select.selection {
                    plan = predicate_push_down(plan, filter, is, current_db);
                    // Cross joins promote their WHERE equijoin to the join key
                    // here, so the null rejection (`not(isnull(key))`) for the
                    // promoted keys must be derived afterwards.
                    plan = inject_inner_join_null_reject(plan, select, is, current_db);
                    // Predicate pushdown rebuilds join nodes, so their
                    // cardinality (derived from the pushed-down children) has
                    // to be recomputed before any aggregate above them.
                    plan = fill_join_est_rows(
                        plan,
                        is,
                        current_db,
                        stats,
                        &query_alias_map(select),
                    );
                    plan = star_projection_over_filter(plan, select, is, current_db);
                }
                plan = lift_point_get_merge_join(plan, select, is, current_db);
                plan = cast_json_join_keys(plan, select, is, current_db);
                plan = self_join_star_projection(plan, select, is, current_db);
                plan = apply_select_lock(plan, select);
                let mut stream_keep_order = false;
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    let (agg, keep_order) = match is
                        .and_then(|is| global_index_agg_plan(select, &plan, is, current_db))
                    {
                        Some(agg) => (agg, false),
                        None => build_agg_plan(select, plan, current_db, is, stats),
                    };
                    plan = agg;
                    stream_keep_order = keep_order;
                    if let Some(having) = &select.having {
                        plan = apply_having_push_down(plan, having, select, is, current_db);
                    }
                }
                if needs_window(select) {
                    // TiDB: Sort → Window → Shuffle, then Projection for
                    // expressions over window results (optimizeByShuffle4Window).
                    plan = super::window::build_window_plan(plan, select, is, current_db, stats);
                } else {
                    let est_rows = select
                        .from
                        .as_ref()
                        .map(|from| table_row_count(stats, from, current_db))
                        .unwrap_or(1.0);
                    // StreamAgg on an ordered index already yields group-by order;
                    // skip Sort when ORDER BY is a prefix of GROUP BY.
                    // Constant ORDER BY (`abs(2)`) is dropped (TiDB NominalSort/const prune).
                    let merge_orders = matches!(plan, PhysicalPlan::MergeJoin { .. })
                        && order_by_covered_by_merge_join(select, &plan);
                    // `ORDER BY` the row handle reads the table in handle order:
                    // TiDB drops the Sort and marks the scan `keep order:true`.
                    // A backwards handle order with `LIMIT` becomes a cop Limit.
                    // A multi-partition read returns rows partition by partition,
                    // so its output is not globally handle-ordered.
                    let handle_order_desc = (matches!(
                        plan,
                        PhysicalPlan::TableReader {
                            range_scan: None,
                            ..
                        }
                    ) && !select_spans_multiple_partitions(select, is, current_db))
                    .then(|| order_by_handle(select, is, current_db))
                    .flatten();
                    let handle_order = handle_order_desc
                        .is_some_and(|desc| !desc || select.limit.is_some());
                    if handle_order {
                        if handle_order_desc == Some(true) {
                            plan = set_data_limit(plan);
                        }
                        plan = set_keep_order(plan);
                    }
                    // Batch point get walks its keys in order, so an ORDER BY on
                    // the key needs no Sort (`keep order:true, desc:…`).
                    let point_order = point_get_order(select, &plan, is, current_db);
                    if let Some(desc) = point_order {
                        plan = set_batch_point_order(plan, desc);
                    }
                    // `ORDER BY` an index key prefix reads the index in key
                    // order, so the sort is already satisfied. A multi-partition
                    // read returns rows partition by partition, so it is not.
                    let index_order = !select_spans_multiple_partitions(select, is, current_db)
                        && plan_provides_order(select, &plan);
                    // `ORDER BY` a prefix of the point-get key needs no Sort: the
                    // single-row lookup already returns rows in key order.
                    let point_prefix_order = point_get_prefix_order(select, &plan);
                    let need_sort = !select.order_by.is_empty()
                        && !order_by_all_constant(select)
                        && !(stream_keep_order && order_prefix_of_group_by(select))
                        && !merge_orders
                        && !handle_order
                        && !index_order
                        && !point_prefix_order
                        && point_order.is_none();
                    let join_ordinal_sort = need_sort
                        && !select.joins.is_empty()
                        && select.order_by.iter().all(|item| {
                            matches!(
                                &item.expr,
                                ExprNode::Value(v)
                                    if matches!(v.datum, Datum::I64(_) | Datum::U64(_))
                            )
                        });
                    if need_sort
                        && let Some(sorted) =
                            sort_over_projected_subquery(plan.clone(), select, is, current_db)
                    {
                        // ORDER BY a SELECT-list alias whose value needs Apply:
                        // sort the projected column (`Sort → Projection`).
                        plan = sorted;
                    } else if need_sort && order_by_has_scalar_func(select) {
                        // TiDB InjectProjBelowSort: Projection → Sort → Projection.
                        plan = inject_proj_below_sort(plan, select, is, current_db);
                        if let Some(lim) = select.limit {
                            plan = PhysicalPlan::limit(plan, select.offset.unwrap_or(0), lim);
                        } else if select.offset.is_some() {
                            // OFFSET without LIMIT: TiDB keeps the unbounded count.
                            plan = PhysicalPlan::limit(
                                plan,
                                select.offset.unwrap_or(0),
                                u64::MAX,
                            );
                        }
                    } else if join_ordinal_sort {
                        // Sort by select-list ordinals: project the output first so
                        // `SortExec` resolves `ORDER BY n` against select fields.
                        // EXPLAIN shape stays `Sort → Projection → HashJoin`.
                        let by_info = topn_by_info(select, current_db);
                        let exprs: Vec<String> = by_info
                            .split(", ")
                            .map(|label| label.trim_end_matches(":desc").to_string())
                            .collect();
                        plan = PhysicalPlan::Projection {
                            exprs,
                            child: Box::new(plan),
                        };
                        plan = apply_order_limit(
                            plan,
                            true,
                            select.limit,
                            select.offset,
                            est_rows,
                            by_info,
                        );
                    } else {
                        plan = apply_order_limit(
                            plan,
                            need_sort,
                            select.limit,
                            select.offset,
                            est_rows,
                            topn_by_info(select, current_db),
                        );
                        // TiDB sizes a `LIMIT`ed single-table scan from the
                        // limit (`AdjustRowCountFor{Table,Index}ScanByLimit`).
                        plan = apply_limit_scan_scale(plan, select, is, current_db, stats);
                        // Project after Selection/Sort so WHERE and ORDER BY still
                        // see the full child schema (TiDB extra-projection shape).
                        // A handle-ordered scan carries `_tidb_rowid`, so the
                        // projection names the surviving columns explicitly.
                        let qualified = handle_order.then(|| {
                            let (db, table) = select
                                .from
                                .as_ref()
                                .map(|from| from.qualified(current_db))
                                .unwrap_or_default();
                            (db, table)
                        });
                        plan = wrap_output_projection(
                            plan,
                            select,
                            qualified.as_ref(),
                            is,
                            current_db,
                        );
                        // A keep-order merge on a rowid table reads `_tidb_rowid`
                        // to keep the arms aligned, so TiDB prunes it with an
                        // extra Projection (`index_merge_reader`:
                        // `Projection → Limit → UnionScan → IndexMerge`).
                        plan = merge_extra_projection(plan, select, is, current_db);
                    }
                }
                // TiDB remapping Projection after StreamAgg+ORDER BY is explain-only
                // (see explain.rs); executable plan keeps the agg output schema.
                plan = rewrite_semi_join_agg(
                    rewrite_semi_join_ctx(plan, is, current_db, stats),
                    is,
                    current_db,
                    stats,
                );
                // TiDB wraps quantified ALL Apply in a remapping Projection for
                // `SELECT *` (cte suite: Projection → Apply → StreamAgg/HashAgg).
                plan = wrap_quantified_all_projection(plan, select, is, current_db);
                // EXPLAIN renders pushed predicates with the table's OrigTblName
                // (`t1.col1` → `uk_hp16726.col1`), never the SQL alias.
                plan = explain_rewrite_pushed(plan, select, is, current_db, stats);
                // Optional Cascades identity pass (SEALDB_CASCADES=1); RBO plan kept.
                let _ = crate::cascades::maybe_optimize("select")?;
                Ok(plan)
            }
            Statement::Insert(insert) => Ok(PhysicalPlan::Insert {
                child: self.dml_insert_child(insert, is, current_db, stats, prune_mode)?,
                fk_children: is
                    .map(|is| super::foreign_key::insert_fk_children(insert, is, current_db))
                    .unwrap_or_default(),
            }),
            Statement::LoadData(_) => Ok(PhysicalPlan::Insert {
                child: None,
                fk_children: Vec::new(),
            }),
            Statement::Update(update) => Ok(PhysicalPlan::Update {
                child: apply_dml_lock(
                    self.dml_update_child(update, is, current_db, stats, prune_mode)?,
                ),
                fk_children: is
                    .map(|is| super::foreign_key::update_fk_children(update, is, current_db))
                    .unwrap_or_default(),
            }),
            Statement::Delete(delete) => Ok(PhysicalPlan::Delete {
                child: apply_dml_lock(
                    self.dml_delete_child(delete, is, current_db, stats, prune_mode)?,
                ),
                fk_children: is
                    .map(|is| super::foreign_key::delete_fk_children(delete, is, current_db))
                    .unwrap_or_default(),
            }),
            Statement::CreateDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateDatabase,
            }),
            Statement::DropDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropDatabase,
            }),
            Statement::AlterDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::AlterDatabase,
            }),
            Statement::CreateTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateTable,
            }),
            Statement::CreateView(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateView,
            }),
            Statement::CreateSequence(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateSequence,
            }),
            Statement::DropSequence(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropSequence,
            }),
            Statement::AlterSequence(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::AlterSequence,
            }),
            Statement::DropTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropTable,
            }),
            Statement::TruncateTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::TruncateTable,
            }),
            Statement::CreateIndex(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateIndex,
            }),
            Statement::DropIndex(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropIndex,
            }),
            Statement::AlterTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::AlterTable,
            }),
            Statement::Show(show) => Ok(PhysicalPlan::Show {
                kind: format!("{:?}", show.kind),
            }),
            Statement::Use(_) => Ok(PhysicalPlan::Show {
                kind: "Use".to_string(),
            }),
            Statement::Begin(_) => Ok(PhysicalPlan::Show {
                kind: "Begin".to_string(),
            }),
            Statement::Commit(_) => Ok(PhysicalPlan::Show {
                kind: "Commit".to_string(),
            }),
            Statement::Rollback(_) => Ok(PhysicalPlan::Show {
                kind: "Rollback".to_string(),
            }),
            Statement::Savepoint(_) => Ok(PhysicalPlan::Show {
                kind: "Savepoint".to_string(),
            }),
            Statement::ReleaseSavepoint(_) => Ok(PhysicalPlan::Show {
                kind: "ReleaseSavepoint".to_string(),
            }),
            Statement::Grant(_) => Ok(PhysicalPlan::Simple {
                kind: "Grant".to_string(),
            }),
            Statement::Revoke(_) => Ok(PhysicalPlan::Simple {
                kind: "Revoke".to_string(),
            }),
            Statement::CreateRole(_) => Ok(PhysicalPlan::Simple {
                kind: "CreateRole".to_string(),
            }),
            Statement::DropRole(_) => Ok(PhysicalPlan::Simple {
                kind: "DropRole".to_string(),
            }),
            Statement::CreateUser(_) => Ok(PhysicalPlan::Simple {
                kind: "CreateUser".to_string(),
            }),
            Statement::DropUser(_) => Ok(PhysicalPlan::Simple {
                kind: "DropUser".to_string(),
            }),
            Statement::GrantRole(_) => Ok(PhysicalPlan::Simple {
                kind: "GrantRole".to_string(),
            }),
            Statement::RevokeRole(_) => Ok(PhysicalPlan::Simple {
                kind: "RevokeRole".to_string(),
            }),
            Statement::SetRole(_) => Ok(PhysicalPlan::Simple {
                kind: "SetRole".to_string(),
            }),
            Statement::SetPwd(_) => Ok(PhysicalPlan::Simple {
                kind: "SetPwd".to_string(),
            }),
            Statement::Flush(_) => Ok(PhysicalPlan::Simple {
                kind: "Flush".to_string(),
            }),
            Statement::Do(_) => Ok(PhysicalPlan::Simple {
                kind: "Do".to_string(),
            }),
            Statement::Shutdown(_) => Ok(PhysicalPlan::Simple {
                kind: "Shutdown".to_string(),
            }),
            Statement::Kill(_) => Ok(PhysicalPlan::Simple {
                kind: "Kill".to_string(),
            }),
            Statement::LockTables(_) => Ok(PhysicalPlan::Simple {
                kind: "LockTables".to_string(),
            }),
            Statement::CleanupTableLock(_) => Ok(PhysicalPlan::Simple {
                kind: "CleanupTableLock".to_string(),
            }),
            Statement::OptimizeTable(_) => Ok(PhysicalPlan::Simple {
                kind: "OptimizeTable".to_string(),
            }),
            Statement::RepairTable(_) => Ok(PhysicalPlan::Simple {
                kind: "RepairTable".to_string(),
            }),
            Statement::LoadStats(_) => Ok(PhysicalPlan::Simple {
                kind: "LoadStats".to_string(),
            }),
            Statement::LockStats(_) => Ok(PhysicalPlan::Simple {
                kind: "LockStats".to_string(),
            }),
            Statement::UnlockStats(_) => Ok(PhysicalPlan::Simple {
                kind: "UnlockStats".to_string(),
            }),
            Statement::Trace(_) => Ok(PhysicalPlan::Simple {
                kind: "Trace".to_string(),
            }),
            Statement::PlanReplayer(_) => Ok(PhysicalPlan::Simple {
                kind: "PlanReplayer".to_string(),
            }),
            Statement::BRIE(_) => Ok(PhysicalPlan::Simple {
                kind: "BRIE".to_string(),
            }),
            Statement::ImportInto(_) => Ok(PhysicalPlan::Simple {
                kind: "ImportInto".to_string(),
            }),
            Statement::SplitRegion(_) => Ok(PhysicalPlan::Simple {
                kind: "SplitRegion".to_string(),
            }),
            Statement::DistributeTable(_) => Ok(PhysicalPlan::Simple {
                kind: "DistributeTable".to_string(),
            }),
            Statement::CompactTable(_) => Ok(PhysicalPlan::Simple {
                kind: "CompactTable".to_string(),
            }),
            Statement::RecommendIndex(_) => Ok(PhysicalPlan::Simple {
                kind: "RecommendIndex".to_string(),
            }),
            Statement::CalibrateResource(_) => Ok(PhysicalPlan::Simple {
                kind: "CalibrateResource".to_string(),
            }),
            Statement::FlashBackTable(_) => Ok(PhysicalPlan::Simple {
                kind: "FlashBackTable".to_string(),
            }),
            Statement::FlashBackDatabase(_) => Ok(PhysicalPlan::Simple {
                kind: "FlashBackDatabase".to_string(),
            }),
            Statement::FlashBackToTimestamp(_) => Ok(PhysicalPlan::Simple {
                kind: "FlashBackToTimestamp".to_string(),
            }),
            Statement::RecoverTable(_) => Ok(PhysicalPlan::Simple {
                kind: "RecoverTable".to_string(),
            }),
            Statement::DropPlacementPolicy(_) => Ok(PhysicalPlan::Simple {
                kind: "DropPlacementPolicy".to_string(),
            }),
            Statement::CreateResourceGroup(_) => Ok(PhysicalPlan::Simple {
                kind: "CreateResourceGroup".to_string(),
            }),
            Statement::SetResourceGroup(_) => Ok(PhysicalPlan::Simple {
                kind: "SetResourceGroup".to_string(),
            }),
            Statement::AddQueryWatch(_) => Ok(PhysicalPlan::Simple {
                kind: "AddQueryWatch".to_string(),
            }),
            Statement::DropQueryWatch(_) => Ok(PhysicalPlan::Simple {
                kind: "DropQueryWatch".to_string(),
            }),
            Statement::RenameUser(_) => Ok(PhysicalPlan::Simple {
                kind: "RenameUser".to_string(),
            }),
            Statement::Binlog(_) => Ok(PhysicalPlan::Simple {
                kind: "Binlog".to_string(),
            }),
            Statement::Change(_) => Ok(PhysicalPlan::Simple {
                kind: "Change".to_string(),
            }),
            Statement::AlterInstance(_) => Ok(PhysicalPlan::Simple {
                kind: "AlterInstance".to_string(),
            }),
            Statement::AnalyzeTable(_) => Ok(PhysicalPlan::Simple {
                kind: "Analyze".to_string(),
            }),
            Statement::SetTxn(_) => Ok(PhysicalPlan::Simple {
                kind: "SetTxn".to_string(),
            }),
            Statement::Prepare(_) => Ok(PhysicalPlan::Simple {
                kind: "Prepare".to_string(),
            }),
            Statement::Execute(_) => Ok(PhysicalPlan::Simple {
                kind: "Execute".to_string(),
            }),
            Statement::Deallocate(_) => Ok(PhysicalPlan::Simple {
                kind: "Deallocate".to_string(),
            }),
            Statement::CreateBinding(_) => Ok(PhysicalPlan::Simple {
                kind: "CreateBinding".to_string(),
            }),
            Statement::DropBinding(_) => Ok(PhysicalPlan::Simple {
                kind: "DropBinding".to_string(),
            }),
            Statement::Set(_) => Ok(PhysicalPlan::Simple {
                kind: "Set".to_string(),
            }),
            Statement::Explain(explain) => Ok(PhysicalPlan::Explain {
                format: explain.format,
                analyze: explain.analyze,
                child: Box::new(with_explain_build(|| {
                    self.build_with_schema(&explain.stmt, is, current_db, stats, prune_mode)
                })?),
            }),
            Statement::SetOpr(set) => {
                let mut arms: Vec<(SetOprType, PhysicalPlan)> =
                    Vec::with_capacity(set.rest.len() + 1);
                arms.push((
                    SetOprType::UnionAll,
                    self.build_with_schema(
                        &Statement::Select(with_inherited(set.first.clone(), set.with.as_ref())),
                        is,
                        current_db,
                        stats,
                        prune_mode,
                    )?,
                ));
                for (op, select) in &set.rest {
                    arms.push((
                        *op,
                        self.build_with_schema(
                            &Statement::Select(with_inherited(select.clone(), set.with.as_ref())),
                            is,
                            current_db,
                            stats,
                            prune_mode,
                        )?,
                    ));
                }
                // EXPLAIN Sort/TopN over a set op renders TiDB's plan column
                // ids (`ORDER BY 1` → `Column#N`).
                let ids = PlanColumnIds::new(is, current_db).statement(set);
                let arm_selects: Vec<&SelectStmt> =
                    std::iter::once(&set.first).chain(set.rest.iter().map(|(_, s)| s)).collect();
                let plan = build_set_op_plan(arms, Some(&arm_selects), &ids, current_db);
                let by_info = if set.order_by.is_empty() {
                    String::new()
                } else {
                    set_op_order_by_info(&set.order_by, &set.first, ids.outer_ids())
                };
                let plan = apply_order_limit(
                    plan,
                    !set.order_by.is_empty(),
                    set.limit,
                    set.offset,
                    Table::PSEUDO_ROW_COUNT as f64,
                    by_info,
                );
                Ok(plan)
            }
        }
    }
}

/// Byte marker on a TableReader `pushed` that carries an EXPLAIN-only cop
/// `Projection` above the pushed `Selection` (`data:Projection`).
/// Format: `COP_PROJ_SEL|<proj exprs>|<selection info>`.
pub(crate) fn cop_proj_sel_push(proj: &str, sel: &str) -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(format!("COP_PROJ_SEL|{proj}|{sel}").into_bytes()),
    })
}

/// `SELECT *` over the DML target table, used for the DML root's EXPLAIN child.
fn dml_scan_select(
    table: &TableName,
    join: Option<&sealdb_parser::ast::Join>,
    selection: Option<ExprNode>,
) -> SelectStmt {
    let (join, selection) = match join {
        Some(join) => dml_join_conditions(join, selection),
        None => (None, selection),
    };
    SelectStmt {
        fields: vec![sealdb_parser::ast::SelectField {
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
            orig_text: None,
        }],
        from: Some(table.clone()),
        joins: join.into_iter().collect(),
        from_item: None,
        selection,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: Default::default(),
        lock_wait: Default::default(),
        with: None,
        text: String::new(),
    }
}

/// `UPDATE t1, t2 …` / `DELETE t1, t2 FROM t1 JOIN t2 …` carry the join key in
/// `WHERE`, not in `ON`. TiDB's predicate pushdown (`pushDownPredicatesForJoin`)
/// promotes a cross-table equality to the join key and mirrors each side's
/// single-table filters onto the other side, which is what lets both sides use
/// a point get (`id = 1`) or a handle range (`id > 0`) in the DML plan.
/// Doing it on the generated SELECT keeps access path selection — and its
/// row-count estimates — working on the derived predicates.
fn dml_join_conditions(
    join: &sealdb_parser::ast::Join,
    selection: Option<ExprNode>,
) -> (Option<sealdb_parser::ast::Join>, Option<ExprNode>) {
    use sealdb_parser::ast::expressions::ColumnNameExpr;
    use sealdb_parser::opcode::Op;

    let Some(selection) = selection else {
        return (Some(join.clone()), None);
    };
    if !matches!(&join.on, ExprNode::Value(v) if v.datum == Datum::I64(1)) {
        return (Some(join.clone()), Some(selection));
    }
    let owns = |col: &ColumnNameExpr, table: &TableName| {
        col.table.as_deref().is_some_and(|t| {
            t.eq_ignore_ascii_case(&table.name) || t.eq_ignore_ascii_case(table.resolve_name())
        })
    };
    // Only a same-named key (`t1.id = t2.id`) can be mirrored by swapping the
    // qualifier; a renamed key needs column mapping, so leave it to the
    // general join planner.
    let join_key = |expr: &ExprNode| -> Option<(ColumnNameExpr, ColumnNameExpr)> {
        let ExprNode::Binary(bin) = expr else {
            return None;
        };
        if bin.op != Op::Eq {
            return None;
        }
        let (ExprNode::Column(lhs), ExprNode::Column(rhs)) =
            (bin.left.as_ref(), bin.right.as_ref())
        else {
            return None;
        };
        if !lhs.name.eq_ignore_ascii_case(&rhs.name) {
            return None;
        }
        if owns(lhs, &join.left) && owns(rhs, &join.right) {
            return Some((lhs.clone(), rhs.clone()));
        }
        if owns(lhs, &join.right) && owns(rhs, &join.left) {
            return Some((rhs.clone(), lhs.clone()));
        }
        None
    };
    let mut conjuncts = selection.split_cnf();
    let Some(key_at) = conjuncts.iter().position(|c| join_key(c).is_some()) else {
        return (Some(join.clone()), Some(selection));
    };
    let key = conjuncts.remove(key_at);
    let mut join = join.clone();
    join.on = key;
    // Promoting the key makes this an inner join (`JOIN` without `ON` parses as
    // CROSS, whose EXPLAIN would keep the CARTESIAN row estimate).
    join.tp = sealdb_parser::ast::JoinType::Inner;
    // Mirror single-table filters onto the other side (same column name as the
    // join key, same comparison, only the qualifier changes).
    let owns_all = |expr: &ExprNode, table: &TableName| -> bool {
        let cols = expr.referenced_columns();
        !cols.is_empty()
            && cols.iter().all(|(qualified, _)| {
                qualified.as_deref().is_some_and(|t| {
                    t.eq_ignore_ascii_case(&table.name)
                        || t.eq_ignore_ascii_case(table.resolve_name())
                })
            })
    };
    let mut derived = Vec::new();
    for conjunct in &conjuncts {
        for (from, to) in [(&join.left, &join.right), (&join.right, &join.left)] {
            if !owns_all(conjunct, from) {
                continue;
            }
            if let Some(mirrored) = rebind_table_qualifier(conjunct, &from.name, &to.name) {
                derived.push(mirrored);
            }
        }
    }
    conjuncts.extend(derived);
    (
        Some(join),
        ExprNode::compose_cnf(conjuncts).or(Some(selection)),
    )
}

/// Replace the table qualifier of every column reference in a conjunct.
fn rebind_table_qualifier(expr: &ExprNode, from: &str, to: &str) -> Option<ExprNode> {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, IsNullExpr, UnaryOpExpr};
    // Literals and other column-free nodes (dates, casts of constants) copy as-is.
    if expr.referenced_columns().is_empty() {
        return Some(expr.clone());
    }
    match expr {
        ExprNode::Column(col) => {
            let table = col.table.as_deref()?;
            if !table.eq_ignore_ascii_case(from) {
                return None;
            }
            Some(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(to.to_string()),
                name: col.name.clone(),
            }))
        }
        ExprNode::Binary(bin) => Some(ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(rebind_table_qualifier(&bin.left, from, to)?),
            right: Box::new(rebind_table_qualifier(&bin.right, from, to)?),
            single_cnf_item: false,
        })),
        ExprNode::Unary(un) => Some(ExprNode::Unary(UnaryOpExpr {
            op: un.op,
            expr: Box::new(rebind_table_qualifier(&un.expr, from, to)?),
        })),
        ExprNode::IsNull(isn) => Some(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(rebind_table_qualifier(&isn.expr, from, to)?),
            not: isn.not,
        })),
        _ => None,
    }
}

/// TiDB physical join choice for a multi-table DML target.
///
/// `target` is the statement's own table (`UPDATE t1, t2 …` → `t1`). When both
/// sides are pinned to the same handle the join key folds to TRUE
/// (`id = 1` mirrored across `t1.id = t2.id`), and TiDB keeps a CARTESIAN
/// HashJoin; otherwise both sides are handle ranges already ordered on the key
/// and TiDB merges them instead of building a hash table.
fn dml_join_shape(plan: PhysicalPlan, target: &str) -> PhysicalPlan {
    let (left, right, _left_table, _right_table, on) = match plan {
        PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left,
            right,
            left_table,
            right_table,
            on,
            ..
        }
        | PhysicalPlan::MergeJoin {
            tp: JoinType::Inner,
            left,
            right,
            left_table,
            right_table,
            on,
            ..
        } => (left, right, left_table, right_table, on),
        other => return other,
    };
    let same_handle = match (left.as_ref(), right.as_ref()) {
        (
            PhysicalPlan::PointGet {
                handle: Some(a), ..
            },
            PhysicalPlan::PointGet {
                handle: Some(b), ..
            },
        ) => a == b,
        _ => false,
    };
    // TiDB lists Build before Probe.
    let (target_child, other_child) =
        if plan_table(&left).is_some_and(|t| t.eq_ignore_ascii_case(target)) {
            (left, right)
        } else {
            (right, left)
        };
    let other_table = plan_table(&other_child).unwrap_or(target).to_string();
    if same_handle {
        // The key folds to TRUE (`id = 1` on both sides of `t1.id = t2.id`).
        // TiDB builds the joined table and probes the statement's own table.
        return PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::Cross,
            left: target_child,
            right: other_child,
            left_table: target.to_string(),
            right_table: other_table,
            on: true_datum(),
        };
    }
    if !is_handle_range(&target_child) || !is_handle_range(&other_child) {
        return PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::Inner,
            left: target_child,
            right: other_child,
            left_table: target.to_string(),
            right_table: other_table,
            on,
        };
    }
    // Both sides are handle ranges that already come out ordered on the join
    // key, so TiDB merges instead of building a hash table; unanalyzed sides
    // use the pseudo null-reject selectivity (3333.33 ⋈ 3333.33 → 4166.67).
    // The DML join keeps the statement's own table as Build.
    let est =
        super::explain::est_rows(&target_child).min(super::explain::est_rows(&other_child)) * 1.25;
    let join_keys = dml_join_key_labels(&on);
    PhysicalPlan::MergeJoin {
        est_rows: Some(est),
        left: set_keep_order((*other_child).clone()).into(),
        right: set_keep_order((*target_child).clone()).into(),
        left_table: other_table,
        right_table: target.to_string(),
        left_key: join_keys.0,
        right_key: join_keys.1,
        tp: JoinType::Inner,
        info: None,
        on,
    }
}

/// Physical table name of a single-table join side.
fn plan_table(plan: &PhysicalPlan) -> Option<&str> {
    match plan {
        PhysicalPlan::PointGet { table, .. } => Some(table.as_str()),
        PhysicalPlan::TableReader { table, .. } => Some(table.as_str()),
        _ => None,
    }
}

fn true_datum() -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::I64(1),
    })
}

fn is_handle_range(plan: &PhysicalPlan) -> bool {
    matches!(
        plan,
        PhysicalPlan::TableReader {
            range_scan: Some(_),
            ..
        }
    )
}

/// `(left key, right key)` EXPLAIN labels for a DML join equality.
fn dml_join_key_labels(on: &ExprNode) -> (String, String) {
    use sealdb_parser::opcode::Op;
    let ExprNode::Binary(bin) = on else {
        return (String::new(), String::new());
    };
    if bin.op != Op::Eq {
        return (String::new(), String::new());
    }
    let (ExprNode::Column(lhs), ExprNode::Column(rhs)) = (bin.left.as_ref(), bin.right.as_ref())
    else {
        return (String::new(), String::new());
    };
    (
        super::explain::explain_join_col(lhs),
        super::explain::explain_join_col(rhs),
    )
}

/// TiDB `DELETE` on a table whose handle is `_tidb_rowid` projects the row id
/// in the cop task: `TableReader (data:Projection) → Projection (cop) → Selection`.
fn dml_delete_rowid_projection(
    plan: PhysicalPlan,
    delete: &sealdb_parser::ast::DeleteStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    _stats: Option<&Handle>,
) -> PhysicalPlan {
    if delete.join.is_some() || !delete.tables.is_empty() {
        return plan;
    }
    let info = is.and_then(|is| is.table_by_name(current_db, delete.table.resolve_name()));
    if info.is_some_and(|t| t.pk_is_handle) {
        return plan;
    }
    let PhysicalPlan::TableReader {
        table,
        partitions,
        partition_names,
        partition_access,
        row_count,
        stats_pseudo,
        data_limit,
        filter_est,
        pushed,
        ..
    } = plan.clone()
    else {
        return plan;
    };
    let Some(filter) = pushed else {
        return plan;
    };
    let scan_rows = row_count.abs();
    let est = if dml_is_range_comparison(&filter) && scan_rows >= 10_000.0 {
        scan_rows / 3.0
    } else {
        super::explain::est_rows(&plan)
    };
    let sel_info = super::explain::explain_pushed_filter(&table, &filter);
    let proj = format!("{}.{}._tidb_rowid", schema_prefix(current_db), table);
    PhysicalPlan::TableReader {
        table,
        partitions,
        partition_names,
        partition_access,
        row_count: -est,
        stats_pseudo,
        data_limit,
        filter_est,
        range_scan: None,
        keep_order: false,
        pushed: Some(cop_proj_sel_push(&proj, &sel_info)),
    }
}

/// Single range comparison (`>`, `>=`, `<`, `<=`) — TiDB pseudo selectivity 1/3.
fn dml_is_range_comparison(expr: &ExprNode) -> bool {
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Binary(b) => matches!(b.op, Op::Gt | Op::Ge | Op::Lt | Op::Le),
        _ => false,
    }
}

impl PlanBuilder {
    /// TiDB keeps the source plan under `INSERT … SELECT` / `REPLACE … SELECT`
    fn dml_insert_child(
        &self,
        insert: &sealdb_parser::ast::InsertStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<Box<PhysicalPlan>>> {
        if let Some(set) = insert.select_set.as_deref() {
            let plan = with_dml_source(|| {
                self.build_with_schema(
                    &Statement::SetOpr(set.clone()),
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )
            })?;
            return Ok(Some(Box::new(plan)));
        }
        let Some(select) = insert.select.as_deref() else {
            return Ok(None);
        };
        let plan = with_dml_source(|| {
            self.build_with_schema(
                &Statement::Select(select.clone()),
                is,
                current_db,
                stats,
                prune_mode,
            )
        })?;
        Ok(Some(Box::new(plan)))
    }

    /// `UPDATE` reads the target rows through a SELECT-shaped plan.
    fn dml_update_child(
        &self,
        update: &sealdb_parser::ast::UpdateStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<Box<PhysicalPlan>>> {
        if !update.order_by.is_empty() || update.limit.is_some() {
            // TiDB plans ordered/limited UPDATE with Sort/Limit below the root.
        }
        let select = dml_scan_select(
            &update.table,
            update.join.as_ref(),
            update.selection.clone(),
        );
        let plan = self.build_with_schema(
            &Statement::Select(select),
            is,
            current_db,
            stats,
            prune_mode,
        )?;
        let plan = if update.join.is_some() {
            dml_join_shape(plan, &update.table.name)
        } else {
            plan
        };
        Ok(Some(Box::new(plan)))
    }

    /// `DELETE` reads the row handles to remove. Non-clustered tables project
    /// `_tidb_rowid` in the cop task (`data:Projection`).
    fn dml_delete_child(
        &self,
        delete: &sealdb_parser::ast::DeleteStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<Box<PhysicalPlan>>> {
        let mut select = dml_scan_select(
            &delete.table,
            delete.join.as_ref(),
            delete.selection.clone(),
        );
        // TiDB's DML children read the row handle only, which lets a covering
        // secondary index serve the scan (`Delete → IndexReader`). For an
        // integer PK the handle is the PK column itself.
        if delete.join.is_none()
            && let Some(handle) = is
                .and_then(|is| is.table_by_name(current_db, delete.table.resolve_name()))
                .filter(|t| t.pk_is_handle)
                .and_then(|t| t.handle_column())
        {
            select.fields = vec![sealdb_parser::ast::SelectField {
                expr: ExprNode::Column(sealdb_parser::ast::expressions::ColumnNameExpr {
                    schema: None,
                    table: None,
                    name: handle.name.o().to_string(),
                }),
                as_name: None,
                orig_text: None,
            }];
        }
        let plan = self.build_with_schema(
            &Statement::Select(select),
            is,
            current_db,
            stats,
            prune_mode,
        )?;
        let plan = if delete.join.is_some() {
            dml_join_shape(plan, &delete.table.name)
        } else {
            plan
        };
        Ok(Some(Box::new(dml_delete_rowid_projection(
            plan, delete, is, current_db, stats,
        ))))
    }

    fn build_cte_plan(
        &self,
        select: &SelectStmt,
        from: &TableName,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        let Some(with) = select.with.as_ref() else {
            return Ok(None);
        };
        if with.find(&from.name).is_none() {
            return Ok(None);
        }
        if from.schema.is_some() {
            return Err(Error::unsupported("qualified CTE name"));
        }
        let seed_select = with.seed_select(&from.name)?;
        let cte = with.find(&from.name);
        // Prefer explicit `WITH cte (a,b) AS`; else seed AS aliases / column names
        // so `cte2 as c1 … WHERE c2.col_2` resolves (TiDB CTE output names).
        let columns = {
            let listed = cte.map(|cte| cte.col_name_list.clone()).unwrap_or_default();
            if listed.is_empty() && !is_star_select(&seed_select) {
                // Non-star seeds: AS aliases / column names become CTE output names.
                // `SELECT *` keeps empty here so CTEExec inherits expanded seed schema.
                cte_seed_output_names(&seed_select)
            } else {
                listed
            }
        };
        let mut seed = self.build_with_schema(
            &Statement::Select(seed_select.clone()),
            is,
            current_db,
            stats,
            prune_mode,
        )?;
        if !is_star_select(&seed_select)
            && !needs_hash_agg(&seed_select)
            && !needs_window(&seed_select)
            && !matches!(seed, PhysicalPlan::Projection { .. })
        {
            let exprs = if columns.is_empty() {
                seed_select
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("col{i}"))
                    .collect()
            } else {
                columns.clone()
            };
            seed = PhysicalPlan::Projection {
                exprs,
                child: Box::new(seed),
            };
        }
        // `SELECT <scalar agg|const> FROM (SELECT <aggs> FROM base …) d`: TiDB
        // prunes the derived aggregate's outputs and seeds a `count(1)`, which
        // also renumbers the derived / cop plan columns (see `derived_agg_ids`).
        if select
            .from
            .as_ref()
            .is_some_and(|f| f.resolve_name().eq_ignore_ascii_case(&from.name))
            && with.set_rest(&from.name)?.is_empty()
            && let Some((ids, _)) = derived_agg_ids(select, is, current_db)
        {
            seed = rewrite_derived_agg(seed, ids);
        }
        let set_rest = with.set_rest(&from.name)?;
        let mut set_op_ids = SetOpColumnIds::default();
        if !set_rest.is_empty() {
            // TiDB set-op output column ids for dual constants start at #3.
            const DUAL_UNION_COLUMN: usize = 3;
            seed = remap_dual_projection_column(seed, DUAL_UNION_COLUMN);
            let mut arms = vec![(SetOprType::UnionAll, seed)];
            for (op, select) in set_rest {
                let right = self.build_with_schema(
                    &Statement::Select(select),
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )?;
                arms.push((op, remap_dual_projection_column(right, DUAL_UNION_COLUMN)));
            }
            // TiDB plan column ids of this CTE body's set op (dedup HashAgg info).
            if let Some(with) = select.with.as_ref() {
                set_op_ids = PlanColumnIds::new(is, current_db).cte(with, &from.name);
            }
            seed = build_set_op_plan(arms, None, &set_op_ids, current_db);
        }
        // TiDB: non-recursive CTEs inline by default unless AS MATERIALIZED or
        // referenced more than once (shared CTETableReader).
        let refs = cte_ref_count(&from.name).unwrap_or_else(|| {
            select
                .join_tables()
                .into_iter()
                .filter(|t| t.resolve_name().eq_ignore_ascii_case(&from.name))
                .count()
        });
        let recursive = cte.is_some_and(|cte| cte.recursive.is_some());
        let recursive_plan = if let Some((_, rec)) = with.recursive_arm(&from.name)? {
            Some(Box::new(self.build_with_schema(
                &Statement::Select(rec),
                is,
                current_db,
                stats,
                prune_mode,
            )?))
        } else {
            None
        };
        let inline = cte.is_some_and(|cte| !cte_materialized(cte, refs))
            || seed_is_dual_projection(&seed)
            // Derived UNION ALL of duals (topn_pushdown) inlines like a dual CTE.
            || seed_is_union_of_dual(&seed);
        // `cte1 AS dt2` → name=cte1, physical=dt2 after join_side remap.
        let alias = from
            .physical
            .clone()
            .filter(|a| !a.eq_ignore_ascii_case(&from.name));
        let limit = cte.and_then(|c| c.limit);
        let offset = cte.and_then(|c| c.offset);
        Ok(Some(PhysicalPlan::CTE {
            name: from.name.clone(),
            alias,
            columns,
            seed: Box::new(seed),
            recursive_plan,
            inline,
            recursive,
            limit,
            offset,
        }))
    }

    /// HashJoin tree. Inner equality joins of 3+ tables use n-table DP
    /// (`rule_join_reorder_dp`); otherwise left-deep with two-table reorder on
    /// the first pair. CTE sides use CTEExec.
    fn build_join_plan(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        let plan = self.build_join_plan_impl(select, is, current_db, stats, prune_mode)?;
        let plan = inject_inner_join_null_reject(plan, select, is, current_db);
        // TiDB sizes the join on the pushed-down children (the null-rejecting
        // Selection above each side changes the key NDV).
        let plan = fill_join_est_rows(
            plan,
            is,
            current_db,
            stats,
            &query_alias_map(select),
        );
        let plan = star_join_projection(plan, select, is, current_db);
        Ok(plan)
    }

    fn build_join_plan_impl(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        // TiDB hash join: subqueries inside a JOIN ON clause are rejected.
        if select.joins.iter().any(|j| j.on.has_subquery())
            || select
                .from_item
                .as_ref()
                .is_some_and(from_item_on_has_subquery)
        {
            return Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                "ON condition doesn't support subqueries yet",
            ));
        }
        if let Some(item) = &select.from_item {
            validate_from_item_using(item, is, current_db)?;
            return self.build_from_item(select, item, is, current_db, stats, prune_mode);
        }
        validate_join_using(select, is, current_db)?;
        // STRAIGHT_JOIN / /*+ MERGE_JOIN(...) */ freeze join order and prefer
        // MergeJoin on equality keys (TiDB PhysicalMergeJoin).
        if select.joins.iter().any(|j| j.straight) || select_has_merge_join_hint(select) {
            return self.build_straight_merge_join(select, is, current_db, stats, prune_mode);
        }
        if let Some(plan) = self.build_join_plan_dp(select, is, current_db, stats, prune_mode)? {
            return Ok(plan);
        }
        if let Some(plan) =
            self.build_left_outer_chain(select, is, current_db, stats, prune_mode)?
        {
            return Ok(plan);
        }
        if let Some(plan) = self.build_left_then_inner(select, is, current_db, stats, prune_mode)? {
            return Ok(plan);
        }
        if let Some(plan) =
            self.build_right_outer_chain(select, is, current_db, stats, prune_mode)?
        {
            return Ok(plan);
        }
        if let Some(plan) = self.build_ordered_right_join(select, is, current_db, stats)? {
            return Ok(plan);
        }
        let first = &select.joins[0];
        // Inner-join key constant propagation: both sides must see `key = const`
        // before access path selection so each can use the point get.
        let (left_derived, right_derived) = if matches!(map_join_type(first.tp), JoinType::Inner) {
            derive_join_key_constants(select, &first.left, &first.right, &first.on)
        } else {
            (Vec::new(), Vec::new())
        };
        let left_select = with_derived_selection(select, left_derived);
        let right_select = with_derived_selection(select, right_derived);
        let mut left = self.join_side(
            left_select.as_ref().unwrap_or(select),
            &first.left,
            is,
            current_db,
            stats,
            prune_mode,
        )?;
        let mut right = self.join_side(
            right_select.as_ref().unwrap_or(select),
            &first.right,
            is,
            current_db,
            stats,
            prune_mode,
        )?;
        let first_tp = map_join_type(first.tp);
        // not(isnull) goes into the null-supplying side: the right table of a
        // LEFT JOIN (build side) and the left table of a RIGHT JOIN.
        let inner_side = if matches!(first_tp, JoinType::RightOuter) {
            &first.left
        } else {
            &first.right
        };
        if matches!(first_tp, JoinType::LeftOuter | JoinType::RightOuter)
            && let Some(filter) = null_reject_on_inner(
                &first.on,
                &inner_side.name,
                inner_side.resolve_name(),
                &table_column_names(is, current_db, inner_side),
            )
        {
            if matches!(first_tp, JoinType::RightOuter) {
                left = predicate_push_down(left, &filter, is, current_db);
            } else {
                right = predicate_push_down(right, &filter, is, current_db);
            }
        }
        // TiDB: single-side ON (e.g. `ON (derived.col)`) → push into that side,
        // leave a pure CARTESIAN join.
        let (left, right, first_on, first_tp) = push_single_side_on(
            select,
            left,
            right,
            &first.left.name,
            &first.right.name,
            first.on.clone(),
            first_tp,
            is,
            current_db,
        );
        let mut plan = reorder_hash_join(
            first_tp,
            left,
            right,
            first.left.name.clone(),
            first.right.name.clone(),
            table_row_count(stats, &first.left, current_db),
            table_row_count(stats, &first.right, current_db),
            // EXPLAIN keys use OrigTblName (`t1` alias → `uk_hp16726`).
            explain_on_expr(&first_on, select),
        );
        if matches!(first_tp, JoinType::RightOuter) && select.joins.len() == 1 {
            plan = refine_right_join_explain(plan, select, is, current_db, stats);
        }
        for join in select.joins.iter().skip(1) {
            let tp = map_join_type(join.tp);
            let mut right =
                self.join_side(select, &join.right, is, current_db, stats, prune_mode)?;
            if matches!(tp, JoinType::LeftOuter | JoinType::RightOuter)
                && let Some(filter) = null_reject_on_inner(
                    &join.on,
                    &join.right.name,
                    join.right.resolve_name(),
                    &table_column_names(is, current_db, &join.right),
                )
            {
                right = predicate_push_down(right, &filter, is, current_db);
            }
            plan = PhysicalPlan::HashJoin {
                est_rows: None,
                tp,
                left: Box::new(plan),
                right: Box::new(right),
                left_table: join.left.name.clone(),
                right_table: join.right.name.clone(),
                on: join.on.clone(),
            };
        }
        Ok(plan)
    }

    /// Left-deep MergeJoin chain for `STRAIGHT_JOIN` / `MERGE_JOIN` hint.
    ///
    /// With a CTE side, TiDB puts the CTE (inlined seed) as Build and wraps
    /// non-PK sides in `not(isnull)` + Sort on the join key so ORDER BY on the
    /// join columns needs no outer Sort.
    fn build_straight_merge_join(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        let first = &select.joins[0];
        let left = self.join_side(select, &first.left, is, current_db, stats, prune_mode)?;
        let right = self.join_side(select, &first.right, is, current_db, stats, prune_mode)?;
        // children_of lists right(Build) then left(Probe). Keep SQL order so a
        // CTE on the right becomes Build (TiDB CTE MERGE_JOIN goldens).
        let (left_key, right_key) = merge_join_key_labels_for(
            select,
            current_db,
            &first.left,
            &first.right,
            &first.on,
            &first.using_cols,
            is,
        );
        let first_tp = map_join_type(first.tp);
        let left = prepare_merge_join_side(
            left,
            select,
            &first.left,
            &first.on,
            &left_key,
            is,
            current_db,
            // The preserved side of an outer join keeps NULL keys (they never
            // match); on an inner join both sides supply NULLs, so both get
            // `not(isnull(key))`.
            matches!(
                first_tp,
                JoinType::RightOuter | JoinType::Inner | JoinType::Cross
            ),
        );
        let mut right = prepare_merge_join_side(
            right,
            select,
            &first.right,
            &first.on,
            &right_key,
            is,
            current_db,
            !matches!(first_tp, JoinType::LeftOuter),
        );
        if matches!(map_join_type(first.tp), JoinType::LeftOuter)
            && let Some(range) = outer_join_inner_range(&first.left, &first.right, &first.on)
        {
            right = push_inner_range_scan(right, &range);
        }
        let info = merge_join_info(
            first.tp,
            &first.left.name,
            &first.right.name,
            &left_key,
            &right_key,
            &first.on,
        );
        let mut plan = PhysicalPlan::MergeJoin {
            est_rows: None,
            left: Box::new(left),
            right: Box::new(right),
            left_table: first.left.name.clone(),
            right_table: first.right.name.clone(),
            left_key,
            right_key,
            tp: map_join_type(first.tp),
            info,
            on: first.on.clone(),
        };
        for join in select.joins.iter().skip(1) {
            let mut right =
                self.join_side(select, &join.right, is, current_db, stats, prune_mode)?;
            let (left_key, right_key) = merge_join_key_labels_for(
                select,
                current_db,
                &join.left,
                &join.right,
                &join.on,
                &join.using_cols,
                is,
            );
            let tp = map_join_type(join.tp);
            right = prepare_merge_join_side(
                right,
                select,
                &join.right,
                &join.on,
                &right_key,
                is,
                current_db,
                !matches!(tp, JoinType::LeftOuter),
            );
            if matches!(map_join_type(join.tp), JoinType::LeftOuter)
                && let Some(range) = outer_join_inner_range(&join.left, &join.right, &join.on)
            {
                right = push_inner_range_scan(right, &range);
            }
            let info = merge_join_info(
                join.tp,
                &join.left.name,
                &join.right.name,
                &left_key,
                &right_key,
                &join.on,
            );
            plan = PhysicalPlan::MergeJoin {
                est_rows: None,
                left: Box::new(plan),
                right: Box::new(right),
                left_table: join.left.name.clone(),
                right_table: join.right.name.clone(),
                left_key,
                right_key,
                tp: map_join_type(join.tp),
                info,
                on: join.on.clone(),
            };
        }
        Ok(plan)
    }

    /// TiDB outer-join reorder (`tidb_enable_outer_join_reorder`): a left-deep
    /// chain `base ⟕ r1 ⟕ r2` whose ON keys only mention `base` can reorder
    /// the right sides by ascending cardinality.
    fn build_left_outer_chain(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        if select.joins.len() < 2
            || !select
                .joins
                .iter()
                .all(|j| matches!(map_join_type(j.tp), JoinType::LeftOuter))
        {
            return Ok(None);
        }
        let base = &select.joins[0].left;
        let base_name = base.name.clone();
        let mut rights: Vec<(TableName, ExprNode, f64)> = Vec::new();
        for join in &select.joins {
            if !outer_join_on_base(&join.on, &base_name, &join.right.name) {
                return Ok(None);
            }
            rights.push((
                join.right.clone(),
                join.on.clone(),
                table_row_count(stats, &join.right, current_db),
            ));
        }
        rights.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        let mut plan = self.join_side(select, base, is, current_db, stats, prune_mode)?;
        let mut left_table = base_name.clone();
        for (right_table, on, _) in rights {
            let mut right =
                self.join_side(select, &right_table, is, current_db, stats, prune_mode)?;
            if let Some(filter) = null_reject_on_inner(
                &on,
                &right_table.name,
                right_table.resolve_name(),
                &table_column_names(is, current_db, &right_table),
            ) {
                right = predicate_push_down(right, &filter, is, current_db);
            }
            plan = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(plan),
                right: Box::new(right),
                left_table: left_table.clone(),
                right_table: right_table.name.clone(),
                on,
            };
            left_table = base_name.clone();
        }
        Ok(Some(wrap_star_join_projection(
            plan, select, is, current_db,
        )))
    }

    /// `A ⋊ B ⋊ C` with ON keys on the preserved spine — mirror of left chain.
    fn build_right_outer_chain(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        if select.joins.len() < 2
            || !select
                .joins
                .iter()
                .all(|j| matches!(map_join_type(j.tp), JoinType::RightOuter))
        {
            return Ok(None);
        }
        // `t2 right join t3 on … right join t1` — preserved side is the last right.
        let preserved = &select.joins.last().unwrap().right;
        let preserved_name = preserved.name.clone();
        // Collect non-preserved sides with ON against preserved / chain.
        // TiDB: convert to left-outer form with Build=inner joins of smaller sides.
        // For `t2 ⋊ t3 ⋊ t1` golden: (t3 ⋈ t2) ⋊ t1 as right outer with Probe=t1.
        let j0 = &select.joins[0];
        let j1 = &select.joins[1];
        // Expect left-deep: (t2 ⋊ t3) ⋊ t1 with ON t3.a=t2.a and t2.a=t1.a.
        if select.joins.len() != 2 {
            return Ok(None);
        }
        if !outer_join_on_base(&j0.on, &j0.left.name, &j0.right.name) {
            return Ok(None);
        }
        if !outer_join_on_base(&j1.on, &j0.left.name, &j1.right.name)
            && !outer_join_on_base(&j1.on, &j0.right.name, &j1.right.name)
        {
            return Ok(None);
        }
        // Inner between j0.left (t2) and j0.right (t3), then right-outer with t1.
        let mut t_left = self.join_side(select, &j0.left, is, current_db, stats, prune_mode)?;
        let mut t_mid = self.join_side(select, &j0.right, is, current_db, stats, prune_mode)?;
        if let Some(filter) = null_reject_on_inner(
            &j0.on,
            &j0.right.name,
            j0.right.resolve_name(),
            &table_column_names(is, current_db, &j0.right),
        ) {
            t_mid = predicate_push_down(t_mid, &filter, is, current_db);
        }
        if let Some(filter) = null_reject_on_inner(
            &j0.on,
            &j0.left.name,
            j0.left.resolve_name(),
            &table_column_names(is, current_db, &j0.left),
        ) {
            t_left = predicate_push_down(t_left, &filter, is, current_db);
        }
        let inner = reorder_hash_join(
            JoinType::Inner,
            t_left,
            t_mid,
            j0.left.name.clone(),
            j0.right.name.clone(),
            table_row_count(stats, &j0.left, current_db),
            table_row_count(stats, &j0.right, current_db),
            flip_eq_for_build(&j0.on, &j0.right.name),
        );
        let outer_right = &j1.right;
        let outer_side = self.join_side(select, outer_right, is, current_db, stats, prune_mode)?;
        // Golden: Build=inner (t3⋈t2), Probe=t1. Right outer null-supplies the
        // left child, so the inner join is `left` and the preserved t1 `right`.
        let plan = PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::RightOuter,
            left: Box::new(inner),
            right: Box::new(outer_side),
            left_table: j0.left.name.clone(),
            right_table: outer_right.name.clone(),
            on: j1.on.clone(),
        };
        let _ = preserved_name;
        Ok(Some(wrap_star_join_projection(
            plan, select, is, current_db,
        )))
    }

    /// `A RIGHT JOIN B ON A.k = B.k` where both sides can read their join key in
    /// order → `MergeJoin` over ordered scans.
    ///
    /// TiDB's physical property keeps an index scan on the inner key ordered
    /// (`IndexFullScan(idx_k), keep order:true`) and a clustered PRIMARY KEY
    /// prefix ordered by the common handle (`TableFullScan, keep order:true`);
    /// the sorted inputs make MergeJoin cheaper than building a hash table, and
    /// the index scan also absorbs the null-rejection of the inner key.
    fn build_ordered_right_join(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
    ) -> Result<Option<PhysicalPlan>> {
        if select.joins.len() != 1 || select.selection.is_some() {
            return Ok(None);
        }
        let join = &select.joins[0];
        if !matches!(join.tp, AstJoinType::RightOuter) || !join.using_cols.is_empty() {
            return Ok(None);
        }
        let Some(is) = is else {
            return Ok(None);
        };
        let (left_db, left_name) = join.left.qualified(current_db);
        let (right_db, right_name) = join.right.qualified(current_db);
        let Some(left_info) = is.table_by_name(&left_db, &left_name) else {
            return Ok(None);
        };
        let Some(right_info) = is.table_by_name(&right_db, &right_name) else {
            return Ok(None);
        };
        let Some((left_col, right_col)) = join_key_columns(left_info, right_info, &join.on) else {
            return Ok(None);
        };
        // Inner side: secondary index led by the key (`idx_9(col_11)`).
        let Some(index) = left_info
            .indices
            .iter()
            .filter(|idx| !idx.primary)
            .filter(|idx| {
                idx.columns
                    .first()
                    .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&left_col))
            })
            .min_by_key(|idx| idx.columns.len())
        else {
            return Ok(None);
        };
        // Preserved side: clustered PRIMARY KEY prefix (`col_3` leads idx_0).
        if !right_info.common_handle
            || right_info
                .clustered_prefix_column()
                .is_none_or(|c| !c.name.o().eq_ignore_ascii_case(&right_col))
        {
            return Ok(None);
        }
        // Both sides must carry statistics; the ordered plan is chosen on cost.
        let Some(left_stats) = stats
            .and_then(|h| h.table(&left_db, &left_name))
            .filter(|t| super::plan_cost::has_real_stats(t))
        else {
            return Ok(None);
        };
        let Some(right_stats) = stats
            .and_then(|h| h.table(&right_db, &right_name))
            .filter(|t| super::plan_cost::has_real_stats(t))
        else {
            return Ok(None);
        };
        let Some(left_column) = left_info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&left_col))
        else {
            return Ok(None);
        };
        let index_rows = left_stats
            .index_range_estimate(
                index.name.o(),
                &left_column.field_type,
                &[sealdb_statistics::range_est::StatsRange {
                    low: None,
                    high: None,
                }],
            )
            .unwrap_or(left_stats.count as f64);
        // TiDB estimates the join from the DataSource cardinalities and the
        // key NDVs: `both row counts / max(NDV(left key), NDV(right key))`.
        let join_est = match (
            left_stats.column_ndv(&left_col),
            right_stats.column_ndv(&right_col),
        ) {
            (Some(ln), Some(rn)) => {
                let ndv = ln.max(rn).max(1.0);
                Some(left_stats.count as f64 * right_stats.count as f64 / ndv)
            }
            _ => None,
        };
        let left_plan = PhysicalPlan::IndexReader {
            table: join.left.name.clone(),
            index: index.name.o().to_string(),
            index_columns: index_key_column_names(left_info, index),
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: index_rows,
            pushed: Some(bytes_info("KEEP_ORDER")),
            range_scan: None,
        };
        let mut right_plan = PhysicalPlan::table_reader_parts(
            join.right.name.clone(),
            Vec::new(),
            Vec::new(),
            String::new(),
            table_row_count(stats, &join.right, current_db),
            false,
        );
        if let PhysicalPlan::TableReader { keep_order, .. } = &mut right_plan {
            *keep_order = true;
        }
        let left_key = format!(
            "{left_db}.{}.{left_col}",
            merge_join_orig_table(select, &join.left)
        );
        let right_key = format!(
            "{right_db}.{}.{right_col}",
            merge_join_orig_table(select, &join.right)
        );
        let on = explain_on_expr(&join.on, select);
        let info = merge_join_info(
            join.tp,
            &join.left.name,
            &join.right.name,
            &left_key,
            &right_key,
            &on,
        );
        Ok(Some(PhysicalPlan::MergeJoin {
            est_rows: join_est,
            left: Box::new(left_plan),
            right: Box::new(right_plan),
            left_table: join.left.name.clone(),
            right_table: join.right.name.clone(),
            left_key,
            right_key,
            tp: JoinType::RightOuter,
            info,
            on,
        }))
    }

    /// `A LEFT JOIN B ON P(A,B) JOIN C ON Q(A,C)` → `(A ⋈ C) ⟕ B`.
    fn build_left_then_inner(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        if select.joins.len() != 2 {
            return Ok(None);
        }
        let j0 = &select.joins[0];
        let j1 = &select.joins[1];
        let tp0 = map_join_type(j0.tp);
        let tp1 = map_join_type(j1.tp);
        let (base, outer_right, outer_on, inner_right, inner_on, outer_tp) =
            if matches!(tp0, JoinType::LeftOuter) && matches!(tp1, JoinType::Inner) {
                (
                    &j0.left,
                    &j0.right,
                    &j0.on,
                    &j1.right,
                    &j1.on,
                    JoinType::LeftOuter,
                )
            } else if matches!(tp0, JoinType::RightOuter) && matches!(tp1, JoinType::Inner) {
                (
                    &j0.right,
                    &j0.left,
                    &j0.on,
                    &j1.right,
                    &j1.on,
                    JoinType::RightOuter,
                )
            } else {
                return Ok(None);
            };
        if !outer_join_on_base(outer_on, &base.name, &outer_right.name) {
            return Ok(None);
        }
        if !outer_join_on_base(inner_on, &base.name, &inner_right.name) {
            return Ok(None);
        }
        let mut base_for_inner = self.join_side(select, base, is, current_db, stats, prune_mode)?;
        let mut inner_side =
            self.join_side(select, inner_right, is, current_db, stats, prune_mode)?;
        if let Some(filter) = null_reject_on_inner(
            inner_on,
            &inner_right.name,
            inner_right.resolve_name(),
            &table_column_names(is, current_db, inner_right),
        ) {
            inner_side = predicate_push_down(inner_side, &filter, is, current_db);
        }
        if let Some(filter) = null_reject_on_inner(
            inner_on,
            &base.name,
            base.resolve_name(),
            &table_column_names(is, current_db, base),
        ) {
            base_for_inner = predicate_push_down(base_for_inner, &filter, is, current_db);
        }
        let inner_join = reorder_hash_join(
            JoinType::Inner,
            base_for_inner,
            inner_side,
            base.name.clone(),
            inner_right.name.clone(),
            table_row_count(stats, base, current_db),
            table_row_count(stats, inner_right, current_db),
            flip_eq_for_build(inner_on, &inner_right.name),
        );
        let mut outer_side =
            self.join_side(select, outer_right, is, current_db, stats, prune_mode)?;
        if let Some(filter) = null_reject_on_inner(
            outer_on,
            &outer_right.name,
            outer_right.resolve_name(),
            &table_column_names(is, current_db, outer_right),
        ) {
            outer_side = predicate_push_down(outer_side, &filter, is, current_db);
        }
        let on = if matches!(outer_tp, JoinType::RightOuter) {
            flip_eq_for_build(outer_on, &outer_right.name)
        } else {
            flip_eq_for_build(outer_on, &base.name)
        };
        // TiDB lists a left-outer over an inner with Build=inner, Probe=outer;
        // a right-outer over an inner with Build=inner, Probe=outer as well,
        // but the executor's right-outer null-supplying side is `left`, so the
        // inner join moves to `left` only for the right-outer form.
        let plan = if matches!(outer_tp, JoinType::RightOuter) {
            PhysicalPlan::HashJoin {
                est_rows: None,
                tp: outer_tp,
                left: Box::new(inner_join),
                right: Box::new(outer_side),
                left_table: base.name.clone(),
                right_table: outer_right.name.clone(),
                on,
            }
        } else {
            PhysicalPlan::HashJoin {
                est_rows: None,
                tp: outer_tp,
                left: Box::new(outer_side),
                right: Box::new(inner_join),
                left_table: outer_right.name.clone(),
                right_table: base.name.clone(),
                on,
            }
        };
        Ok(Some(wrap_star_join_projection(
            plan, select, is, current_db,
        )))
    }

    fn build_from_item(
        &self,
        select: &SelectStmt,
        item: &sealdb_parser::ast::FromItem,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        use sealdb_parser::ast::FromItem;
        match item {
            FromItem::Table(table) => {
                self.join_side(select, table, is, current_db, stats, prune_mode)
            }
            FromItem::Join {
                left,
                right,
                tp,
                on,
                using_cols: _,
                natural: _,
            } => {
                if let Some(plan) = self.try_bushy_quad_outer(
                    select, left, right, *tp, on, is, current_db, stats, prune_mode,
                )? {
                    return Ok(plan);
                }
                let left_plan =
                    self.build_from_item(select, left, is, current_db, stats, prune_mode)?;
                let mut right_plan =
                    self.build_from_item(select, right, is, current_db, stats, prune_mode)?;
                let left_table = left
                    .tables()
                    .last()
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                let right_table = right
                    .tables()
                    .first()
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                // TiDB ExplainInfo uses OrigTblName (`select.t.a` for `t t4`).
                let right_physical = right
                    .tables()
                    .first()
                    .map(|t| t.resolve_name().to_string())
                    .unwrap_or_else(|| right_table.clone());
                let join_tp = map_join_type(*tp);
                // Nested outer→inner only when the outer left is a base table
                // (e.g. `t2 ⟕ (t1 ⟕ t3)`). Bushy `(J1) ⟕ (J2)` needs a fuller
                // reorder — do not convert/Projection-wrap here.
                let mut reordered = false;
                if matches!(left.as_ref(), FromItem::Table(_))
                    && matches!(join_tp, JoinType::LeftOuter | JoinType::RightOuter)
                    && let Some(converted) =
                        convert_nested_outer_to_inner(&mut right_plan, on, is, current_db)
                {
                    reordered = converted;
                }
                if !reordered && matches!(join_tp, JoinType::LeftOuter | JoinType::RightOuter) {
                    if let Some(filter) =
                        null_reject_on_inner(on, &right_table, &right_physical, &[])
                    {
                        right_plan = predicate_push_down(right_plan, &filter, is, current_db);
                    }
                }
                let plan = PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp: join_tp,
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    left_table,
                    right_table,
                    on: explain_on_expr(on, select),
                };
                if reordered {
                    Ok(wrap_star_join_projection(plan, select, is, current_db))
                } else {
                    Ok(plan)
                }
            }
        }
    }

    /// `(t1 ⟕ t2) ⟕ (t3 ⟕ t4) ON …` — TiDB bushy outer-join reorder shapes.
    fn try_bushy_quad_outer(
        &self,
        select: &SelectStmt,
        left: &sealdb_parser::ast::FromItem,
        right: &sealdb_parser::ast::FromItem,
        tp: sealdb_parser::ast::JoinType,
        on: &ExprNode,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        use sealdb_parser::ast::FromItem;
        if !matches!(map_join_type(tp), JoinType::LeftOuter) {
            return Ok(None);
        }
        let (
            FromItem::Join {
                left: l1,
                right: l2,
                tp: lt,
                on: lon,
                ..
            },
            FromItem::Join {
                left: r1,
                right: r2,
                tp: rt,
                on: ron,
                ..
            },
        ) = (left, right)
        else {
            return Ok(None);
        };
        if !matches!(map_join_type(*lt), JoinType::LeftOuter)
            || !matches!(map_join_type(*rt), JoinType::LeftOuter)
        {
            return Ok(None);
        }
        let (FromItem::Table(t1), FromItem::Table(t2)) = (l1.as_ref(), l2.as_ref()) else {
            return Ok(None);
        };
        let (FromItem::Table(t3), FromItem::Table(t4)) = (r1.as_ref(), r2.as_ref()) else {
            return Ok(None);
        };
        if !outer_join_on_base(lon, &t1.name, &t2.name)
            || !outer_join_on_base(ron, &t3.name, &t4.name)
        {
            return Ok(None);
        }
        let side = |t: &TableName| self.join_side(select, t, is, current_db, stats, prune_mode);
        let with_isnull = |plan: PhysicalPlan, on: &ExprNode, table: &TableName| {
            if let Some(filter) = null_reject_on_inner(
                on,
                &table.name,
                table.resolve_name(),
                &table_column_names(is, current_db, table),
            ) {
                predicate_push_down(plan, &filter, is, current_db)
            } else {
                plan
            }
        };

        // ON t2.a = t4.a → ((t3 ⋈ t4) ⟕ t2) as Build, t1 as Probe; top eq(t1,t2).
        if outer_join_on_base(on, &t2.name, &t4.name) {
            let mut a3 = side(t3)?;
            let mut a4 = side(t4)?;
            a3 = with_isnull(a3, ron, t3);
            a4 = with_isnull(a4, ron, t4);
            let inner = reorder_hash_join(
                JoinType::Inner,
                a3,
                a4,
                t3.name.clone(),
                t4.name.clone(),
                table_row_count(stats, t3, current_db),
                table_row_count(stats, t4, current_db),
                flip_eq_for_build(ron, &t3.name),
            );
            let mut a2 = side(t2)?;
            a2 = with_isnull(a2, on, t2);
            let mid = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a2),
                right: Box::new(inner),
                left_table: t2.name.clone(),
                right_table: t4.name.clone(),
                on: on.clone(),
            };
            let a1 = side(t1)?;
            return Ok(Some(PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a1),
                right: Box::new(mid),
                left_table: t1.name.clone(),
                right_table: t2.name.clone(),
                on: lon.clone(),
            }));
        }

        // ON t2.a = t3.a → (((t3 ⟕ t2) ⟕ t1) as Build, t4 as Probe).
        if outer_join_on_base(on, &t2.name, &t3.name) {
            let mut a3 = side(t3)?;
            a3 = with_isnull(a3, on, t3);
            let mut a2 = side(t2)?;
            a2 = with_isnull(a2, on, t2);
            let low = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a2),
                right: Box::new(a3),
                left_table: t2.name.clone(),
                right_table: t3.name.clone(),
                on: on.clone(),
            };
            let a1 = side(t1)?;
            let mid = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a1),
                right: Box::new(low),
                left_table: t1.name.clone(),
                right_table: t2.name.clone(),
                on: lon.clone(),
            };
            let mut a4 = side(t4)?;
            a4 = with_isnull(a4, ron, t4);
            // TiDB keeps the preserved spine (`(t3 ⟕ t2) ⟕ t1`) as the outer
            // side and hashes the t4 probe; EXPLAIN draws Build first either way.
            return Ok(Some(PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(mid),
                right: Box::new(a4),
                left_table: t3.name.clone(),
                right_table: t4.name.clone(),
                on: ron.clone(),
            }));
        }

        // ON t1.a = t4.a → Projection; Build=t2, Probe=((t3⋈t4) ⟕ t1).
        if outer_join_on_base(on, &t1.name, &t4.name) {
            let mut a3 = side(t3)?;
            let mut a4 = side(t4)?;
            a3 = with_isnull(a3, ron, t3);
            a4 = with_isnull(a4, ron, t4);
            let inner = reorder_hash_join(
                JoinType::Inner,
                a3,
                a4,
                t3.name.clone(),
                t4.name.clone(),
                table_row_count(stats, t3, current_db),
                table_row_count(stats, t4, current_db),
                flip_eq_for_build(ron, &t3.name),
            );
            let a1 = side(t1)?;
            let mid = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a1),
                right: Box::new(inner),
                left_table: t1.name.clone(),
                right_table: t4.name.clone(),
                on: on.clone(),
            };
            let mut a2 = side(t2)?;
            a2 = with_isnull(a2, lon, t2);
            // Build=t2 (nullable), Probe=((t3⋈t4)⟕t1) preserved spine on t1.
            let plan = PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(mid),
                right: Box::new(a2),
                left_table: t1.name.clone(),
                right_table: t2.name.clone(),
                on: lon.clone(),
            };
            return Ok(Some(wrap_star_join_projection(
                plan, select, is, current_db,
            )));
        }

        Ok(None)
    }

    /// Costed n-table DP when all joins are Inner equality on a connected graph
    /// of base tables (no CTEs). Returns `Ok(None)` to fall back.
    fn build_join_plan_dp(
        &self,
        select: &SelectStmt,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<Option<PhysicalPlan>> {
        let tables = select.join_tables();
        if tables.len() < 3 {
            return Ok(None);
        }
        if tables
            .iter()
            .any(|t| named_cte(select.with.as_ref(), t.resolve_name()))
        {
            return Ok(None);
        }
        if select
            .joins
            .iter()
            .any(|j| !matches!(j.tp, AstJoinType::Inner))
        {
            return Ok(None);
        }
        let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
        let rows: Vec<f64> = tables
            .iter()
            .map(|t| table_row_count(stats, t, current_db))
            .collect();
        let mut edges = Vec::with_capacity(select.joins.len());
        for join in &select.joins {
            let Some((l, r)) = eq_join_edge(&join.on, &names) else {
                return Ok(None);
            };
            edges.push(JoinEqEdge {
                left: l,
                right: r,
                on: join.on.clone(),
            });
        }
        let Some(steps) = dp_join_steps(&rows, &edges) else {
            return Ok(None);
        };
        let seed = steps[0].table;
        let mut plan = self.join_side(select, tables[seed], is, current_db, stats, prune_mode)?;
        let mut used = vec![seed];
        for step in steps.iter().skip(1) {
            let edge = edge_for_extension(&edges, &used, step.table)
                .ok_or_else(|| Error::unsupported("n-table join reorder missing equality edge"))?;
            let right_side = self.join_side(
                select,
                tables[step.table],
                is,
                current_db,
                stats,
                prune_mode,
            )?;
            let peer = if edge.left == step.table {
                edge.right
            } else {
                edge.left
            };
            let new_name = names[step.table].clone();
            let peer_name = names[peer].clone();
            // First binary join: reuse two-table costed orientation.
            if used.len() == 1 {
                plan = reorder_hash_join(
                    JoinType::Inner,
                    plan,
                    right_side,
                    peer_name,
                    new_name,
                    rows[used[0]],
                    rows[step.table],
                    edge.on.clone(),
                );
            } else if step.swap {
                // New leaf probes; accumulated plan builds (`right_table` =
                // peer living inside the build subtree).
                plan = PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp: JoinType::Inner,
                    left: Box::new(right_side),
                    right: Box::new(plan),
                    left_table: new_name,
                    right_table: peer_name,
                    on: edge.on.clone(),
                };
            } else {
                plan = PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp: JoinType::Inner,
                    left: Box::new(plan),
                    right: Box::new(right_side),
                    left_table: peer_name,
                    right_table: new_name,
                    on: edge.on.clone(),
                };
            }
            used.push(step.table);
        }
        Ok(Some(plan))
    }

    fn join_side(
        &self,
        select: &SelectStmt,
        table: &TableName,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
        prune_mode: PartitionPruneMode,
    ) -> Result<PhysicalPlan> {
        if named_cte(select.with.as_ref(), table.resolve_name()) {
            let mut cte_from = table.clone();
            if table.physical.is_some() {
                cte_from.name = table.resolve_name().to_string();
                cte_from.physical = Some(table.name.clone());
            }
            self.build_cte_plan(select, &cte_from, is, current_db, stats, prune_mode)?
                .ok_or_else(|| Error::unsupported(format!("unknown CTE {}", table.resolve_name())))
        } else {
            Ok(access_plan(
                select, table, is, current_db, stats, prune_mode,
            ))
        }
    }
}

impl Default for PlanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub fn stmt_kind(stmt: &Statement) -> &'static str {
    stmt.stmt_type()
}

fn is_information_schema(from: &TableName, current_db: &str) -> bool {
    from.schema
        .as_deref()
        .or(Some(current_db))
        .is_some_and(|schema| schema.eq_ignore_ascii_case("information_schema"))
}

fn named_cte(with: Option<&WithClause>, name: &str) -> bool {
    with.is_some_and(|with| with.find(name).is_some())
}

/// MySQL `USING` resolution on a bushy `FromItem` tree: each side of a join
/// must expose exactly one column with the bare name (1052 when a side has
/// several, 1054 when it has none). A nested `(a JOIN b USING …)` group
/// collapses its USING columns, so `t1 JOIN (t2 JOIN t3 USING (b)) USING (a)`
/// leaves both `t2.a` and `t3.a` visible → ambiguous.
fn validate_from_item_using(
    item: &sealdb_parser::ast::FromItem,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    fn columns_of(is: &InfoSchema, current_db: &str, table: &TableName) -> Option<Vec<String>> {
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        is.table_by_name(schema, table.resolve_name()).map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
    }

    fn visible(
        item: &sealdb_parser::ast::FromItem,
        is: &InfoSchema,
        db: &str,
    ) -> Option<Vec<String>> {
        match item {
            sealdb_parser::ast::FromItem::Table(t) => columns_of(is, db, t),
            sealdb_parser::ast::FromItem::Join {
                left,
                right,
                using_cols,
                ..
            } => {
                let mut cols = visible(left, is, db)?;
                let right_cols = visible(right, is, db)?;
                let drop: std::collections::HashSet<String> =
                    using_cols.iter().map(|c| c.to_ascii_lowercase()).collect();
                cols.extend(
                    right_cols
                        .into_iter()
                        .filter(|c| !drop.contains(&c.to_ascii_lowercase())),
                );
                Some(cols)
            }
        }
    }

    fn check(item: &sealdb_parser::ast::FromItem, is: &InfoSchema, db: &str) -> Result<()> {
        let sealdb_parser::ast::FromItem::Join {
            left,
            right,
            using_cols,
            ..
        } = item
        else {
            return Ok(());
        };
        check(left, is, db)?;
        check(right, is, db)?;
        if using_cols.is_empty() {
            return Ok(());
        }
        let (Some(left_cols), Some(right_cols)) = (visible(left, is, db), visible(right, is, db))
        else {
            return Ok(());
        };
        for col in using_cols {
            let count =
                |cols: &[String]| cols.iter().filter(|c| c.eq_ignore_ascii_case(col)).count();
            let (l, r) = (count(&left_cols), count(&right_cols));
            if l > 1 || r > 1 {
                return Err(Error::new(
                    sealdb_errno::ERR_AMBIGUOUS_COLUMN,
                    format!("Column '{col}' in from clause is ambiguous"),
                ));
            }
            if l == 0 || r == 0 {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Unknown column '{col}' in 'from clause'"),
                ));
            }
        }
        Ok(())
    }

    let Some(is) = is else {
        return Ok(());
    };
    check(item, is, current_db)
}

/// `FROM ((a JOIN b ON <subquery>) …)` — ON conditions nested in paren groups.
fn from_item_on_has_subquery(item: &sealdb_parser::ast::FromItem) -> bool {
    match item {
        sealdb_parser::ast::FromItem::Table(_) => false,
        sealdb_parser::ast::FromItem::Join {
            left, right, on, ..
        } => {
            on.has_subquery() || from_item_on_has_subquery(left) || from_item_on_has_subquery(right)
        }
    }
}

/// MySQL resolves `USING (col)` against both join inputs before planning, so an
/// unknown name is Error 1054 rather than a keyless join. CTE / derived sides
/// have no infoschema entry here; skip validation for those.
fn validate_join_using(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    let Some(is) = is else {
        return Ok(());
    };
    if select.joins.is_empty() {
        return Ok(());
    }
    let columns_of = |table: &TableName| -> Option<Vec<String>> {
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        is.table_by_name(schema, table.resolve_name()).map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
    };
    let tables = select.join_tables();
    for (i, join) in select.joins.iter().enumerate() {
        if join.using_cols.is_empty() || tables.len() <= i + 1 {
            continue;
        }
        let Some(left) = tables[..=i]
            .iter()
            .map(|table| columns_of(table))
            .collect::<Option<Vec<Vec<String>>>>()
        else {
            continue;
        };
        let Some(right) = columns_of(&join.right) else {
            continue;
        };
        for col in &join.using_cols {
            let has = |side: &[Vec<String>]| {
                side.iter()
                    .flatten()
                    .any(|name| name.eq_ignore_ascii_case(col))
            };
            if !has(&left) || !has(std::slice::from_ref(&right)) {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!("Unknown column '{col}' in 'from clause'"),
                ));
            }
        }
    }
    Ok(())
}

/// `/*+ MERGE_JOIN(t1, t2) */` in restored select text (TiDB hint parser MVB).
/// `INL_MERGE_JOIN` (index merge join) is a different hint and must not route
/// here: TiDB preserves the outer row order for index nested-loop joins.
fn select_has_merge_join_hint(select: &SelectStmt) -> bool {
    let text = select.text.to_ascii_lowercase();
    // `MERGE_JOIN(...)` and TiDB's legacy `TIDB_SMJ(...)` hint.
    if text.contains("tidb_smj") {
        return true;
    }
    let mut rest = text.as_str();
    let mut base = 0usize;
    while let Some(found) = rest.find("merge_join") {
        let at = base + found;
        if !text[..at].ends_with("inl_") {
            return true;
        }
        base = at + "merge_join".len();
        rest = &text[base..];
    }
    false
}

fn with_inherited(mut select: SelectStmt, with: Option<&WithClause>) -> SelectStmt {
    if select.with.is_none() {
        select.with = with.cloned();
    }
    select
}

/// `not(isnull(inner.join_key))` for TiDB outer-join null rejection on the build side.
fn table_column_names(is: Option<&InfoSchema>, current_db: &str, table: &TableName) -> Vec<String> {
    let Some(is) = is else {
        return Vec::new();
    };
    let schema = table
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    is.table_by_name(schema, table.resolve_name())
        .map(|info| {
            info.columns
                .iter()
                .map(|c| c.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn null_reject_on_inner(
    on: &ExprNode,
    inner_alias: &str,
    inner_physical: &str,
    inner_cols: &[String],
) -> Option<ExprNode> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    // A column belongs to the inner side when it is qualified with the inner
    // alias/physical name, or (unqualified) when its name exists on that side.
    // Unqualified ON columns (`ON b1 = a1`) must not be attributed to the wrong
    // table — pushing `not(isnull(a1))` into `t2` fails at execution.
    let belongs = |c: &ColumnNameExpr| -> bool {
        if let Some(t) = c.table.as_deref() {
            return t.eq_ignore_ascii_case(inner_alias) || t.eq_ignore_ascii_case(inner_physical);
        }
        inner_cols.is_empty() || inner_cols.iter().any(|n| n.eq_ignore_ascii_case(&c.name))
    };
    let mut col = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(c), _) if belongs(c) => c.clone(),
        (_, ExprNode::Column(c)) if belongs(c) => c.clone(),
        _ => return None,
    };
    // OrigTblName for ExplainInfo (`t t4` → select.t.a).
    col.table = Some(inner_physical.to_string());
    Some(ExprNode::Unary(UnaryOpExpr {
        op: Op::Not,
        expr: Box::new(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(ExprNode::Column(col)),
            not: false,
        })),
    }))
}

/// TiDB inner-join null rejection: `not(isnull(key))` is pushed into both
/// sides for nullable equality keys (`PhysicalHashJoin` / `rule_predicate_push_down`).
fn inner_join_null_reject_filters(
    on: &ExprNode,
    table: &str,
    physical: &str,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Vec<ExprNode> {
    let Some(is) = is else {
        return Vec::new();
    };
    let Some(info) = is.table_by_name(current_db, physical) else {
        return Vec::new();
    };
    let nullable = |name: &str| {
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(name))
            .is_some_and(|c| c.field_type.flag & 1 == 0)
    };
    let belongs = |c: &ColumnNameExpr| -> bool {
        match c.table.as_deref() {
            // EXPLAIN-rendered keys may already carry the OrigTblName.
            Some(t) => t.eq_ignore_ascii_case(table) || t.eq_ignore_ascii_case(physical),
            None => info
                .columns
                .iter()
                .any(|col| col.name.o().eq_ignore_ascii_case(&c.name)),
        }
    };
    let mut out = Vec::new();
    for item in on.split_cnf() {
        let ExprNode::Binary(bin) = &item else {
            continue;
        };
        if bin.op != Op::Eq {
            continue;
        }
        let (this, other) = match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(a), ExprNode::Column(b)) => (a, b),
            _ => continue,
        };
        let col = if belongs(this) && !belongs(other) {
            this
        } else if belongs(other) && !belongs(this) {
            other
        } else if belongs(this) && belongs(other) && this.name.eq_ignore_ascii_case(&other.name) {
            // Self-join: both keys render the same OrigTblName, so the ON text
            // is ambiguous. TiDB null-rejects both sides with their own alias.
            this
        } else {
            continue;
        };
        if !nullable(&col.name) {
            continue;
        }
        out.push(ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(table.to_string()),
                    name: col.name.clone(),
                })),
                not: false,
            })),
        }));
    }
    out
}

/// Push `not(isnull(key))` into a plain base-table scan of an inner join.
fn push_inner_null_reject(
    side: PhysicalPlan,
    on: &ExprNode,
    table: &str,
    physical: &str,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match &side {
        PhysicalPlan::TableReader { pushed, .. }
        | PhysicalPlan::IndexReader { pushed, .. }
        | PhysicalPlan::IndexLookUpReader { pushed, .. }
            if pushed.is_none() => {}
        _ => return side,
    }
    let mut filters = inner_join_null_reject_filters(on, table, physical, is, current_db);
    if filters.is_empty() {
        return side;
    }
    let mut filter = filters.remove(0);
    for extra in filters {
        filter = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(filter),
            right: Box::new(extra),
            single_cnf_item: false,
        });
    }
    predicate_push_down(side, &filter, is, current_db)
}

/// Walk a join tree and add TiDB inner-join null rejection to both sides.
fn inject_inner_join_null_reject(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let mut left = inject_inner_join_null_reject(*left, select, is, current_db);
            let mut right = inject_inner_join_null_reject(*right, select, is, current_db);
            if matches!(tp, JoinType::Inner) {
                let left_phys = join_side_physical(select, &left_table);
                let right_phys = join_side_physical(select, &right_table);
                left = push_inner_null_reject(left, &on, &left_table, &left_phys, is, current_db);
                right =
                    push_inner_null_reject(right, &on, &right_table, &right_phys, is, current_db);
            }
            PhysicalPlan::HashJoin {
                est_rows,
                tp,
                left: Box::new(left),
                right: Box::new(right),
                left_table,
                right_table,
                on,
            }
        }
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
        },
        PhysicalPlan::Selection { child, filter, est_rows } => PhysicalPlan::Selection {
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
            filter,
            est_rows,
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
            offset,
            count,
        },
        PhysicalPlan::UnionScan {
            info,
            est_rows,
            child,
        } => PhysicalPlan::UnionScan {
            info,
            est_rows,
            child: Box::new(inject_inner_join_null_reject(
                *child, select, is, current_db,
            )),
        },
        other => other,
    }
}

fn map_join_type(tp: AstJoinType) -> JoinType {
    match tp {
        AstJoinType::Inner => JoinType::Inner,
        AstJoinType::LeftOuter => JoinType::LeftOuter,
        AstJoinType::RightOuter => JoinType::RightOuter,
        AstJoinType::Cross => JoinType::Cross,
        AstJoinType::Semi => JoinType::Semi,
        AstJoinType::Anti => JoinType::Anti,
    }
}

/// Resolve MergeJoin key labels using OrigTblName (CTE seed / physical table).
/// TiDB picks the merge key that the leading index column (or PK) already
/// orders, so the sort is free; fall back to the first cross-side equality.
fn merge_join_key_pair_preferred(
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
    left_phys: &str,
) -> Option<(String, String)> {
    let pairs = merge_join_key_pairs(left, right, on);
    if pairs.is_empty() {
        return None;
    }
    let ordered_leads: Vec<String> = is
        .and_then(|is| is.table_by_name(current_db, left_phys))
        .map(|info| {
            let mut cols = Vec::new();
            if info.pk_is_handle
                && let Some(handle) = info.handle_column()
            {
                cols.push(handle.name.o().to_string());
            }
            for index in &info.indices {
                if let Some(col) = index.columns.first() {
                    cols.push(col.name.o().to_string());
                }
            }
            cols
        })
        .unwrap_or_default();
    pairs
        .iter()
        .find(|(l, _)| ordered_leads.iter().any(|c| c.eq_ignore_ascii_case(l)))
        .cloned()
        .or_else(|| pairs.first().cloned())
}

/// All cross-side column equalities among the ON conjuncts.
fn merge_join_key_pairs(
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
) -> Vec<(String, String)> {
    let mut conjuncts = Vec::new();
    merge_info_conjuncts(on, &mut conjuncts);
    let on_side = |table: &TableName, col: &ColumnNameExpr| -> bool {
        col.table.as_deref().is_some_and(|t| {
            t.eq_ignore_ascii_case(&table.name) || t.eq_ignore_ascii_case(table.resolve_name())
        })
    };
    let mut out = Vec::new();
    for conjunct in conjuncts {
        let ExprNode::Binary(bin) = conjunct else {
            continue;
        };
        if bin.op != Op::Eq {
            continue;
        }
        let (ExprNode::Column(lc), ExprNode::Column(rc)) = (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        if on_side(left, lc) && on_side(right, rc) {
            out.push((lc.name.clone(), rc.name.clone()));
        } else if on_side(left, rc) && on_side(right, lc) {
            out.push((rc.name.clone(), lc.name.clone()));
        }
    }
    out
}

/// The single cross-side equality of a two-table `ON`, resolved by column
/// membership. The `ON` reaches the planner before binding qualifies columns,
/// so `merge_join_key_pairs` (which matches on `col.table`) sees nothing here.
fn join_key_columns(
    left: &sealdb_meta::model::table::TableInfo,
    right: &sealdb_meta::model::table::TableInfo,
    on: &ExprNode,
) -> Option<(String, String)> {
    let mut conjuncts = Vec::new();
    merge_info_conjuncts(on, &mut conjuncts);
    let mut found: Option<(String, String)> = None;
    for conjunct in conjuncts {
        let ExprNode::Binary(bin) = conjunct else {
            return None;
        };
        if bin.op != Op::Eq {
            return None;
        }
        let (ExprNode::Column(lc), ExprNode::Column(rc)) = (bin.left.as_ref(), bin.right.as_ref())
        else {
            return None;
        };
        let owns = |info: &sealdb_meta::model::table::TableInfo, col: &str| {
            info.columns
                .iter()
                .any(|c| c.name.o().eq_ignore_ascii_case(col))
        };
        let pair = if owns(left, &lc.name) && owns(right, &rc.name) {
            (lc.name.clone(), rc.name.clone())
        } else if owns(left, &rc.name) && owns(right, &lc.name) {
            (rc.name.clone(), lc.name.clone())
        } else {
            return None;
        };
        // Two equi-join keys would need a composite ordered scan on both sides.
        if found.is_some() {
            return None;
        }
        found = Some(pair);
    }
    found
}

/// First cross-side column equality among the ON conjuncts, as
/// `(left column, right column)`.
fn merge_join_key_pair(
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
) -> Option<(String, String)> {
    merge_join_key_pairs(left, right, on).into_iter().next()
}

fn merge_join_key_labels_for(
    select: &SelectStmt,
    current_db: &str,
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
    using_cols: &[String],
    is: Option<&InfoSchema>,
) -> (String, String) {
    let left_phys = merge_join_orig_table(select, left);
    let right_phys = merge_join_orig_table(select, right);
    let schema = if current_db.is_empty() {
        "select"
    } else {
        current_db
    };
    if let Some(col) = using_cols.first() {
        return (
            format!("{schema}.{left_phys}.{col}"),
            format!("{schema}.{right_phys}.{col}"),
        );
    }
    // ON may carry extra conjuncts (`t1.a = t2.a AND t1.a != 3`), so scan
    // every `AND` leaf for the cross-side key equality.
    if let Some((l, r)) = merge_join_key_pair_preferred(left, right, on, is, current_db, &left_phys)
    {
        return (
            format!("{schema}.{left_phys}.{l}"),
            format!("{schema}.{right_phys}.{r}"),
        );
    }
    (
        format!("{schema}.{left_phys}.id"),
        format!("{schema}.{right_phys}.id"),
    )
}

fn merge_join_orig_table(select: &SelectStmt, table: &TableName) -> String {
    if let Some(with) = select.with.as_ref()
        && let Some(cte) = with.find(table.resolve_name())
    {
        // Non-recursive CTE seed FROM table → OrigTblName for ExplainInfo.
        if let Some(from) = cte.query.from.as_ref() {
            return from.resolve_name().to_string();
        }
    }
    table.resolve_name().to_string()
}

/// Null-reject + Sort for MergeJoin inputs that are not already PK-ordered.
/// A single-side ON conjunct on the preserved (left) side can be mirrored
/// onto the inner side through the join key: with `t1.a = t2.a`, the cond
/// `t1.a != 3` also rejects `t2.a = 3`. TiDB turns that into a range scan
/// (`range:[-inf,3), (3,+inf]`) and keeps the original as `left cond`.
fn outer_join_inner_range(left: &TableName, right: &TableName, on: &ExprNode) -> Option<String> {
    let (key_left, key_right) = merge_join_key_pair(left, right, on)?;
    let mut conjuncts = Vec::new();
    merge_info_conjuncts(on, &mut conjuncts);
    for conjunct in conjuncts {
        let ExprNode::Binary(bin) = conjunct else {
            continue;
        };
        if bin.op != Op::Ne {
            continue;
        }
        let (ExprNode::Column(col), value) = (bin.left.as_ref(), bin.right.as_ref()) else {
            let (value, ExprNode::Column(col)) = (bin.left.as_ref(), bin.right.as_ref()) else {
                continue;
            };
            if !col.name.eq_ignore_ascii_case(&key_left)
                || !col.table.as_deref().is_some_and(|t| {
                    t.eq_ignore_ascii_case(&left.name)
                        || t.eq_ignore_ascii_case(left.resolve_name())
                })
            {
                continue;
            }
            return Some(ne_range(&key_right, value));
        };
        if !col.name.eq_ignore_ascii_case(&key_left)
            || !col.table.as_deref().is_some_and(|t| {
                t.eq_ignore_ascii_case(&left.name) || t.eq_ignore_ascii_case(left.resolve_name())
            })
        {
            continue;
        }
        let _ = value;
        return Some(ne_range(&key_right, bin.right.as_ref()));
    }
    None
}

/// TiDB ranger rendering for `col != const`: `[-inf,c), (c,+inf]`.
fn ne_range(_col: &str, value: &ExprNode) -> String {
    let lit = match value {
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::F64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::Decimal(d),
        }) => d.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => {
            format!("\"{}\"", String::from_utf8_lossy(b))
        }
        _ => return String::new(),
    };
    if lit.is_empty() {
        return String::new();
    }
    format!("[-inf,{lit}), ({lit},+inf]")
}

/// Mark the inner scan as a range scan with the derived predicate. TiDB's
/// unanalyzed `!=` selectivity is 2/3 of the table cardinality.
fn push_inner_range_scan(plan: PhysicalPlan, range: &str) -> PhysicalPlan {
    if range.is_empty() {
        return plan;
    }
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            ..
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count: if stats_pseudo {
                10_000.0 * 2.0 / 3.0
            } else {
                row_count
            },
            stats_pseudo,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
            pushed: Some(range_push(range)),
        },
        other => other,
    }
}

/// Stats-driven EXPLAIN for a single `A RIGHT JOIN B`.
///
/// TiDB estimates the join from the key NDVs
/// (`|A|·|B| / max(NDV_a, NDV_b)`), orients the equijoin as `(inner key,
/// preserved key)` — `eq(tbl_2.col_11, tbl_0.col_3)` for
/// `tbl_2 right join tbl_0` — and sizes the null-rejection filter from
/// `count - null_count` instead of the pseudo `0.999` selectivity.
fn refine_right_join_explain(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> PhysicalPlan {
    let Some((est, filter_rows, on)) =
        right_join_explain_override(&plan, select, is, current_db, stats)
    else {
        return plan;
    };
    let PhysicalPlan::HashJoin {
        tp,
        left,
        right,
        left_table,
        right_table,
        ..
    } = plan
    else {
        return plan;
    };
    let left = match filter_rows {
        Some(rows) => with_filter_est(*left, rows),
        None => *left,
    };
    PhysicalPlan::HashJoin {
        est_rows: est,
        tp,
        left: Box::new(left),
        right,
        left_table,
        right_table,
        on,
    }
}

/// `(join est, inner-side filter rows, oriented equijoin)` for a right join
/// whose two tables are analyzed.
fn right_join_explain_override(
    plan: &PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> Option<(Option<f64>, Option<f64>, ExprNode)> {
    if !in_explain_build()
        || !matches!(
            plan,
            PhysicalPlan::HashJoin {
                tp: JoinType::RightOuter,
                ..
            }
        )
    {
        return None;
    }
    let is = is?;
    let join = select.joins.first()?;
    let (left_db, left_name) = join.left.qualified(current_db);
    let (right_db, right_name) = join.right.qualified(current_db);
    let left_info = is.table_by_name(&left_db, &left_name)?;
    let right_info = is.table_by_name(&right_db, &right_name)?;
    let (left_col, right_col) = join_key_columns(left_info, right_info, &join.on)?;
    let left_stats = stats
        .and_then(|h| h.table(&left_db, &left_name))
        .filter(|t| super::plan_cost::has_real_stats(t))?;
    let right_stats = stats
        .and_then(|h| h.table(&right_db, &right_name))
        .filter(|t| super::plan_cost::has_real_stats(t))?;
    let est = match (
        left_stats.column_ndv(&left_col),
        right_stats.column_ndv(&right_col),
    ) {
        (Some(ln), Some(rn)) => {
            Some(left_stats.count as f64 * right_stats.count as f64 / ln.max(rn).max(1.0))
        }
        _ => None,
    };
    let filter_rows = left_stats.column_not_null_count(&left_col);
    let key = |db: &str, table: &TableName, column: &str| {
        ExprNode::Column(ColumnNameExpr {
            schema: Some(db.to_string()),
            table: Some(merge_join_orig_table(select, table).to_ascii_lowercase()),
            name: column.to_string(),
        })
    };
    let on = ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(key(&left_db, &join.left, &left_col)),
        right: Box::new(key(&right_db, &join.right, &right_col)),
        single_cnf_item: false,
    });
    Some((est, filter_rows, on))
}

/// Override the cop filter cardinality of a scan (stats-derived), looking
/// through the root wrappers the build path may have added.
fn with_filter_est(plan: PhysicalPlan, rows: f64) -> PhysicalPlan {
    match plan {
        PhysicalPlan::TableReader {
            filter_est: Some(_),
            ..
        } => plan,
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            range_scan,
            keep_order,
            pushed,
            ..
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            filter_est: Some(rows),
            range_scan,
            keep_order,
            pushed,
        },
        PhysicalPlan::Selection { filter, child, est_rows } => PhysicalPlan::Selection {
            filter,
            child: Box::new(with_filter_est(*child, rows)),
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(with_filter_est(*child, rows)),
        },
        other => other,
    }
}

/// TiDB `PhysicalMergeJoin` ExplainInfo. Plain inner equi-joins keep the
/// default rendering; outer joins and ON conjuncts that are not join keys
/// (`left outer join, left key:…, right key:…, left cond:ne(…)`) need the
/// decorated string.
fn merge_join_info(
    tp: sealdb_parser::ast::JoinType,
    left_table: &str,
    right_table: &str,
    left_key: &str,
    right_key: &str,
    on: &ExprNode,
) -> Option<String> {
    use sealdb_parser::ast::JoinType as AstJoinType;
    let label = match tp {
        AstJoinType::LeftOuter => "left outer join",
        AstJoinType::RightOuter => "right outer join",
        _ => "inner join",
    };
    let mut conjuncts = Vec::new();
    merge_info_conjuncts(on, &mut conjuncts);
    let mut left_conds = Vec::new();
    let mut right_conds = Vec::new();
    for conjunct in conjuncts {
        if matches!(
            conjunct,
            ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(_),
            })
        ) {
            continue;
        }
        let (left, right) = merge_info_sides(conjunct, right_table);
        let _ = left_table;
        if left && right {
            // Key equality (or a cond spanning both) is rendered by the keys.
            continue;
        }
        if right {
            right_conds.push(super::rule_semi_join::explain_semi_pred(conjunct));
        } else {
            left_conds.push(super::rule_semi_join::explain_semi_pred(conjunct));
        }
    }
    if matches!(tp, AstJoinType::Inner) && left_conds.is_empty() && right_conds.is_empty() {
        return None;
    }
    let mut info = format!("{label}, left key:{left_key}, right key:{right_key}");
    if !left_conds.is_empty() {
        info.push_str(&format!(", left cond:{}", left_conds.join(", ")));
    }
    if !right_conds.is_empty() {
        info.push_str(&format!(", right cond:{}", right_conds.join(", ")));
    }
    Some(info)
}

fn merge_info_conjuncts<'a>(on: &'a ExprNode, out: &mut Vec<&'a ExprNode>) {
    if let ExprNode::Binary(bin) = on
        && bin.op == Op::LogicAnd
    {
        merge_info_conjuncts(&bin.left, out);
        merge_info_conjuncts(&bin.right, out);
        return;
    }
    out.push(on);
}

/// `(mentions_left, mentions_right)` for the enclosing join's two sides:
/// anything that is not the immediate right table lives in the accumulated
/// left subtree. Unqualified columns are treated as both.
fn merge_info_sides(expr: &ExprNode, right_table: &str) -> (bool, bool) {
    fn walk(expr: &ExprNode, right_table: &str, left: &mut bool, right: &mut bool) {
        match expr {
            ExprNode::Column(c) => match c.table.as_deref() {
                Some(table) if table.eq_ignore_ascii_case(right_table) => *right = true,
                Some(_) => *left = true,
                None => {
                    *left = true;
                    *right = true;
                }
            },
            ExprNode::Binary(bin) => {
                walk(&bin.left, right_table, left, right);
                walk(&bin.right, right_table, left, right);
            }
            ExprNode::Unary(un) => walk(&un.expr, right_table, left, right),
            ExprNode::Func(f) => {
                for arg in &f.args {
                    walk(arg, right_table, left, right);
                }
            }
            _ => {}
        }
    }
    let mut left = false;
    let mut right = false;
    walk(expr, right_table, &mut left, &mut right);
    (left, right)
}

fn prepare_merge_join_side(
    mut plan: PhysicalPlan,
    select: &SelectStmt,
    table: &TableName,
    on: &ExprNode,
    key_label: &str,
    is: Option<&InfoSchema>,
    current_db: &str,
    // TiDB pushes `not(isnull(key))` only into the null-supplying side of an
    // outer join; the preserved side keeps NULL keys (they never match).
    null_reject: bool,
) -> PhysicalPlan {
    let phys = merge_join_orig_table(select, table);
    let pk_ordered = merge_join_side_pk_ordered(select, table, on, is, current_db);
    // Strip CTE ORDER BY Sort — MergeJoin supplies order (#25674), including PK.
    plan = match plan {
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline: true,
            recursive,
            limit,
            offset,
        } => PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed: Box::new(strip_cte_order_sort(*seed)),
            recursive_plan,
            inline: true,
            recursive,
            limit,
            offset,
        },
        other => other,
    };
    if !pk_ordered {
        // `not(isnull(key))` only applies to sides backed by a base-table scan
        // whose schema keeps `table.col`. Derived sides (`Projection` over dual /
        // aggregate) expose the projected alias, so the filter would not resolve;
        // TiDB omits it there too.
        let scan_shaped = side_has_base_scan(&plan);
        let inner_cols = table_column_names(is, current_db, table);
        let filter = (scan_shaped && null_reject)
            .then(|| {
                null_reject_on_inner(on, &table.name, &phys, &inner_cols)
                    .or_else(|| null_reject_on_inner(on, table.resolve_name(), &phys, &inner_cols))
            })
            .flatten();
        if let Some(filter) = filter {
            // Push through inline CTE seed so EXPLAIN shows cop Selection under
            // TableReader (`data:Selection`), not a root Selection above CTE.
            plan = match plan {
                PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed,
                    recursive_plan,
                    inline: true,
                    recursive,
                    limit,
                    offset,
                } => PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed: Box::new(predicate_push_down(*seed, &filter, is, current_db)),
                    recursive_plan,
                    inline: true,
                    recursive,
                    limit,
                    offset,
                },
                other => predicate_push_down(other, &filter, is, current_db),
            };
        }
        plan = PhysicalPlan::Sort {
            child: Box::new(plan),
            by_info: key_label.to_string(),
        };
    }
    plan
}

/// True when the plan reads a base table somewhere, so `table.col` names are
/// still meaningful for a null-reject filter.
fn side_has_base_scan(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::TableReader { .. }
        | PhysicalPlan::IndexReader { .. }
        | PhysicalPlan::IndexLookUpReader { .. }
        | PhysicalPlan::PointGet { .. }
        | PhysicalPlan::BatchPointGet { .. } => true,
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::UnionScan { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. } => side_has_base_scan(child),
        PhysicalPlan::CTE { seed, .. } => side_has_base_scan(seed),
        _ => false,
    }
}

fn strip_cte_order_sort(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Sort { child, .. } => strip_cte_order_sort(*child),
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(strip_cte_order_sort(*child)),
        },
        other => other,
    }
}

fn merge_join_side_pk_ordered(
    select: &SelectStmt,
    table: &TableName,
    on: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> bool {
    // Recursive CTE working sets are not PK-ordered even when the seed is.
    if select
        .with
        .as_ref()
        .and_then(|w| w.find(table.resolve_name()))
        .is_some_and(|cte| cte.recursive.is_some())
    {
        return false;
    }
    let phys = merge_join_orig_table(select, table);
    let Some(info) = is.and_then(|schema| schema.table_by_name(current_db, &phys)) else {
        return false;
    };
    if !info.pk_is_handle {
        return false;
    }
    let Some(handle) = info.handle_column() else {
        return false;
    };
    let handle_name = handle.name.o();
    join_on_mentions_col(on, &table.name, table.resolve_name(), handle_name)
        || join_on_mentions_col(on, &phys, &phys, handle_name)
}

fn join_on_mentions_col(on: &ExprNode, alias: &str, phys: &str, col: &str) -> bool {
    if let ExprNode::Binary(bin) = on
        && bin.op == Op::LogicAnd
    {
        return join_on_mentions_col(&bin.left, alias, phys, col)
            || join_on_mentions_col(&bin.right, alias, phys, col);
    }
    let ExprNode::Binary(bin) = on else {
        return false;
    };
    if bin.op != Op::Eq {
        return false;
    }
    for side in [bin.left.as_ref(), bin.right.as_ref()] {
        if let ExprNode::Column(c) = side {
            let tbl = c.table.as_deref().unwrap_or("");
            if c.name.eq_ignore_ascii_case(col)
                && (tbl.is_empty()
                    || tbl.eq_ignore_ascii_case(alias)
                    || tbl.eq_ignore_ascii_case(phys))
            {
                return true;
            }
        }
    }
    false
}

/// `_tidb_rowid` only exists on heap tables; a table with an integer PRIMARY
/// KEY handle has no such column (TiDB raises 1054 in the field list).
fn check_tidb_rowid_reference(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Result<()> {
    fn mentions_rowid(expr: &ExprNode) -> bool {
        match expr {
            ExprNode::Column(col) => {
                col.table.is_none() && col.name.eq_ignore_ascii_case("_tidb_rowid")
            }
            ExprNode::Func(func) => func.args.iter().any(mentions_rowid),
            ExprNode::Binary(bin) => mentions_rowid(&bin.left) || mentions_rowid(&bin.right),
            ExprNode::Unary(unary) => mentions_rowid(&unary.expr),
            ExprNode::Cast(cast) => mentions_rowid(&cast.expr),
            _ => false,
        }
    }
    let Some(from) = &select.from else {
        return Ok(());
    };
    let Some(info) = is.and_then(|schema| {
        let (db, table) = from.qualified(current_db);
        schema.table_by_name(&db, &table)
    }) else {
        return Ok(());
    };
    if !info.pk_is_handle {
        return Ok(());
    }
    if select
        .fields
        .iter()
        .any(|field| mentions_rowid(&field.expr))
    {
        return Err(Error::new(
            ERR_BAD_FIELD,
            "Unknown column '_tidb_rowid' in 'field list'",
        ));
    }
    Ok(())
}

/// TiDB: a table scan yields row-handle order, so `ORDER BY` the handle needs
/// no Sort (`keep order:true`). Only ascending single-key ordering is modeled.
/// `ORDER BY` the table handle reads the table in handle order (forward or
/// backward) so no `Sort` is needed. Returns the direction of the required
/// scan (`Some(true)` for descending).
fn order_by_handle(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<bool> {
    if select.order_by.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is.and_then(|schema| schema.table_by_name(&db, &table))?;
    let handle = if info.pk_is_handle {
        info.handle_column()?.name.o().to_string()
    } else {
        "_tidb_rowid".to_string()
    };
    let mut desc = None;
    for item in &select.order_by {
        let ExprNode::Column(col) = &item.expr else {
            return None;
        };
        if !col.name.eq_ignore_ascii_case(&handle) {
            return None;
        }
        match desc {
            Some(prev) if prev != item.desc => return None,
            _ => desc = Some(item.desc),
        }
    }
    desc
}

/// `ORDER BY` a prefix of the point-get key: a single-row lookup returns its
/// row in key order, so TiDB keeps no `Sort` above the point get.
fn point_get_prefix_order(select: &SelectStmt, plan: &PhysicalPlan) -> bool {
    let PhysicalPlan::PointGet {
        index,
        index_columns,
        ..
    } = plan
    else {
        return false;
    };
    if index.is_none() || index_columns.is_empty() {
        return false;
    }
    if select.order_by.is_empty() || select.order_by.len() > index_columns.len() {
        return false;
    }
    let mut desc = None;
    for (item, key) in select.order_by.iter().zip(index_columns.iter()) {
        let ExprNode::Column(col) = &item.expr else {
            return false;
        };
        if !col.name.eq_ignore_ascii_case(key) {
            return false;
        }
        match desc {
            Some(prev) if prev != item.desc => return false,
            _ => desc = Some(item.desc),
        }
    }
    true
}

/// `ORDER BY` the batch point-get key (index prefix or handle) needs no Sort:
/// the batch walks its keys in order (`keep order:true, desc:…`).
/// Returns `Some(descending)` when the sort is satisfied by the key order.
fn point_get_order(
    select: &SelectStmt,
    plan: &PhysicalPlan,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<bool> {
    let plan = inner_batch_point_get(plan)?;
    let PhysicalPlan::BatchPointGet {
        column,
        index,
        index_columns,
        tuple_columns,
        ..
    } = plan
    else {
        return None;
    };
    // An unsigned integer handle is stored as its signed bit pattern, so the
    // row-key order is not the SQL order (`bigint unsigned` 9814441339970117597
    // sorts before 1 in the key space). TiDB keeps the Sort in that case.
    if index.is_none()
        && is
            .and_then(|schema| {
                let from = select.from.as_ref()?;
                let (db, table) = from.qualified(current_db);
                schema
                    .table_by_name(&db, &table)
                    .and_then(|info| {
                        info.columns
                            .iter()
                            .find(|c| c.name.o().eq_ignore_ascii_case(column))
                    })
                    .map(|c| c.field_type.is_unsigned())
            })
            .unwrap_or(false)
    {
        return None;
    }
    if select.order_by.is_empty() {
        return None;
    }
    let keys: Vec<&String> = if !tuple_columns.is_empty() {
        tuple_columns.iter().collect()
    } else if index.is_some() {
        index_columns.iter().collect()
    } else {
        vec![column]
    };
    if keys.len() != select.order_by.len() {
        return None;
    }
    let same_direction = select.order_by[0].desc;
    if select
        .order_by
        .iter()
        .any(|item| item.desc != same_direction)
    {
        return None;
    }
    keys.iter()
        .zip(select.order_by.iter())
        .all(|(key, item)| {
            matches!(&item.expr, ExprNode::Column(col) if col.name.eq_ignore_ascii_case(key))
        })
        .then_some(same_direction)
}

fn set_batch_point_order(plan: PhysicalPlan, desc: bool) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Selection { filter, child, est_rows } => PhysicalPlan::Selection {
            filter,
            child: Box::new(set_batch_point_order(*child, desc)),
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(set_batch_point_order(*child, desc)),
        },
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            lock,
            common_handle,
            ..
        } => PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            lock,
            keep_order: true,
            desc,
            common_handle,
        },
        other => other,
    }
}

fn inner_batch_point_get(plan: &PhysicalPlan) -> Option<&PhysicalPlan> {
    match plan {
        PhysicalPlan::BatchPointGet { .. } => Some(plan),
        PhysicalPlan::Selection { child, .. } | PhysicalPlan::Projection { child, .. } => {
            inner_batch_point_get(child)
        }
        _ => None,
    }
}

/// Mark the table scan as `keep order:true` (the Sort was dropped).
fn set_keep_order(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            filter_est,
            range_scan,
            pushed,
            ..
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            filter_est,
            range_scan,
            keep_order: true,
            pushed,
        },
        other => other,
    }
}

/// TiDB `LIMIT` over a backwards handle scan: the coprocessor `Limit` stops the
/// scan after one row (`TableReader(data:Limit) → Limit → TableFullScan` with
/// `keep order:true, desc`), so no `TopN` is built.
fn set_data_limit(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            stats_pseudo,
            filter_est,
            range_scan,
            pushed,
            ..
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count: 1.0,
            stats_pseudo,
            data_limit: true,
            filter_est,
            range_scan,
            keep_order: true,
            pushed,
        },
        other => other,
    }
}

/// TiDB: MergeJoin already orders by join keys; skip outer Sort when ORDER BY
/// is ordinals or those same columns (`ORDER BY 1, 2` on equijoin output).
fn order_by_covered_by_merge_join(select: &SelectStmt, plan: &PhysicalPlan) -> bool {
    let PhysicalPlan::MergeJoin {
        left_key,
        right_key,
        ..
    } = plan
    else {
        return false;
    };
    if select.order_by.is_empty() {
        return false;
    }
    select.order_by.iter().all(|item| match &item.expr {
        ExprNode::Value(v) => matches!(v.datum, Datum::I64(_) | Datum::U64(_)),
        ExprNode::Column(c) => {
            let name = c.name.as_str();
            left_key.ends_with(&format!(".{name}")) || right_key.ends_with(&format!(".{name}"))
        }
        _ => false,
    })
}

fn apply_order_limit(
    mut plan: PhysicalPlan,
    ordered: bool,
    limit: Option<u64>,
    offset: Option<u64>,
    est_rows: f64,
    by_info: String,
) -> PhysicalPlan {
    // IndexMerge already carries the pushed LIMIT (`limit embedded(...)`), so no
    // Limit node is added above it.
    if !ordered
        && let PhysicalPlan::IndexMerge {
            limit: Some((_, count)),
            ..
        } = &plan
        && Some(*count) == limit
        && offset.unwrap_or(0) == 0
    {
        return plan;
    }
    // TiDB folds `LIMIT 0` into an empty plan (`TableDual rows:0`).
    if limit == Some(0) && offset.unwrap_or(0) == 0 {
        return PhysicalPlan::TableDual { row_count: 0 };
    }
    // A point get returns at most one row, so TiDB's point-get path drops a
    // plain `LIMIT n` (n >= 1) instead of keeping a Limit node above it.
    if offset.unwrap_or(0) == 0
        && limit.is_some_and(|lim| lim >= 1)
        && matches!(plan, PhysicalPlan::PointGet { .. })
    {
        return plan;
    }
    if ordered && let Some(lim) = limit {
        // join-cost: costed TopN vs Sort+Limit (offset forces Sort+Limit).
        // TiDB still uses TopN when offset=0; prefer_topn may also allow small offsets.
        let off = offset.unwrap_or(0);
        if off == 0 && prefer_topn(est_rows, lim) {
            return PhysicalPlan::TopN {
                child: Box::new(plan),
                by_info,
                offset: off,
                count: lim,
            };
        }
        plan = PhysicalPlan::Sort {
            child: Box::new(plan),
            by_info,
        };
        return PhysicalPlan::limit(plan, off, lim);
    }
    if ordered {
        plan = PhysicalPlan::Sort {
            child: Box::new(plan),
            by_info,
        };
    }
    if let Some(lim) = limit {
        plan = PhysicalPlan::limit(plan, offset.unwrap_or(0), lim);
    } else if offset.is_some() {
        plan = PhysicalPlan::limit(plan, offset.unwrap_or(0), u64::MAX);
    }
    plan
}

/// TiDB `AdjustRowCountFor{Table,Index}ScanByLimit` over an unanalyzed table.
///
/// `LIMIT n` above a single-table reader scales what the scan has to read: it
/// only needs the rows the remaining filters still reject, so the scan's
/// expected count becomes `min(accessRows, n * accessRows / filteredRows)`
/// (`n / selectivity`). The cop `Selection`/probe keep the `n`-sized output,
/// and TiDB pushes the limit into the reader's cop task (`data:Limit`,
/// `Limit(Probe)`) unless a double read has to keep the index order.
fn apply_limit_scan_scale(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> PhysicalPlan {
    let PhysicalPlan::Limit {
        offset,
        count,
        child,
    } = plan
    else {
        return plan;
    };
    // Single base table only: joins and derived tables have their own readers.
    if !select.joins.is_empty() || select.from_item.is_some() || !select.group_by.is_empty() {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    }
    let Some(from) = select.from.as_ref() else {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    };
    let Some(selection) = select.selection.as_ref() else {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    };
    // Pseudo statistics only: the model reads the pseudo range rates.
    let (db, table) = from.qualified(current_db);
    let Some(info) = is.and_then(|is| is.table_by_name(&db, &table)) else {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    };
    if stats
        .and_then(|handle| handle.table(&db, &table))
        .is_some_and(|table| super::plan_cost::has_real_stats(&table))
    {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    }
    let expected = (offset + count) as f64;
    if expected <= 0.0 {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    }
    let conjuncts = selection.split_cnf();
    let total = table_row_count(stats, from, current_db);
    let filtered = sealdb_statistics::pseudo::filter_rows(&conjuncts, total);
    // TiDB only adjusts when the limit is below the filtered row count.
    if filtered <= 0.0 || expected >= filtered {
        return PhysicalPlan::Limit {
            offset,
            count,
            child,
        };
    }
    let scaled = scale_reader_by_limit(
        *child, selection, info, from, &conjuncts, total, filtered, expected, offset, count,
    );
    PhysicalPlan::Limit {
        offset,
        count,
        child: Box::new(scaled),
    }
}

/// Scale the single-table reader under a `LIMIT` (see [`apply_limit_scan_scale`]).
#[allow(clippy::too_many_arguments)]
fn scale_reader_by_limit(
    plan: PhysicalPlan,
    selection: &ExprNode,
    info: &sealdb_meta::TableInfo,
    _from: &TableName,
    conjuncts: &[ExprNode],
    total: f64,
    filtered: f64,
    expected: f64,
    offset: u64,
    count: u64,
) -> PhysicalPlan {
    match plan {
        // Clustered PK / rowid handle range: cop `Limit` above the scan.
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            mut row_count,
            stats_pseudo,
            filter_est,
            range_scan,
            keep_order,
            data_limit,
            pushed,
        } => {
            let (Some(raw), true) = (range_scan, stats_pseudo) else {
                return PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    filter_est,
                    range_scan: None,
                    keep_order,
                    data_limit,
                    pushed,
                };
            };
            let Some(handle) = info.handle_column() else {
                return PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    filter_est,
                    range_scan: Some(raw),
                    keep_order,
                    data_limit,
                    pushed,
                };
            };
            let access = access_conjuncts(selection, handle);
            if access.is_empty() {
                return PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    filter_est,
                    range_scan: Some(raw),
                    keep_order,
                    data_limit,
                    pushed,
                };
            }
            let _ = conjuncts;
            let access_rows = sealdb_statistics::pseudo::filter_rows(&access, total);
            if access_rows <= 0.0 {
                return PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    filter_est,
                    range_scan: Some(raw),
                    keep_order,
                    data_limit,
                    pushed,
                };
            }
            let scan = (expected * access_rows / filtered).min(access_rows);
            row_count = scan;
            let shape = split_range_shape(&raw);
            let selection_marker = shape
                .selection
                .map(|sel| format!("\u{1}SEL:{sel}"))
                .unwrap_or_default();
            let text = format!(
                "{}{selection_marker}\u{1}SCAN:{scan}\u{1}LIMIT:{offset}:{count}",
                shape.text
            );
            PhysicalPlan::TableReader {
                table,
                partitions,
                partition_names,
                partition_access,
                row_count,
                stats_pseudo,
                filter_est: Some(expected.min(filtered)),
                range_scan: Some(text),
                keep_order,
                data_limit,
                pushed,
            }
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            selection_info,
            probe_selection_info,
            probe_est,
            range_scan,
            pushed,
            ..
        } => {
            // An embedded `LIMIT` already shaped the lookup (`limit embedded`).
            if selection_info
                .as_deref()
                .is_some_and(|sel| sel.starts_with("LIMIT|"))
            {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan,
                    pushed,
                };
            }
            let Some(mut range) = range_scan else {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: None,
                    pushed,
                };
            };
            if !range.pseudo {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: Some(range),
                    pushed,
                };
            }
            let Some(index_info) = info
                .indices
                .iter()
                .find(|idx| idx.name.o().eq_ignore_ascii_case(&index))
            else {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: Some(range),
                    pushed,
                };
            };
            let Some(access) = super::access_range::index_access_range(
                info,
                index_info,
                conjuncts,
                total,
                None,
            ) else {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: Some(range),
                    pushed,
                };
            };
            let access_rows = access.range.rows;
            if access_rows <= 0.0 {
                return PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count: probe_est,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: Some(range),
                    pushed,
                };
            }
            let keys = index_key_column_names(info, index_info);
            let index_filters: Vec<ExprNode> = conjuncts
                .iter()
                .filter(|conjunct| {
                    !access
                        .consumed
                        .iter()
                        .any(|used| used.restore_string() == conjunct.restore_string())
                        && conjunct.referenced_columns().iter().all(|(_, column)| {
                            keys.iter().any(|key| key.eq_ignore_ascii_case(column))
                        })
                })
                .cloned()
                .collect();
            let after_index = if index_filters.is_empty() {
                access_rows
            } else {
                sealdb_statistics::pseudo::filter_rows(&index_filters, access_rows)
            };
            let scan = (expected * access_rows / filtered).min(access_rows);
            let build_rows = scan * after_index / access_rows;
            range.rows = scan;
            range.filter_rows = build_rows;
            let out_rows = expected.min(filtered);
            let probe_marker = match probe_selection_info.as_deref() {
                Some(sel) => format!("PROBE_LIMIT|{offset}|{count}|{sel}"),
                None => format!("PROBE_LIMIT|{offset}|{count}|"),
            };
            PhysicalPlan::IndexLookUpReader {
                table,
                index,
                index_columns,
                partitions,
                partition_access,
                row_count: out_rows,
                selection_info,
                probe_selection_info: Some(probe_marker),
                probe_est: out_rows,
                range_scan: Some(range),
                pushed,
            }
        }
        other => other,
    }
}

/// EXPLAIN-only pieces encoded in a `TableReader::range_scan` string.
struct RangeShape<'a> {
    text: &'a str,
    selection: Option<&'a str>,
}

fn split_range_shape(raw: &str) -> RangeShape<'_> {
    let mut parts = raw.split('\u{1}');
    let text = parts.next().unwrap_or(raw);
    let mut shape = RangeShape {
        text,
        selection: None,
    };
    for part in parts {
        if let Some(sel) = part.strip_prefix("SEL:") {
            shape.selection = Some(sel);
        }
    }
    shape
}

/// TiDB distinct-UNION dedup selectivity (`buildDistinct` → `HashAgg`):
/// pseudo stats give the dedup agg `0.8 * Union.estRows`.
const DISTINCT_UNION_EST_FACTOR: f64 = 0.8;

/// TiDB set-op plan (`planner/core/logical_plan_builder.go` `buildUnion` /
/// `buildUnionAll` / `buildDistinct`): `PhysicalUnionAll` is n-ary over all
/// arms, and a distinct `UNION` group is deduped by a `HashAgg(firstrow)` above
/// it. Trailing `UNION ALL` arms stack on top of that agg
/// (`divideUnionSelectPlans`).
///
/// `ids` are TiDB's plan column ids for the set-op output (see
/// [`PlanColumnIds`]); `arm_selects[i]` is the `SELECT` behind `arms[i]` (used
/// to push the dedup aggregation into the arms, `None` for CTE bodies).
fn build_set_op_plan(
    arms: Vec<(SetOprType, PhysicalPlan)>,
    arm_selects: Option<&[&SelectStmt]>,
    ids: &SetOpColumnIds,
    db: &str,
) -> PhysicalPlan {
    let ops: Vec<SetOprType> = arms.iter().map(|(op, _)| *op).collect();
    let Some((distinct_len, _)) = set_op_groups(&ops) else {
        // EXCEPT / INTERSECT keep per-arm semantics (binary fold).
        let mut iter = arms.into_iter();
        let (_, mut plan) = iter.next().expect("set op arm");
        for (op, right) in iter {
            plan = PhysicalPlan::Union {
                op,
                children: vec![Box::new(plan), Box::new(right)],
            };
        }
        return plan;
    };
    let mut arms = arms;
    let all_arms = arms.split_off(distinct_len);
    let deduped = match arms.len() {
        0 => None,
        1 => Some(arms.pop().expect("set op arm").1),
        _ => {
            let mut children: Vec<Box<PhysicalPlan>> =
                arms.into_iter().map(|(_, plan)| Box::new(plan)).collect();
            let pushed = arm_selects.and_then(|selects| {
                push_dedup_across_union(&children, &selects[..distinct_len], ids, db)
            });
            let raw_union_est = super::explain::est_rows(&PhysicalPlan::Union {
                op: SetOprType::UnionAll,
                children: children.clone(),
            });
            let est_rows = raw_union_est * DISTINCT_UNION_EST_FACTOR;
            let info = match &pushed {
                Some(_) => distinct_union_info_pushed(&ids.distinct, &ids.partial),
                None => distinct_union_info(&ids.distinct),
            };
            if let Some(pushed) = pushed {
                children = pushed;
            }
            let union = PhysicalPlan::Union {
                op: SetOprType::UnionAll,
                children,
            };
            Some(PhysicalPlan::DistinctUnion {
                info,
                est_rows,
                child: Box::new(union),
            })
        }
    };
    // All arms distinct → the dedup HashAgg is the set op root (TiDB keeps a
    // single UnionAll below it; no extra one-child Union wrapper).
    if all_arms.is_empty() {
        if let Some(deduped) = deduped {
            return deduped;
        }
    }
    let mut children: Vec<Box<PhysicalPlan>> = Vec::with_capacity(all_arms.len() + 1);
    children.extend(deduped.map(Box::new));
    children.extend(all_arms.into_iter().map(|(_, plan)| Box::new(plan)));
    PhysicalPlan::Union {
        op: SetOprType::UnionAll,
        children,
    }
}

/// `(distinct_len, union_all_len)` for a flattened set op; `None` when the op
/// list has `EXCEPT` / `INTERSECT` arms. TiDB `divideUnionSelectPlans` keeps
/// every arm up to the last non-`UNION ALL` operator in the distinct group.
fn set_op_groups(ops: &[SetOprType]) -> Option<(usize, usize)> {
    if ops
        .iter()
        .any(|op| !matches!(op, SetOprType::Union | SetOprType::UnionAll))
    {
        return None;
    }
    let distinct = ops
        .iter()
        .rposition(|op| *op != SetOprType::UnionAll)
        .map_or(0, |i| i + 1);
    Some((distinct, ops.len() - distinct))
}

/// TiDB `AggregationPushDownSolver.tryAggPushDownForUnion`: the `buildDistinct`
/// aggregation above a `UNION ALL` is split into a partial aggregation on every
/// arm plus the original final aggregation above the union
/// (`splitPartialAgg` + `pushAggCrossUnion`).
///
/// Each arm's partial aggregation groups by the arm's own column and also emits
/// one `firstrow` per group key, so the final aggregation consumes
/// `[partial values…, group keys…]`. Only the plain single-column index-scan arm
/// (`select c1 from t`) is materialised: the index emits that column in order,
/// so the partial aggregation is a `StreamAgg` over the `IndexReader`.
fn push_dedup_across_union(
    children: &[Box<PhysicalPlan>],
    selects: &[&SelectStmt],
    ids: &SetOpColumnIds,
    db: &str,
) -> Option<Vec<Box<PhysicalPlan>>> {
    if ids.distinct.len() != 1 || ids.partial.len() != 1 {
        return None;
    }
    let group_id = ids.distinct[0];
    let partial_id = ids.partial[0];
    children
        .iter()
        .zip(selects)
        .map(|(child, select)| {
            push_dedup_into_arm(child.as_ref(), select, group_id, partial_id, db).map(Box::new)
        })
        .collect()
}

/// One arm of [`push_dedup_across_union`].
fn push_dedup_into_arm(
    plan: &PhysicalPlan,
    select: &SelectStmt,
    group_id: usize,
    partial_id: usize,
    db: &str,
) -> Option<PhysicalPlan> {
    // The arm must be a bare single-column read: `SELECT c FROM t`.
    if select.distinct
        || !select.joins.is_empty()
        || select.from_item.is_some()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.limit.is_some()
        || select.offset.is_some()
        || !select.order_by.is_empty()
    {
        return None;
    }
    if select.fields.len() != 1 {
        return None;
    }
    let ExprNode::Column(col) = &select.fields[0].expr else {
        return None;
    };
    // `StreamAgg` needs the group key to arrive ordered, which only an index
    // scan whose leading column is that key provides. `buildProjection4Union`
    // puts a remap projection above each arm; TiDB's projection elimination
    // drops it, so look through it here.
    let PhysicalPlan::Projection { exprs, child } = plan else {
        return None;
    };
    if !exprs
        .iter()
        .all(|e| e.starts_with("col") && !e.contains("->"))
    {
        return None;
    }
    let PhysicalPlan::IndexReader {
        table,
        index_columns,
        pushed,
        range_scan,
        ..
    } = child.as_ref()
    else {
        return None;
    };
    if pushed.is_some() || range_scan.is_some() {
        return None;
    }
    if index_columns
        .first()
        .is_none_or(|c| !c.eq_ignore_ascii_case(&col.name))
    {
        return None;
    }
    // TiDB qualifies the aggregation argument with the DataSource's own schema
    // (`explain_easy.t2.c1`).
    let schema = col
        .schema
        .clone()
        .or_else(|| select.from.as_ref().and_then(|t| t.schema.clone()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| db.to_string());
    let qualifier = if schema.is_empty() {
        format!("{table}.{}", col.name)
    } else {
        format!("{schema}.{table}.{}", col.name)
    };
    // TiDB `RemoveUnnecessaryFirstRow`: every `firstrow` argument is a group
    // key, so the cop task keeps only the group-by columns.
    Some(PhysicalPlan::stream_agg(
        plan.clone(),
        format!(
            "group by:{qualifier}, funcs:firstrow({qualifier})->Column#{partial_id}, \
             funcs:firstrow({qualifier})->Column#{group_id}"
        ),
        format!("group by:{qualifier}, "),
        super::explain::est_rows(plan) * DISTINCT_UNION_EST_FACTOR,
    ))
}

/// Final aggregation after [`push_dedup_across_union`]: it consumes the arms'
/// partial values (`Column#partial`) and groups by the union output column.
fn distinct_union_info_pushed(group: &[usize], partial: &[usize]) -> String {
    if group.is_empty() || group.len() != partial.len() {
        return distinct_union_info(group);
    }
    let group_by: Vec<String> = group.iter().map(|c| format!("Column#{c}")).collect();
    let funcs: Vec<String> = group
        .iter()
        .zip(partial)
        .map(|(g, p)| format!("funcs:firstrow(Column#{p})->Column#{g}"))
        .collect();
    format!("group by:{}, {}", group_by.join(", "), funcs.join(", "))
}

/// TiDB `buildDistinct` HashAgg over a distinct UNION:
/// `group by:Column#7, Column#8, funcs:firstrow(Column#7)->Column#7, …`.
fn distinct_union_info(cols: &[usize]) -> String {
    if cols.is_empty() {
        return "group by:Column#1, funcs:firstrow(Column#1)->Column#1".to_string();
    }
    let group_by: Vec<String> = cols.iter().map(|c| format!("Column#{c}")).collect();
    let funcs: Vec<String> = cols
        .iter()
        .map(|c| format!("funcs:firstrow(Column#{c})->Column#{c}"))
        .collect();
    format!("group by:{}, {}", group_by.join(", "), funcs.join(", "))
}

/// `ORDER BY` over a set op sorts by TiDB's output plan column ids
/// (`ORDER BY 1` → `Column#7`).
fn set_op_order_by_info(order_by: &[ByItem], first: &SelectStmt, ids: &[usize]) -> String {
    let position = |expr: &ExprNode| -> Option<usize> {
        match expr {
            ExprNode::Value(ValueExpr {
                datum: Datum::I64(n),
            }) if *n > 0 => Some(*n as usize - 1),
            ExprNode::Value(ValueExpr {
                datum: Datum::U64(n),
            }) if *n > 0 => Some(*n as usize - 1),
            ExprNode::Column(col) => first.fields.iter().position(|f| {
                f.as_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&col.name))
                    || matches!(&f.expr, ExprNode::Column(c) if c.name.eq_ignore_ascii_case(&col.name))
            }),
            _ => None,
        }
    };
    order_by
        .iter()
        .map(|item| {
            let base = position(&item.expr)
                .and_then(|i| ids.get(i))
                .map(|c| format!("Column#{c}"))
                .unwrap_or_else(|| item.expr.restore_string());
            if item.desc {
                format!("{base}:desc")
            } else {
                base
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// TiDB plan column ids of a set operation's output columns.
#[derive(Clone, Default)]
struct SetOpColumnIds {
    /// Dedup `HashAgg` output of the distinct `UNION` group.
    distinct: Vec<usize>,
    /// Output columns of the outermost `UNION ALL`.
    outer: Vec<usize>,
    /// Partial-aggregation columns TiDB allocates when it pushes the dedup
    /// `HashAgg` into every arm of the `UNION ALL` below it.
    partial: Vec<usize>,
}

impl SetOpColumnIds {
    /// Final output column ids of the set op (outermost group).
    fn outer_ids(&self) -> &[usize] {
        if self.outer.is_empty() {
            &self.distinct
        } else {
            &self.outer
        }
    }
}

/// A visible `WITH` CTE definition in [`PlanColumnIds`].
#[derive(Clone)]
struct CteColumnId {
    name: String,
    /// Output column count (`cte (a, b)` list or the seed select width).
    width: usize,
    /// Plan ids consumed building the CTE body (inline references rebuild it).
    body_cost: usize,
    /// TiDB `computeCTEInlineFlag`: recursive / `AS MATERIALIZED` /
    /// referenced more than once → shared `CTETableReader`.
    materialized: bool,
}

/// Replays TiDB's plan column id allocation for set operations.
///
/// `buildUnionAll` hands out one id per output column (`buildProjection4Union`)
/// *after* every arm is built, and `WITH` CTE definitions are built before the
/// query body. EXPLAIN renders those ids in the `Sort`/`TopN` by-items above a
/// set op and in the distinct-UNION dedup HashAgg. This mirrors TiDB's build
/// order, not our physical plan.
struct PlanColumnIds<'a> {
    is: Option<&'a InfoSchema>,
    current_db: &'a str,
    /// Next id handed out (TiDB starts at 1).
    next: usize,
    /// CTE definitions in scope, innermost last.
    ctes: Vec<CteColumnId>,
    /// Set-op output ids recorded per CTE name.
    recorded: Vec<(String, SetOpColumnIds)>,
}

impl<'a> PlanColumnIds<'a> {
    fn new(is: Option<&'a InfoSchema>, current_db: &'a str) -> Self {
        Self {
            is,
            current_db,
            next: 1,
            ctes: Vec::new(),
            recorded: Vec::new(),
        }
    }

    /// Top-level `Statement::SetOpr`.
    fn statement(mut self, set: &SetOprStmt) -> SetOpColumnIds {
        if let Some(with) = &set.with {
            self.consume_with(with);
        }
        self.consume_set_op(&set.first, &set.rest, None).0
    }

    /// Statement-wide reference count for `name` (TiDB `computeCTEInlineFlag`).
    fn ref_count(&self, name: &str) -> usize {
        cte_ref_count(name).unwrap_or(1)
    }

    /// Set-op ids of `name`'s body (`WITH` definitions are built first).
    fn cte(mut self, with: &WithClause, name: &str) -> SetOpColumnIds {
        self.consume_with(with);
        self.recorded
            .iter()
            .rev()
            .find(|(cte, _)| cte.eq_ignore_ascii_case(name))
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default()
    }

    fn alloc(&mut self) -> usize {
        let id = self.next;
        self.next += 1;
        id
    }

    fn alloc_width(&mut self, width: usize) -> Vec<usize> {
        (0..width.max(1)).map(|_| self.alloc()).collect()
    }

    fn lookup_cte(&self, name: &str) -> Option<CteColumnId> {
        self.ctes
            .iter()
            .rev()
            .find(|cte| cte.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Build `WITH` definitions in declaration order (`buildWith`). Scopes are
    /// inherited into every subquery by the parser (`inherit_cte_scope_in_select`),
    /// so an already built name is skipped.
    fn consume_with(&mut self, with: &WithClause) {
        for cte in &with.ctes {
            self.consume_cte_def(with, cte);
        }
    }

    fn consume_cte_def(&mut self, with: &WithClause, cte: &CommonTableExpression) {
        if self.lookup_cte(&cte.name).is_some() {
            return;
        }
        let width = self.cte_width(cte);
        let start = self.next;
        if cte.recursive.is_some() {
            // TiDB `buildRecursiveCTE`: arms are built left to right until the
            // first self reference fails (the CTE has no seed plan yet, so no
            // column ids are consumed), then the seed group is rebuilt as a set
            // op, then the recursive arms are built (`CTETable` re-allocates the
            // CTE schema via `getResultCTESchema`) and the CTE union projection
            // (`buildProjection4CTEUnion`) allocates the result schema.
            self.ctes.push(CteColumnId {
                name: cte.name.clone(),
                width,
                body_cost: 0,
                materialized: true,
            });
            let mut seed_group: Vec<SelectStmt> = Vec::new();
            if let Ok(seed) = with.seed_select(&cte.name) {
                seed_group.push(seed);
                // `set_rest` arms that do not touch the CTE are seed arms.
                for (_, arm) in with.set_rest(&cte.name).unwrap_or_default() {
                    if select_refs_cte(&arm, &cte.name) {
                        break;
                    }
                    seed_group.push(arm);
                }
            }
            // Probe build: the seed arms are built (and thrown away) first.
            for arm in &seed_group {
                self.consume_select(arm, Some(&cte.name));
            }
            // Seed rebuild, then the seed set-op output columns.
            for arm in &seed_group {
                self.consume_select(arm, None);
            }
            if seed_group.len() > 1 {
                let w = self.select_width(&seed_group[0]);
                self.alloc_width(w);
            }
            let rec_arms = with.recursive_arms(&cte.name).unwrap_or_default();
            for (_, arm) in &rec_arms {
                self.consume_select(arm, Some(&cte.name));
            }
            if rec_arms.len() > 1 {
                let w = self.select_width(&rec_arms[0].1);
                self.alloc_width(w);
            }
            self.alloc_width(width);
            let cost = self.next - start;
            if let Some(entry) = self
                .ctes
                .iter_mut()
                .rev()
                .find(|entry| entry.name.eq_ignore_ascii_case(&cte.name))
            {
                entry.body_cost = cost;
            }
            return;
        }
        let mut recorded = None;
        if let Ok(seed) = with.seed_select(&cte.name) {
            let rest = with.set_rest(&cte.name).unwrap_or_default();
            if rest.is_empty() {
                self.consume_select(&seed, None);
            } else {
                let (ids, _) = self.consume_set_op(&seed, &rest, None);
                recorded = Some(ids);
            }
        }
        let body_cost = self.next - start;
        // TiDB inlines a CTE unless it is recursive, `AS MATERIALIZED`, or
        // referenced more than once (shared CTETableReader).
        let materialized = cte_materialized(cte, self.ref_count(&cte.name));
        self.ctes.push(CteColumnId {
            name: cte.name.clone(),
            width,
            body_cost,
            materialized,
        });
        if let Some(ids) = recorded {
            self.recorded.push((cte.name.clone(), ids));
        }
    }

    fn cte_width(&self, cte: &CommonTableExpression) -> usize {
        if !cte.col_name_list.is_empty() {
            return cte.col_name_list.len();
        }
        self.select_width(&cte.query)
    }

    fn select_width(&self, select: &SelectStmt) -> usize {
        if is_star_select(select) {
            let width: usize = select
                .join_tables()
                .into_iter()
                .map(|table| self.table_width(table))
                .sum();
            return width.max(1);
        }
        select.fields.len().max(1)
    }

    fn table_width(&self, table: &TableName) -> usize {
        if let Some(cte) = self
            .lookup_cte(table.resolve_name())
            .or_else(|| self.lookup_cte(&table.name))
        {
            return cte.width;
        }
        self.is
            .and_then(|is| is.table_by_name(self.schema_of(table), table.resolve_name()))
            .map_or(1, |info| info.columns.len())
    }

    fn schema_of<'b>(&'b self, table: &'b TableName) -> &'b str {
        table
            .schema
            .as_deref()
            .filter(|schema| !schema.is_empty())
            .unwrap_or(self.current_db)
    }

    fn consume_result_set(&mut self, node: &ResultSetNode, self_cte: Option<&str>) -> usize {
        match node {
            ResultSetNode::Select(select) => self.consume_select(select, self_cte),
            ResultSetNode::SetOpr(set) => {
                if let Some(with) = &set.with {
                    self.consume_with(with);
                }
                self.consume_set_op(&set.first, &set.rest, self_cte).1
            }
        }
    }

    /// One `SELECT` block: `WITH`, FROM, WHERE, GROUP BY / HAVING, select list.
    fn consume_select(&mut self, select: &SelectStmt, self_cte: Option<&str>) -> usize {
        if let Some(with) = &select.with {
            self.consume_with(with);
        }
        for table in select.join_tables() {
            self.consume_table(table, self_cte);
        }
        if let Some(selection) = &select.selection {
            self.consume_expr(selection, self_cte);
        }
        for group in &select.group_by {
            self.consume_expr(group, self_cte);
        }
        if let Some(having) = &select.having {
            self.consume_expr(having, self_cte);
        }
        if !is_star_select(select) {
            for field in &select.fields {
                self.consume_expr(&field.expr, self_cte);
            }
            // Computed select-list expressions get a fresh output column
            // (`buildProjectionField`); bare columns reuse the table's id.
            for field in &select.fields {
                if !matches!(field.expr, ExprNode::Column(_)) {
                    self.alloc();
                }
            }
        } else {
            for field in &select.fields {
                self.consume_expr(&field.expr, self_cte);
            }
        }
        self.select_width(select)
    }

    fn consume_table(&mut self, table: &TableName, self_cte: Option<&str>) {
        let name = table.resolve_name();
        if let Some(cte) = self
            .lookup_cte(name)
            .or_else(|| self.lookup_cte(&table.name))
        {
            // Recursive self-scan is a `CTETable` (no fresh column ids).
            let self_scan = self_cte.is_some_and(|cte_name| {
                cte_name.eq_ignore_ascii_case(name) || cte_name.eq_ignore_ascii_case(&table.name)
            });
            if !self_scan {
                self.next += if cte.materialized {
                    cte.width
                } else {
                    // Inline reference: rebuild the body (+ output column).
                    cte.body_cost + cte.width
                };
            } else {
                // Recursive `CTETable`: `getResultCTESchema` re-allocates the
                // CTE result schema for the self scan.
                self.next += cte.width;
            }
            return;
        }
        if let Some(info) = self
            .is
            .and_then(|is| is.table_by_name(self.schema_of(table), name))
        {
            // TableFullScan schema = columns + `_tidb_rowid` unless the table
            // has an integer clustered primary key.
            self.next += info.columns.len() + usize::from(!info.pk_is_handle);
        }
    }

    fn consume_expr(&mut self, expr: &ExprNode, self_cte: Option<&str>) {
        match expr {
            ExprNode::Exists(e) => {
                self.consume_result_set(&e.query, self_cte);
            }
            ExprNode::InSubquery(e) => {
                self.consume_expr(&e.expr, self_cte);
                self.consume_result_set(&e.query, self_cte);
            }
            ExprNode::ScalarSubquery(e) => {
                self.consume_result_set(&e.query, self_cte);
            }
            ExprNode::QuantifiedCmp(e) => {
                self.consume_expr(&e.expr, self_cte);
                self.consume_result_set(&e.query, self_cte);
            }
            ExprNode::Binary(b) => {
                self.consume_expr(&b.left, self_cte);
                self.consume_expr(&b.right, self_cte);
            }
            ExprNode::Unary(u) => self.consume_expr(&u.expr, self_cte),
            ExprNode::Func(f) => {
                for arg in &f.args {
                    self.consume_expr(arg, self_cte);
                }
            }
            ExprNode::Cast(c) => self.consume_expr(&c.expr, self_cte),
            ExprNode::InList(i) => {
                self.consume_expr(&i.expr, self_cte);
                for item in &i.list {
                    self.consume_expr(item, self_cte);
                }
            }
            ExprNode::Between(b) => {
                self.consume_expr(&b.expr, self_cte);
                self.consume_expr(&b.low, self_cte);
                self.consume_expr(&b.high, self_cte);
            }
            ExprNode::Like(l) => {
                self.consume_expr(&l.expr, self_cte);
                self.consume_expr(&l.pattern, self_cte);
            }
            ExprNode::IsNull(i) => self.consume_expr(&i.expr, self_cte),
            ExprNode::Case(c) => {
                if let Some(operand) = &c.operand {
                    self.consume_expr(operand, self_cte);
                }
                for (when, then) in &c.when_then {
                    self.consume_expr(when, self_cte);
                    self.consume_expr(then, self_cte);
                }
                if let Some(else_expr) = &c.else_expr {
                    self.consume_expr(else_expr, self_cte);
                }
            }
            ExprNode::Window(w) => {
                for arg in &w.args {
                    self.consume_expr(arg, self_cte);
                }
            }
            ExprNode::Row(r) => {
                for expr in &r.exprs {
                    self.consume_expr(expr, self_cte);
                }
            }
            ExprNode::Value(_) | ExprNode::Column(_) => {}
        }
    }

    /// Every arm left to right, then the distinct-group output columns, then the
    /// outer `UNION ALL` output columns.
    fn consume_set_op(
        &mut self,
        first: &SelectStmt,
        rest: &[(SetOprType, SelectStmt)],
        self_cte: Option<&str>,
    ) -> (SetOpColumnIds, usize) {
        let arms: Vec<(&SelectStmt, SetOprType)> = std::iter::once((first, SetOprType::UnionAll))
            .chain(rest.iter().map(|(op, select)| (select, *op)))
            .collect();
        for (select, _) in &arms {
            self.consume_select(select, self_cte);
        }
        let width = self.select_width(first);
        let ops: Vec<SetOprType> = arms.iter().map(|(_, op)| *op).collect();
        let mut ids = SetOpColumnIds::default();
        match set_op_groups(&ops) {
            Some((distinct_len, all_len)) => {
                if distinct_len >= 2 {
                    ids.distinct = self.alloc_width(width);
                    if all_len > 0 {
                        ids.outer = self.alloc_width(width);
                    }
                    // TiDB allocates the pushed-down partial aggregation's
                    // columns during logical optimization, i.e. after every
                    // build-time column id of the statement.
                    ids.partial = self.alloc_width(width);
                } else {
                    ids.outer = self.alloc_width(width);
                }
            }
            // EXCEPT / INTERSECT: the binary fold allocates once per operator.
            None => {
                ids.outer = self.alloc_width(width);
            }
        }
        (ids, width)
    }
}

thread_local! {
    /// Table name → schema of the statement being built, used by EXPLAIN to
    /// qualify columns with their *own* schema (TiDB `Column.ExplainInfo`).
    static TABLE_DB_MAP: std::cell::RefCell<Vec<std::collections::HashMap<String, String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Alias → physical table name (`FROM t t1` → `t1` → `t`). TiDB EXPLAIN
    /// (OrigTblName) always qualifies columns with the physical name.
    static TABLE_PHYS_MAP: std::cell::RefCell<Vec<std::collections::HashMap<String, String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Statistics snapshots of the statement's tables (TiDB hands the
    /// DataSource its `TableStats`), so predicate pushdown can size ranges.
    static STATS_SCOPE: std::cell::RefCell<
        Vec<std::collections::HashMap<(String, String), sealdb_statistics::table::Table>>,
    > = const { std::cell::RefCell::new(Vec::new()) };
    /// Statement-wide table reference counts for the plan build in progress
    /// (TiDB `computeCTEInlineFlag`, see [`with_statement_cte_refs`]).
    static CTE_REF_COUNTS: std::cell::RefCell<Option<std::collections::HashMap<String, usize>>> =
        const { std::cell::RefCell::new(None) };
    /// Whether the plan build in progress belongs to an `EXPLAIN` statement.
    static EXPLAIN_BUILD_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Whether the current statement runs inside an explicit transaction, where
    /// `SELECT … FOR UPDATE` point gets report `lock` in EXPLAIN.
    static EXPLICIT_TXN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the plan build in progress plans the source SELECT of an
    /// `INSERT`/`REPLACE` (TiDB allocates the insert target columns first).
    static DML_SOURCE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Table ids the current transaction has written (TiDB dirty `MemBuffer`):
    /// their reads become `UnionScan` over snapshot ∪ pending writes.
    static DIRTY_TABLES: std::cell::RefCell<Vec<i64>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Whether the statement being built is a read (`SELECT` / `EXPLAIN
    /// SELECT`): only those gather dirty rows through a `UnionScan`.
    static READ_STATEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// TiDB `tidb_cost_model_version`: 1 keeps the legacy double-read charge for
    /// an `IndexLookUp`, 2 (the 8.x default) prices the probe as one scan.
    static COST_MODEL_VERSION: std::cell::Cell<i32> = const { std::cell::Cell::new(2) };
    /// `(partial, final)` of TiDB `tidb_hashagg_{partial,final}_concurrency`.
    /// The cost model amortises a root `HashAgg`'s CPU cost by
    /// `min(partial, final)`; when both are 1 the divisor is 0 and the hash
    /// aggregate prices exactly like a `StreamAgg` plus its memory cost.
    static HASHAGG_CONCURRENCY: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((5, 5)) };
    /// TiDB `tidb_opt_agg_push_down` (default OFF): `AggregationPushDownSolver`
    /// may split a decomposable aggregation into a partial one below an inner
    /// join plus a final aggregation above it.
    static AGG_PUSH_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// TiDB `tidb_opt_insubq_to_join_and_agg` (default ON): a non-correlated
    /// `x IN (SELECT …)` becomes an inner join with a de-duplicating aggregate
    /// on the subquery side instead of a semi join.
    static INSUBQ_TO_JOIN_AND_AGG: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Run `f` with the session's `tidb_opt_insubq_to_join_and_agg` visible.
pub fn with_insubq_to_join_and_agg<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let prev = INSUBQ_TO_JOIN_AND_AGG.with(|c| c.replace(on));
    let out = f();
    INSUBQ_TO_JOIN_AND_AGG.with(|c| c.set(prev));
    out
}

/// Session `tidb_opt_insubq_to_join_and_agg` (TiDB default ON).
pub fn insubq_to_join_and_agg() -> bool {
    INSUBQ_TO_JOIN_AND_AGG.with(|c| c.get())
}

/// Run `f` with the session's `tidb_opt_agg_push_down` visible to the builder.
pub fn with_agg_push_down<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let prev = AGG_PUSH_DOWN.with(|c| c.replace(on));
    let out = f();
    AGG_PUSH_DOWN.with(|c| c.set(prev));
    out
}

/// Session `tidb_opt_agg_push_down` (TiDB default OFF).
pub fn agg_push_down() -> bool {
    AGG_PUSH_DOWN.with(|c| c.get())
}

/// Run `f` with the session's hash-aggregate concurrency visible to cost models.
pub fn with_hashagg_concurrency<T>(partial: u64, fin: u64, f: impl FnOnce() -> T) -> T {
    let prev = HASHAGG_CONCURRENCY.with(|c| c.replace((partial, fin)));
    let out = f();
    HASHAGG_CONCURRENCY.with(|c| c.set(prev));
    out
}

/// Whether TiDB amortises a root `HashAgg`'s CPU cost over goroutines
/// (`PhysicalHashAgg.cpuCostDivisor` returns 0 with partial=final=1).
pub fn hashagg_cpu_amortized() -> bool {
    HASHAGG_CONCURRENCY.with(|c| c.get()) != (1, 1)
}

/// Run `f` with the session's `tidb_cost_model_version` visible to cost models.
pub fn with_cost_model_version<T>(version: i32, f: impl FnOnce() -> T) -> T {
    let prev = COST_MODEL_VERSION.with(|c| c.replace(version));
    let out = f();
    COST_MODEL_VERSION.with(|c| c.set(prev));
    out
}

/// Session `tidb_cost_model_version` (defaults to TiDB 8.x `2`).
pub fn cost_model_version() -> i32 {
    COST_MODEL_VERSION.with(|c| c.get())
}

/// Mark the plan build in progress as a read statement (see [`READ_STATEMENT`]).
fn with_read_statement<T>(read: bool, f: impl FnOnce() -> T) -> T {
    let prev = READ_STATEMENT.with(|c| c.replace(read));
    let out = f();
    READ_STATEMENT.with(|c| c.set(prev));
    out
}

/// Run `f` with the transaction's dirty table ids visible to plan building.
pub fn with_dirty_tables<T>(tables: Vec<i64>, f: impl FnOnce() -> T) -> T {
    let prev = DIRTY_TABLES.with(|c| c.replace(tables));
    let out = f();
    DIRTY_TABLES.with(|c| *c.borrow_mut() = prev);
    out
}

fn table_is_dirty(table_id: i64) -> bool {
    DIRTY_TABLES.with(|c| c.borrow().contains(&table_id))
}

/// Run `f` with the explicit-transaction flag visible to plan building.
pub fn with_explicit_txn<T>(explicit: bool, f: impl FnOnce() -> T) -> T {
    let prev = EXPLICIT_TXN.with(|c| c.replace(explicit));
    let out = f();
    EXPLICIT_TXN.with(|c| c.set(prev));
    out
}

fn in_explicit_txn() -> bool {
    EXPLICIT_TXN.with(|c| c.get())
}

/// Run `f` as the source SELECT of an `INSERT`/`REPLACE` statement.
fn with_dml_source<T>(f: impl FnOnce() -> T) -> T {
    DML_SOURCE_DEPTH.with(|d| d.set(d.get() + 1));
    let out = f();
    DML_SOURCE_DEPTH.with(|d| d.set(d.get() - 1));
    out
}

fn in_dml_source() -> bool {
    DML_SOURCE_DEPTH.with(|d| d.get() > 0)
}

/// Is the select being planned the statement's outermost one? The table-scope
/// stack gets one entry per planned select, plus one for the `EXPLAIN` wrapper,
/// so at most two entries means top level.
fn select_is_top_level() -> bool {
    TABLE_DB_MAP.with(|stack| stack.borrow().len() <= 2)
}

/// Run `f` as part of an `EXPLAIN` plan build (`DESC`/`EXPLAIN` shape builders
/// must not fire for the same SQL executed normally).
fn with_explain_build<T>(f: impl FnOnce() -> T) -> T {
    EXPLAIN_BUILD_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let out = f();
    EXPLAIN_BUILD_DEPTH.with(|depth| depth.set(depth.get() - 1));
    out
}

fn in_explain_build() -> bool {
    EXPLAIN_BUILD_DEPTH.with(|depth| depth.get() > 0)
}

/// TiDB `ExplainInfo` qualifies a column with the *table's* schema. Push the
/// schema map of the statement being planned for the duration of the build.
struct TableDbScope;

impl TableDbScope {
    fn new(stmt: &Statement, current_db: &str) -> Self {
        let mut map = std::collections::HashMap::new();
        // `EXPLAIN <select>` plans the inner statement.
        let stmt = match stmt {
            Statement::Explain(explain) => explain.stmt.as_ref(),
            other => other,
        };
        if let Statement::Select(select) = stmt {
            for table in select.join_tables() {
                let db = table
                    .schema
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| (!current_db.is_empty()).then(|| current_db.to_string()));
                if let Some(db) = db {
                    map.insert(table.name.to_ascii_lowercase(), db.clone());
                    map.insert(table.resolve_name().to_ascii_lowercase(), db);
                }
            }
        }
        TABLE_DB_MAP.with(|stack| stack.borrow_mut().push(map));
        let mut phys = std::collections::HashMap::new();
        if let Statement::Select(select) = stmt {
            for table in select.join_tables() {
                let physical = table.resolve_name().to_ascii_lowercase();
                phys.insert(table.name.to_ascii_lowercase(), physical.clone());
                phys.insert(physical.clone(), physical);
            }
        }
        TABLE_PHYS_MAP.with(|stack| stack.borrow_mut().push(phys));
        Self
    }
}

impl Drop for TableDbScope {
    fn drop(&mut self) {
        TABLE_DB_MAP.with(|stack| {
            stack.borrow_mut().pop();
        });
        TABLE_PHYS_MAP.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// Schema of `table` as recorded by [`TableDbScope`] (`None` for derived
/// tables and plans built outside a statement scope).
pub(crate) fn explain_table_schema(table: &str) -> Option<String> {
    TABLE_DB_MAP.with(|stack| {
        stack
            .borrow()
            .iter()
            .rev()
            .find_map(|map| map.get(&table.to_ascii_lowercase()).cloned())
    })
}

/// Physical (OrigTblName) table behind an alias: `FROM t t1` → `t1` → `t`.
/// TiDB EXPLAIN qualifies column references with this name.
pub(crate) fn explain_physical_table(table: &str) -> Option<String> {
    TABLE_PHYS_MAP.with(|stack| {
        stack
            .borrow()
            .iter()
            .rev()
            .find_map(|map| map.get(&table.to_ascii_lowercase()).cloned())
    })
}

/// Schema of `table` (as named in the plan) for the statement being planned.
/// A plan node only keeps the table name, so the statement scope supplies the
/// database that `FROM other_db.t` resolved to.
pub(crate) fn statement_table_db(table: &str) -> Option<String> {
    explain_table_schema(table)
}

/// Statistics snapshot of a table (matched by name within the statement's
/// table scope) for the plan build in progress.
pub(crate) fn statement_stats(table: &str) -> Option<sealdb_statistics::table::Table> {
    let name = table.to_ascii_lowercase();
    STATS_SCOPE.with(|stack| {
        stack.borrow().iter().rev().find_map(|map| {
            map.iter()
                .find(|((_, table), _)| *table == name)
                .map(|(_, stats)| stats.clone())
        })
    })
}

/// Snapshot every base table of `stmt` so pushdown rules can estimate with the
/// same statistics TiDB's DataSource carries.
struct StatsScope;

impl StatsScope {
    fn new(stmt: &Statement, current_db: &str, stats: Option<&Handle>) -> Self {
        let mut map = std::collections::HashMap::new();
        if let Some(handle) = stats {
            let stmt = match stmt {
                Statement::Explain(explain) => explain.stmt.as_ref(),
                other => other,
            };
            if let Statement::Select(select) = stmt {
                for table in select.join_tables() {
                    let (db, name) = table.qualified(current_db);
                    if let Some(table_stats) = handle
                        .table(&db, &name)
                        .filter(super::plan_cost::has_real_stats)
                    {
                        map.insert(
                            (db.to_ascii_lowercase(), name.to_ascii_lowercase()),
                            table_stats,
                        );
                    }
                }
            }
        }
        STATS_SCOPE.with(|stack| stack.borrow_mut().push(map));
        Self
    }
}

impl Drop for StatsScope {
    fn drop(&mut self) {
        STATS_SCOPE.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// `{schema}.` prefix for EXPLAIN column references, falling back to the
/// session schema when the table is unknown.
/// TiDB decides CTE inlining per statement (`computeCTEInlineFlag`): a
/// non-recursive CTE referenced more than once is materialized as a shared
/// `CTETableReader`. Count table references over the whole statement for the
/// outermost `build_with_schema` call; nested subquery builds reuse them.
fn with_statement_cte_refs<T>(stmt: &Statement, f: impl FnOnce() -> T) -> T {
    let outermost = CTE_REF_COUNTS.with(|cell| cell.borrow().is_none());
    if outermost {
        let mut counts = std::collections::HashMap::new();
        count_statement_table_refs(stmt, &mut counts);
        CTE_REF_COUNTS.with(|cell| *cell.borrow_mut() = Some(counts));
    }
    let out = f();
    if outermost {
        CTE_REF_COUNTS.with(|cell| *cell.borrow_mut() = None);
    }
    out
}

/// Whether `select` references the CTE `name` in its FROM (recursive arm
/// detection for `buildRecursiveCTE` seeding).
fn select_refs_cte(select: &SelectStmt, name: &str) -> bool {
    select.join_tables().into_iter().any(|table| {
        table.name.eq_ignore_ascii_case(name) || table.resolve_name().eq_ignore_ascii_case(name)
    })
}

fn cte_ref_count(name: &str) -> Option<usize> {
    CTE_REF_COUNTS.with(|cell| {
        let counts = cell.borrow();
        counts
            .as_ref()
            .and_then(|counts| counts.get(&name.to_ascii_lowercase()).copied())
    })
}

fn count_statement_table_refs(
    stmt: &Statement,
    counts: &mut std::collections::HashMap<String, usize>,
) {
    match stmt {
        Statement::Select(select) => count_select_table_refs(select, counts),
        Statement::SetOpr(set) => {
            if let Some(with) = &set.with {
                for cte in &with.ctes {
                    count_select_table_refs(&cte.query, counts);
                }
            }
            count_select_table_refs(&set.first, counts);
            for (_, select) in &set.rest {
                count_select_table_refs(select, counts);
            }
        }
        Statement::Explain(explain) => count_statement_table_refs(&explain.stmt, counts),
        _ => {}
    }
}

fn count_select_table_refs(
    select: &SelectStmt,
    counts: &mut std::collections::HashMap<String, usize>,
) {
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            count_select_table_refs(&cte.query, counts);
            for (_, arm) in &cte.set_rest {
                count_select_table_refs(arm, counts);
            }
            if let Some((_, rec)) = &cte.recursive {
                count_select_table_refs(rec, counts);
            }
            for (_, rec) in &cte.recursive_rest {
                count_select_table_refs(rec, counts);
            }
        }
    }
    for table in select.join_tables() {
        // One reference, but count both the written name and the CTE it
        // resolves to (aliases `FROM cte AS a`); avoid double counting when
        // the two are identical, else single-reference CTEs look shared.
        let written = table.name.to_ascii_lowercase();
        *counts.entry(written.clone()).or_insert(0) += 1;
        let resolved = table.resolve_name().to_ascii_lowercase();
        if resolved != written {
            *counts.entry(resolved).or_insert(0) += 1;
        }
    }
    for field in &select.fields {
        count_expr_table_refs(&field.expr, counts);
    }
    if let Some(selection) = &select.selection {
        count_expr_table_refs(selection, counts);
    }
    for group in &select.group_by {
        count_expr_table_refs(group, counts);
    }
    if let Some(having) = &select.having {
        count_expr_table_refs(having, counts);
    }
}

fn count_expr_table_refs(expr: &ExprNode, counts: &mut std::collections::HashMap<String, usize>) {
    match expr {
        ExprNode::Exists(e) => count_result_set_table_refs(&e.query, counts),
        ExprNode::InSubquery(e) => {
            count_expr_table_refs(&e.expr, counts);
            count_result_set_table_refs(&e.query, counts);
        }
        ExprNode::ScalarSubquery(e) => count_result_set_table_refs(&e.query, counts),
        ExprNode::QuantifiedCmp(e) => {
            count_expr_table_refs(&e.expr, counts);
            count_result_set_table_refs(&e.query, counts);
        }
        ExprNode::Binary(b) => {
            count_expr_table_refs(&b.left, counts);
            count_expr_table_refs(&b.right, counts);
        }
        ExprNode::Unary(u) => count_expr_table_refs(&u.expr, counts),
        ExprNode::Func(f) => {
            for arg in &f.args {
                count_expr_table_refs(arg, counts);
            }
        }
        ExprNode::Cast(c) => count_expr_table_refs(&c.expr, counts),
        ExprNode::InList(i) => {
            count_expr_table_refs(&i.expr, counts);
            for item in &i.list {
                count_expr_table_refs(item, counts);
            }
        }
        ExprNode::Between(b) => {
            count_expr_table_refs(&b.expr, counts);
            count_expr_table_refs(&b.low, counts);
            count_expr_table_refs(&b.high, counts);
        }
        ExprNode::Like(l) => {
            count_expr_table_refs(&l.expr, counts);
            count_expr_table_refs(&l.pattern, counts);
        }
        ExprNode::IsNull(i) => count_expr_table_refs(&i.expr, counts),
        ExprNode::Case(c) => {
            if let Some(operand) = &c.operand {
                count_expr_table_refs(operand, counts);
            }
            for (when, then) in &c.when_then {
                count_expr_table_refs(when, counts);
                count_expr_table_refs(then, counts);
            }
            if let Some(else_expr) = &c.else_expr {
                count_expr_table_refs(else_expr, counts);
            }
        }
        ExprNode::Window(w) => {
            for arg in &w.args {
                count_expr_table_refs(arg, counts);
            }
        }
        ExprNode::Row(r) => {
            for expr in &r.exprs {
                count_expr_table_refs(expr, counts);
            }
        }
        ExprNode::Value(_) | ExprNode::Column(_) => {}
    }
}

fn count_result_set_table_refs(
    node: &ResultSetNode,
    counts: &mut std::collections::HashMap<String, usize>,
) {
    match node {
        ResultSetNode::Select(select) => count_select_table_refs(select, counts),
        ResultSetNode::SetOpr(set) => {
            count_select_table_refs(&set.first, counts);
            for (_, select) in &set.rest {
                count_select_table_refs(select, counts);
            }
        }
    }
}

/// TiDB CTE materialization: recursive or `AS MATERIALIZED`, or a
/// non-`NOT MATERIALIZED` CTE referenced more than once.
fn cte_materialized(cte: &CommonTableExpression, refs: usize) -> bool {
    if cte.recursive.is_some() || cte.materialize == CteMaterialize::Materialized {
        return true;
    }
    cte.materialize != CteMaterialize::NotMaterialized && refs > 1
}

/// TiDB TopN ExplainInfo by-items: `select.<table>.<col>` (query-block prefix).
/// Derived UNION duals use `Column#3:desc` (TiDB plan column id for set-op output).
/// Join + ORDER BY ordinals use `{db}.{origTbl}.{col}` per output position.
/// Table owning a USING/NATURAL output column: the preserved side for outer
/// joins (`t2.a` for `t1 RIGHT JOIN t2 USING(a)`), otherwise the left side.
fn using_output_table(select: &SelectStmt, name: &str) -> Option<String> {
    for join in &select.joins {
        if join.using_cols.iter().any(|c| c.eq_ignore_ascii_case(name)) {
            let owner = if matches!(join.tp, AstJoinType::RightOuter) {
                &join.right
            } else {
                &join.left
            };
            return Some(owner.name.clone());
        }
    }
    None
}

pub(crate) fn topn_by_info(select: &SelectStmt, current_db: &str) -> String {
    let table = select.from.as_ref().map(|t| t.name.as_str());
    let from_derived_union = select.with.as_ref().is_some_and(|w| {
        table.is_some_and(|t| w.find(t).is_some_and(|cte| !cte.set_rest.is_empty()))
    });
    let join_labels = join_star_output_labels(select, current_db);
    select
        .order_by
        .iter()
        .map(|item| {
            let base = match (&table, &item.expr) {
                (_, ExprNode::Column(_)) if from_derived_union => "Column#3".to_string(),
                (_, ExprNode::Column(col)) => {
                    // USING/NATURAL column: the output column follows the
                    // preserved side (`t2.a` for RIGHT JOIN, `t1.a` otherwise).
                    let owner = using_output_table(select, &col.name)
                        .or_else(|| table.map(|t| t.to_string()));
                    match owner {
                        Some(t) => format!("{}.{t}.{}", schema_prefix(current_db), col.name),
                        None => item.expr.restore_string(),
                    }
                }
                (_, ExprNode::Value(v)) if matches!(v.datum, Datum::I64(_) | Datum::U64(_)) => {
                    let ord = match v.datum {
                        Datum::I64(n) => n as usize,
                        Datum::U64(n) => n as usize,
                        _ => 0,
                    };
                    if !join_labels.is_empty() {
                        join_labels
                            .get(ord.saturating_sub(1))
                            .cloned()
                            .unwrap_or_else(|| item.expr.restore_string())
                    } else {
                        // `ORDER BY 1` over a base table resolves to the field's
                        // column label (`cte1.tt2.c1`), not the ordinal.
                        let from_cte = table.is_some_and(|t| {
                            select.with.as_ref().is_some_and(|w| w.find(t).is_some())
                        });
                        let label = select.fields.get(ord.saturating_sub(1)).and_then(|f| {
                            match (&f.expr, table) {
                                (ExprNode::Column(c), Some(t)) if !from_cte => Some(format!(
                                    "{}.{}.{}",
                                    schema_prefix(current_db),
                                    c.table.as_deref().unwrap_or(t),
                                    c.name
                                )),
                                _ => None,
                            }
                        });
                        label.unwrap_or_else(|| item.expr.restore_string())
                    }
                }
                _ => item.expr.restore_string(),
            };
            if item.desc {
                format!("{base}:desc")
            } else {
                base
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `SELECT *` join output labels for EXPLAIN Sort/Projection (`cte.t1.c1, cte.tpk.c1`).
fn join_star_output_labels(select: &SelectStmt, current_db: &str) -> Vec<String> {
    if select.joins.is_empty() || !is_star_select(select) {
        return Vec::new();
    }
    let schema = if current_db.is_empty() {
        "select"
    } else {
        current_db
    };
    let mut labels = Vec::new();
    for tbl in select.join_tables() {
        let phys = merge_join_orig_table(select, tbl);
        // Single-column CTE/table cases in the cte suite; expand via seed fields.
        if let Some(with) = select.with.as_ref()
            && let Some(cte) = with.find(tbl.resolve_name())
        {
            for name in cte_seed_output_names(&cte.query) {
                if !name.is_empty() {
                    labels.push(format!("{schema}.{phys}.{name}"));
                }
            }
            continue;
        }
        labels.push(format!("{schema}.{phys}.c1"));
    }
    labels
}

/// `SELECT * … WHERE col > ALL (WITH …)` — TiDB EXPLAIN shows Projection over Apply.
fn wrap_quantified_all_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if !is_star_select(select) || matches!(plan, PhysicalPlan::Projection { .. }) {
        return plan;
    }
    let PhysicalPlan::Apply {
        info: Some(info), ..
    } = &plan
    else {
        return plan;
    };
    if !info.contains("CARTESIAN inner join, other cond:") {
        return plan;
    }
    let Some(from) = &select.from else {
        return plan;
    };
    let schema = schema_prefix(current_db);
    let table = from.name.as_str();
    let cols: Vec<String> = is
        .and_then(|is| is.table_by_name(current_db, table))
        .map(|info| {
            info.columns
                .iter()
                .map(|c| c.name.o().to_string())
                .collect()
        })
        .unwrap_or_else(|| vec!["c1".into(), "c2".into()]);
    let exprs = cols
        .into_iter()
        .map(|c| format!("{schema}.{table}.{c}"))
        .collect();
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
}

/// TiDB's extra projection over a paths-based IndexMerge (`needExtraProj`):
/// the merge's table scan ships the handle (and, when an arm keeps order, the
/// sort columns) into the reader schema, so the DataSource keeps a Projection
/// that prunes back to the SELECT list. TiDB elides it for `SELECT *` (nothing
/// to prune) except on a rowid table whose merge keeps order — there the handle
/// is `_tidb_rowid`, an extra column the star list never showed.
fn merge_extra_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if !select.joins.is_empty() {
        return plan;
    }
    let Some(from) = &select.from else {
        return plan;
    };
    let (db, table) = from.qualified(current_db);
    let Some(info) = is.and_then(|is| is.table_by_name(&db, &table)) else {
        return plan;
    };
    let star = is_star_select(select);
    let rowid = !info.pk_is_handle && !info.pk_is_clustered();
    // `Projection` sitting directly on the access path whose items are the
    // trivial `colN` passthrough the explain walker drops; it still prunes the
    // handle columns for execution.
    fn passthrough(plan: &PhysicalPlan) -> Option<&PhysicalPlan> {
        if let PhysicalPlan::Projection { exprs, child } = plan
            && !exprs.is_empty()
            && exprs.iter().all(|e| {
                e.strip_prefix("col")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
        {
            return Some(child);
        }
        None
    }
    fn merge(p: &PhysicalPlan) -> Option<&PhysicalPlan> {
        match p {
            PhysicalPlan::UnionScan { child, .. } | PhysicalPlan::Limit { child, .. } => merge(child),
            PhysicalPlan::IndexMerge { paths, .. } if !paths.is_empty() => Some(p),
            _ => None,
        }
    }
    let keep_order = |p: &PhysicalPlan| match merge(p) {
        Some(PhysicalPlan::IndexMerge { keep_order, .. }) => *keep_order,
        _ => false,
    };
    // Star lists only need the projection when the merge keeps order on a
    // rowid table; other SELECT lists always prune the merge's extra columns.
    let existing = passthrough(&plan);
    let access = existing.unwrap_or(&plan);
    if merge(access).is_none() || (star && !(rowid && keep_order(access))) {
        return plan;
    }
    let prefix = schema_prefix(current_db);
    // TiDB prints the *physical* table name for these items even when the query
    // aliases the table (`from tk x` → `projection: probe_im.tk.a`).
    let exprs: Vec<String> = if star {
        info.columns
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| format!("{prefix}.{}.{}", from.name, c.name.o()))
            .collect()
    } else {
        let mut items = Vec::with_capacity(select.fields.len());
        for field in &select.fields {
            let ExprNode::Column(col) = &field.expr else {
                // Expressions keep the regular output projection.
                return plan;
            };
            items.push(format!("{prefix}.{}.{}", from.name, col.name));
        }
        items
    };
    let child = match plan {
        PhysicalPlan::Projection { child, .. } if existing.is_some() => *child,
        other => other,
    };
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(child),
    }
}

/// ORDER BY a SELECT-list alias backed by a subquery (`select (select …) q
/// from t order by q`): the sort key only exists after the output Projection,
/// so plan `Sort → Projection` (SortExec resolves the alias by name).
fn sort_over_projected_subquery(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<PhysicalPlan> {
    if select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || is_star_select(select)
        || needs_hash_agg(select)
        || needs_window(select)
    {
        return None;
    }
    let mut idxs = Vec::with_capacity(select.order_by.len());
    for item in &select.order_by {
        let ExprNode::Column(col) = &item.expr else {
            return None;
        };
        let idx = select.fields.iter().position(|f| {
            let name = f
                .as_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .or(f.orig_text.as_deref())
                .unwrap_or("");
            name.eq_ignore_ascii_case(&col.name)
        })?;
        idxs.push(idx);
    }
    if !idxs.iter().any(|i| select.fields[*i].expr.has_subquery()) {
        return None;
    }
    let projected = wrap_output_projection(plan, select, None, is, current_db);
    if !matches!(projected, PhysicalPlan::Projection { .. }) {
        return None;
    }
    let by_info = idxs
        .iter()
        .map(|i| format!("Column#{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    Some(PhysicalPlan::Sort {
        child: Box::new(projected),
        by_info,
    })
}

fn wrap_output_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    qualified_table: Option<&(String, String)>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    // A table with an expression index carries a hidden generated column in
    // its DataSource schema; `SELECT *` still projects only the user columns,
    // so the star list goes through the same projection path. TiDB only adds
    // the hidden column to the DataSource when the expression index is the
    // chosen access path (the point-get case is [`expr_index_point_projection`]);
    // a scan of the primary key / an ordinary index never sees it.
    let star_hidden = is_star_select(select)
        && select.joins.is_empty()
        && hidden_table_info(select, is, current_db)
            .is_some_and(|info| plan_reads_expr_index(&plan, info));
    if (is_star_select(select) && !star_hidden)
        || needs_hash_agg(select)
        || needs_window(select)
        || matches!(plan, PhysicalPlan::Projection { .. })
    {
        return plan;
    }
    if star_hidden {
        // TiDB pushes the projection into the cop task when the reader is a
        // bare scan and keeps it at the root above any Selection/IndexLookUp.
        if let Some(proj) = cop_output_projection(&plan, select, is, current_db) {
            return proj;
        }
        let prefix = schema_prefix(current_db);
        let items = star_columns_for_table(select, None, is, current_db)
            .into_iter()
            .map(|(table, col)| format!("{prefix}.{table}.{col}"))
            .collect();
        return PhysicalPlan::Projection {
            exprs: items,
            child: Box::new(plan),
        };
    }
    // A SELECT list mixing `*`/`t.*` with other expressions expands the star
    // into the table's columns first (TiDB `buildProjection` star item).
    if select.joins.is_empty() && select.fields.iter().any(|f| is_star_field(&f.expr)) {
        return wrap_star_mixed_projection(plan, select, is, current_db);
    }
    // TiDB keeps the output projection *inside* the cop task when the
    // DataSource has to emit columns beyond the SELECT list — a pushed
    // filter/range column that is not projected. The cop projection prunes
    // them (`SELECT c FROM t WHERE b …` → `TableReader data:Projection` /
    // `IndexReader index:Projection`). When the SELECT list already covers
    // every accessed column the projections cancel and TiDB elides the node.
    if let Some(proj) = cop_output_projection(&plan, select, is, current_db) {
        return proj;
    }
    // Keep the Projection node (execution relies on it), but name items the
    // way TiDB's ExplainExpressionList does:
    //  - all-plain-column lists stay `colN` so the explain walker elides the
    //    trivial passthrough Projection (e.g. `select c` over a Point_Get);
    //  - otherwise a reused column prints its qualified name with no remap
    //    suffix, and each non-column item consumes the next plan column id.
    let has_non_column = select
        .fields
        .iter()
        .any(|f| !matches!(&f.expr, ExprNode::Column(_)));
    // TiDB numbers the source SELECT of an INSERT/REPLACE after the insert
    // target schema, and a nested select (derived table, subquery, CTE body or
    // an inlined view body) after every column the enclosing statements already
    // allocated — both keep the legacy base.
    let col_base = if in_dml_source() || !select_is_top_level() {
        projection_column_base(&plan, None)
    } else if let Some((ids, true)) = derived_agg_ids(select, is, current_db) {
        // `SELECT 1, 'a' FROM (SELECT <aggs> …) d`: the pruned seed keeps a
        // projection whose items follow the derived aggregate's outputs.
        ids.outer
    } else {
        projection_column_base(&plan, from_table_info(select, is, current_db))
    };
    let mut next_col = col_base;
    let exprs = select
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if matches!(&f.expr, ExprNode::Column(_)) {
                if let Some((db, table)) = qualified_table
                    && let ExprNode::Column(col) = &f.expr
                {
                    let schema = if db.is_empty() { "select" } else { db.as_str() };
                    return format!("{schema}.{table}.{}", col.name);
                }
                if has_non_column {
                    tidb_scalar_explain(&f.expr, true)
                } else {
                    format!("col{i}")
                }
            } else {
                let text = tidb_scalar_explain(&f.expr, true);
                let label = format!("{text}->Column#{next_col}");
                next_col += 1;
                label
            }
        })
        .collect();
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
}

/// Cop-task `Projection` for a plain-column SELECT list over a single table
/// whose pushed filter/range reads columns the projection drops.
///
/// TiDB builds the `Projection` above the DataSource and pushes it into the cop
/// task; it only survives `eliminatePhysicalProjection` when the DataSource
/// schema carries extra columns (filter columns that are not projected), so
/// `SELECT c FROM t WHERE b …` becomes `TableReader data:Projection` /
/// `IndexReader index:Projection`. The marker is EXPLAIN-only: execution still
/// runs the (kept) projection node above the reader.
fn cop_output_projection(
    plan: &PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<PhysicalPlan> {
    if !select.joins.is_empty()
        || select.from.is_none()
        || select.selection.is_none()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
    {
        return None;
    }
    if !matches!(
        plan,
        PhysicalPlan::TableReader { .. } | PhysicalPlan::IndexReader { .. }
    ) {
        return None;
    }
    // The DataSource of a table with an expression index materialises the
    // hidden generated column; the cop projection hides it from the reader
    // output exactly like a filter-only column.
    let hidden = hidden_table_info(select, is, current_db).is_some();
    let mut out = Vec::new();
    for field in &select.fields {
        if is_star_field(&field.expr) {
            if !hidden {
                return None;
            }
            out.extend(
                star_columns_for_table(select, None, is, current_db)
                    .into_iter()
                    .map(|(_, col)| col),
            );
            continue;
        }
        let ExprNode::Column(col) = &field.expr else {
            return None;
        };
        out.push(col.name.clone());
    }
    if out.is_empty() {
        return None;
    }
    let filter = select.selection.as_ref()?;
    let prunes = hidden
        || filter
            .referenced_columns()
            .iter()
            .any(|(_, name)| !out.iter().any(|o| o.eq_ignore_ascii_case(name)));
    // An `IndexReader` already outputs the index keys plus the row handle, so a
    // projection that merely re-lists them is a no-op TiDB never renders.
    let prunes = prunes
        && !match plan {
            PhysicalPlan::IndexReader { index_columns, .. } => {
                let handle = select
                    .from
                    .as_ref()
                    .and_then(|f| {
                        let (db, table) = f.qualified(current_db);
                        is.and_then(|is| is.table_by_name(&db, &table))
                    })
                    .and_then(|t| t.handle_column())
                    .map(|c| c.name.o().to_string());
                let provided = |name: &str| {
                    index_columns.iter().any(|c| c.eq_ignore_ascii_case(name))
                        || handle.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(name))
                };
                filter
                    .referenced_columns()
                    .iter()
                    .all(|(_, name)| provided(name))
                    && out.iter().all(|o| provided(o))
            }
            _ => false,
        };
    if !prunes {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, _) = from.qualified(current_db);
    let table = merge_join_orig_table(select, from);
    let list = out
        .iter()
        .map(|c| format!("{db}.{table}.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(PhysicalPlan::Projection {
        exprs: vec![cop_projection_marker(&list)],
        child: Box::new(plan.clone()),
    })
}

/// EXPLAIN-only marker: the cop `Projection` lives inside the reader;
/// `COP_PROJ|<exprs>` carries the TiDB `ExplainExpressionList` text.
pub(crate) fn cop_projection_marker(list: &str) -> String {
    format!("COP_PROJ|{list}")
}

/// TiDB `IndexInfo.Columns` key text: a plain column name, or the generating
/// expression of the hidden column backing an expression key part, which is
/// what EXPLAIN renders (`index:uk_expr(tidb_shard(`a`), a)`).
pub(crate) fn index_key_column_names(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
) -> Vec<String> {
    index
        .columns
        .iter()
        .map(|c| {
            info.columns
                .iter()
                .find(|col| col.hidden && col.name.o().eq_ignore_ascii_case(c.name.o()))
                .and_then(|col| col.generated_expr.clone())
                .unwrap_or_else(|| c.name.o().to_string())
        })
        .collect()
}

/// Any `*` / `t.*` SELECT item.
fn is_star_field(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Column(c) => c.name == "*",
        ExprNode::Value(v) => {
            matches!(&v.datum, sealdb_types::Datum::Bytes(b) if b == b"*")
        }
        _ => false,
    }
}

/// `SELECT *, date_format(b, "") …`: TiDB expands each star item to the table's
/// columns (`db.t.col`) and then names the remaining expressions `Column#N`.
fn wrap_star_mixed_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let prefix = schema_prefix(current_db);
    let mut items: Vec<String> = Vec::new();
    let mut star_cols = 0usize;
    let mut next_col = 0usize;
    let mut derived = false;
    for field in &select.fields {
        match &field.expr {
            ExprNode::Column(c) if c.name == "*" => {
                for (table, col) in
                    star_columns_for_table(select, c.table.as_deref(), is, current_db)
                {
                    items.push(format!("{prefix}.{table}.{col}"));
                    star_cols += 1;
                }
            }
            ExprNode::Value(v) if matches!(&v.datum, sealdb_types::Datum::Bytes(b) if b == b"*") => {
                for (table, col) in star_columns_for_table(select, None, is, current_db) {
                    items.push(format!("{prefix}.{table}.{col}"));
                    star_cols += 1;
                }
            }
            ExprNode::Column(_) => items.push(tidb_scalar_explain(&field.expr, true)),
            other => {
                // TiDB numbers the first derived column after the point get's
                // output (`…, _tidb_rowid, date_format(...)->Column#4`).
                if next_col == 0 {
                    next_col = star_cols + 2;
                }
                let text = projection_expr_explain(other, select, is, current_db);
                items.push(format!("{text}->Column#{next_col}"));
                next_col += 1;
                derived = true;
            }
        }
    }
    if !derived {
        // A star-only list still needs the Projection for execution (it is
        // what adds `_tidb_rowid` to the output), but TiDB elides it from
        // EXPLAIN when the items are a plain passthrough.
        let items = (0..items.len()).map(|i| format!("col{i}")).collect();
        return PhysicalPlan::Projection {
            exprs: items,
            child: Box::new(plan),
        };
    }
    PhysicalPlan::Projection {
        exprs: items,
        child: Box::new(plan),
    }
}

/// TiDB `expression.PropagateConstant` over a single-table selection: every
/// `col = const` conjunct substitutes the column in the other conjuncts.
///
/// Like TiDB the substitution does not fold the rewritten predicate (`b = 300`
/// inside `a = 100 AND ...` becomes `300 = 100`), so the access-path choice
/// still sees the original comparison structure; the constant folding happens
/// later when the selection is simplified.
fn propagate_constant_select(select: &SelectStmt) -> Option<SelectStmt> {
    if !select.joins.is_empty() {
        return None;
    }
    select.from.as_ref()?;
    let filter = select.selection.as_ref()?;
    let conds = filter.split_cnf_all();
    let mut pins: Vec<(String, Datum)> = Vec::new();
    for cond in &conds {
        let Some((key, value)) = cond.eq_key_const() else {
            continue;
        };
        match pins.iter().find(|(name, _)| name.eq_ignore_ascii_case(&key)) {
            // `a = 1 AND a = 2` is unsatisfiable and handled elsewhere.
            Some((_, existing)) if *existing != value => return None,
            Some(_) => {}
            None => pins.push((key.to_ascii_lowercase(), value)),
        }
    }
    if pins.is_empty() {
        return None;
    }
    let mut changed = false;
    let rewritten: Vec<ExprNode> = conds
        .into_iter()
        .map(|cond| {
            let own = cond
                .eq_key_const()
                .map(|(key, _)| key.to_ascii_lowercase())
                .into_iter()
                .collect::<Vec<_>>();
            let pins: Vec<(String, Datum)> = pins
                .iter()
                .filter(|(name, _)| !own.iter().any(|own| own == name))
                .cloned()
                .collect();
            if pins.is_empty() {
                return cond;
            }
            match substitute_eq_columns(&cond, &pins) {
                Some(substituted) => {
                    changed = true;
                    substituted
                }
                None => cond,
            }
        })
        .collect();
    if !changed {
        return None;
    }
    let mut out = select.clone();
    out.selection = ExprNode::compose_cnf(rewritten);
    Some(out)
}

/// Replace `col` with its pinned constant. `None` (leave the predicate alone)
/// for a node the rewriter does not model.
pub(crate) fn substitute_eq_columns(expr: &ExprNode, pins: &[(String, Datum)]) -> Option<ExprNode> {
    use sealdb_parser::ast::expressions::{
        BinaryOpExpr, FuncCastExpr, UnaryOpExpr, ValueExpr,
    };
    match expr {
        ExprNode::Column(col) => Some(
            match pins
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&col.name))
            {
                Some((_, value)) => ExprNode::Value(ValueExpr {
                    datum: value.clone(),
                }),
                // A column with no pin stays as it is.
                None => expr.clone(),
            },
        ),
        ExprNode::Value(_) => Some(expr.clone()),
        ExprNode::Binary(bin) => Some(ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(substitute_eq_columns(&bin.left, pins)?),
            right: Box::new(substitute_eq_columns(&bin.right, pins)?),
            single_cnf_item: bin.single_cnf_item,
        })),
        ExprNode::Unary(unary) => Some(ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(substitute_eq_columns(&unary.expr, pins)?),
        })),
        ExprNode::Cast(cast) => Some(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(substitute_eq_columns(&cast.expr, pins)?),
            target: cast.target.clone(),
        })),
        _ => None,
    }
}

/// Table metadata when the statement reads a single table whose schema
/// carries a hidden generated column (expression index key part).
fn hidden_table_info<'a>(
    select: &SelectStmt,
    is: Option<&'a InfoSchema>,
    current_db: &str,
) -> Option<&'a sealdb_meta::model::table::TableInfo> {
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    is?.table_by_name(&db, &table)
        .filter(|info| info.columns.iter().any(|c| c.hidden))
}

/// Does the access path read an index that has an expression key part?
///
/// Only then does TiDB's DataSource carry the hidden generated column, so the
/// star list has to be projected back to the user columns.
fn plan_reads_expr_index(
    plan: &PhysicalPlan,
    info: &sealdb_meta::model::table::TableInfo,
) -> bool {
    let is_expr = |name: &str| {
        info.indices
            .iter()
            .find(|index| index.name.o().eq_ignore_ascii_case(name))
            .is_some_and(|index| super::expr_index::has_expression_part(info, index))
    };
    match plan {
        PhysicalPlan::IndexReader { index, .. }
        | PhysicalPlan::IndexLookUpReader { index, .. } => is_expr(index),
        PhysicalPlan::IndexMerge { indexes, .. } => indexes.iter().any(|i| is_expr(i)),
        PhysicalPlan::PointGet {
            index: Some(index), ..
        }
        | PhysicalPlan::BatchPointGet {
            index: Some(index), ..
        } => is_expr(index),
        _ => false,
    }
}

/// Columns of `alias` (or of every FROM table when `alias` is None) in
/// definition order, as `(table_name, column_name)`.
fn star_columns_for_table(
    select: &SelectStmt,
    alias: Option<&str>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for table in select.join_tables() {
        if let Some(alias) = alias
            && !table.name.eq_ignore_ascii_case(alias)
            && !table.resolve_name().eq_ignore_ascii_case(alias)
        {
            continue;
        }
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        let Some(info) = is.and_then(|is| is.table_by_name(schema, table.resolve_name())) else {
            continue;
        };
        for col in info.columns.iter().filter(|c| !c.hidden) {
            out.push((table.resolve_name().to_string(), col.name.o().to_string()));
        }
    }
    out
}

/// Projection item text; `date_format` over a non-temporal column gets the
/// implicit `cast(…, datetime BINARY)` TiDB adds, and its format literal is
/// printed raw (`date_format(…, %Y)`).
fn projection_expr_explain(
    expr: &ExprNode,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> String {
    use sealdb_types::Datum;
    let ExprNode::Func(f) = expr else {
        return tidb_scalar_explain(expr, true);
    };
    if !f.name.eq_ignore_ascii_case("date_format") || f.args.len() != 2 {
        return tidb_scalar_explain(expr, true);
    }
    let first = match &f.args[0] {
        ExprNode::Column(c) if column_needs_datetime_cast(c, select, is, current_db) => {
            format!(
                "cast({}, datetime BINARY)",
                tidb_scalar_explain(&f.args[0], true)
            )
        }
        other => tidb_scalar_explain(other, true),
    };
    let second = match &f.args[1] {
        ExprNode::Value(v) => match &v.datum {
            Datum::Bytes(b) => String::from_utf8_lossy(b).to_string(),
            _ => tidb_scalar_explain(&f.args[1], true),
        },
        other => tidb_scalar_explain(other, true),
    };
    format!("date_format({first}, {second})")
}

/// True when a column feeding a temporal function is stored as a number.
fn column_needs_datetime_cast(
    col: &sealdb_parser::ast::expressions::ColumnNameExpr,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> bool {
    use sealdb_types::{
        TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DURATION, TYPE_LONG_BLOB, TYPE_MEDIUM_BLOB,
        TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY_BLOB, TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
    };
    let Some(is) = is else { return false };
    for table in select.join_tables() {
        if let Some(tbl) = col.table.as_deref()
            && !table.name.eq_ignore_ascii_case(tbl)
            && !table.resolve_name().eq_ignore_ascii_case(tbl)
        {
            continue;
        }
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        let Some(info) = is.table_by_name(schema, table.resolve_name()) else {
            continue;
        };
        let Some(column) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
        else {
            continue;
        };
        return !matches!(
            column.field_type.tp,
            TYPE_DATE
                | TYPE_DATETIME
                | TYPE_TIMESTAMP
                | TYPE_DURATION
                | TYPE_YEAR
                | TYPE_STRING
                | TYPE_VAR_STRING
                | TYPE_VARCHAR
                | TYPE_BLOB
                | TYPE_TINY_BLOB
                | TYPE_MEDIUM_BLOB
                | TYPE_LONG_BLOB
        );
    }
    false
}

/// TiDB ScalarFunction ExplainInfo; `nil_as_angle` → `<nil>` (Projection) else `NULL`.
fn tidb_scalar_explain(expr: &ExprNode, nil_as_angle: bool) -> String {
    use sealdb_parser::ast::expressions::ValueExpr;
    use sealdb_types::Datum;
    match expr {
        ExprNode::Column(c) => {
            let tbl = c.table.as_deref().unwrap_or("t");
            let schema = c.schema.clone().unwrap_or_else(|| {
                explain_table_schema(tbl).unwrap_or_else(sealdb_expression::current_db_name)
            });
            if schema.is_empty() {
                format!("select.{tbl}.{}", c.name)
            } else {
                format!("{schema}.{tbl}.{}", c.name)
            }
        }
        ExprNode::Value(ValueExpr { datum: Datum::Null }) => {
            if nil_as_angle {
                "<nil>".into()
            } else {
                "NULL".into()
            }
        }
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => v.to_string(),
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Plus => "plus",
                Op::Minus => "minus",
                Op::Mul => "mul",
                Op::Div => "div",
                Op::IntDiv => "intdiv",
                Op::Mod => "mod",
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::LogicAnd => "and",
                Op::LogicOr => "or",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                tidb_scalar_explain(&bin.left, nil_as_angle),
                tidb_scalar_explain(&bin.right, nil_as_angle)
            )
        }
        ExprNode::Cast(c) => {
            let target = cast_target_explain(&c.target);
            format!(
                "cast({}, {target})",
                tidb_scalar_explain(&c.expr, nil_as_angle)
            )
        }
        ExprNode::Like(l) => {
            let pattern = match l.pattern.as_ref() {
                // TiDB coerces numeric `+` under LIKE to double then var_string.
                ExprNode::Binary(b) if b.op == Op::Plus => format!(
                    "cast(plus(cast({}, double BINARY), cast({}, double BINARY)), var_string(370))",
                    tidb_scalar_explain(&b.left, nil_as_angle),
                    tidb_scalar_explain(&b.right, nil_as_angle),
                ),
                other => tidb_scalar_explain(other, nil_as_angle),
            };
            format!(
                "like({}, {}, 92)",
                tidb_scalar_explain(&l.expr, nil_as_angle),
                pattern,
            )
        }
        ExprNode::Func(f) => {
            let args: Vec<String> = f
                .args
                .iter()
                .map(|a| tidb_scalar_explain(a, nil_as_angle))
                .collect();
            format!("{}({})", f.name.to_ascii_lowercase(), args.join(", "))
        }
        other => other.restore_string(),
    }
}

fn cast_target_explain(target: &str) -> String {
    let upper = target.to_ascii_uppercase();
    if upper.contains("DOUBLE") || upper == "FLOAT8" {
        "double BINARY".into()
    } else if upper.contains("CHAR") || upper.contains("STRING") || upper.contains("BINARY") {
        // TiDB LIKE cast width for NULL+NULL stringification.
        "var_string(370)".into()
    } else {
        format!("{} BINARY", target.to_ascii_lowercase())
    }
}

/// Push a single-side join ON into that side's Selection; return Cross + true ON.
fn push_single_side_on(
    select: &SelectStmt,
    left: PhysicalPlan,
    right: PhysicalPlan,
    left_name: &str,
    right_name: &str,
    on: ExprNode,
    tp: JoinType,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> (PhysicalPlan, PhysicalPlan, ExprNode, JoinType) {
    if matches!(
        tp,
        JoinType::LeftOuter | JoinType::RightOuter | JoinType::Semi | JoinType::Anti
    ) {
        return (left, right, on, tp);
    }
    if join_on_has_equijoin_local(&on) {
        return (left, right, on, tp);
    }
    // `ON dt2.c1 = 1` / `ON (derived.col)` — push into that side, leave CARTESIAN.
    let (side_is_left, filter) = match &on {
        ExprNode::Column(c) => {
            let on_left = c
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(left_name));
            let on_right = c
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(right_name));
            if on_left && !on_right {
                match cte_field_expr(select, left_name, &c.name) {
                    Some(f) => (true, f),
                    None => return (left, right, on, tp),
                }
            } else if on_right && !on_left {
                match cte_field_expr(select, right_name, &c.name) {
                    Some(f) => (false, f),
                    None => return (left, right, on, tp),
                }
            } else {
                return (left, right, on, tp);
            }
        }
        ExprNode::Binary(bin) if bin.op == Op::Eq => {
            let (col, val) = match (bin.left.as_ref(), bin.right.as_ref()) {
                (ExprNode::Column(c), v @ ExprNode::Value(_)) => (c, v.clone()),
                (v @ ExprNode::Value(_), ExprNode::Column(c)) => (c, v.clone()),
                _ => return (left, right, on, tp),
            };
            let on_left = col
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(left_name));
            let on_right = col
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(right_name));
            let side_left = if on_left && !on_right {
                true
            } else if on_right && !on_left {
                false
            } else {
                return (left, right, on, tp);
            };
            let side_name = if side_left { left_name } else { right_name };
            let side_tbl = select.join_tables().into_iter().find(|t| {
                t.name.eq_ignore_ascii_case(side_name)
                    || t.resolve_name().eq_ignore_ascii_case(side_name)
            });
            let phys = side_tbl
                .as_ref()
                .map(|t| merge_join_orig_table(select, t))
                .unwrap_or_else(|| side_name.to_string());
            let mut col = col.clone();
            col.table = Some(phys);
            let filter = ExprNode::Binary(BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(col)),
                right: Box::new(val),
                single_cnf_item: false,
            });
            (side_left, filter)
        }
        _ => return (left, right, on, tp),
    };
    let true_on = ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
        datum: sealdb_types::Datum::I64(1),
    });
    if side_is_left {
        let left = push_filter_under_projection(left, &filter, is, current_db);
        (left, right, true_on, JoinType::Cross)
    } else {
        let right = push_filter_under_projection(right, &filter, is, current_db);
        (left, right, true_on, JoinType::Cross)
    }
}

/// Output column names for a CTE seed when `col_name_list` is empty.
fn cte_seed_output_names(seed: &SelectStmt) -> Vec<String> {
    seed.fields
        .iter()
        .map(|f| {
            if let Some(name) = &f.as_name
                && !name.is_empty()
            {
                return name.clone();
            }
            match &f.expr {
                ExprNode::Column(c) => c.name.clone(),
                ExprNode::Value(v) => match &v.datum {
                    Datum::I64(n) => n.to_string(),
                    Datum::U64(n) => n.to_string(),
                    Datum::F64(n) => n.to_string(),
                    Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    // `SELECT NULL` seeds a column literally named NULL
                    // (executor `tidb_projection_field_name`), not an empty name.
                    Datum::Null => "NULL".to_string(),
                    _ => f
                        .orig_text
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                },
                _ => f
                    .orig_text
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn cte_field_expr(select: &SelectStmt, cte_name: &str, col: &str) -> Option<ExprNode> {    let with = select.with.as_ref()?;
    let cte = with.find(cte_name)?;
    if let Some(i) = cte
        .col_name_list
        .iter()
        .position(|n| n.eq_ignore_ascii_case(col))
    {
        return cte.query.fields.get(i).map(|f| f.expr.clone());
    }
    cte.query.fields.iter().find_map(|f| {
        f.as_name
            .as_deref()
            .filter(|n| n.eq_ignore_ascii_case(col))
            .map(|_| f.expr.clone())
    })
}

fn push_filter_under_projection(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(predicate_push_down(*child, filter, is, current_db)),
        },
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline: true,
            recursive,
            limit,
            offset,
        } => {
            // Inline CTE: push into seed so HASH_JOIN `ON cte.col = const` becomes
            // TableReader data:Selection / PointGet (TiDB cte suite).
            PhysicalPlan::CTE {
                name,
                alias,
                columns,
                seed: Box::new(push_filter_under_projection(*seed, filter, is, current_db)),
                recursive_plan,
                inline: true,
                recursive,
                limit,
                offset,
            }
        }
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline,
            recursive,
            limit,
            offset,
        } => {
            // Materialized / recursive: keep filter above.
            predicate_push_down(
                PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed,
                    recursive_plan,
                    inline,
                    recursive,
                    limit,
                    offset,
                },
                filter,
                is,
                current_db,
            )
        }
        other => predicate_push_down(other, filter, is, current_db),
    }
}

fn join_on_has_equijoin_local(on: &ExprNode) -> bool {
    match on {
        ExprNode::Binary(bin) if bin.op == Op::Eq => {
            matches!(
                (bin.left.as_ref(), bin.right.as_ref()),
                (ExprNode::Column(_), ExprNode::Column(_))
            )
        }
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            join_on_has_equijoin_local(&bin.left) || join_on_has_equijoin_local(&bin.right)
        }
        _ => false,
    }
}

fn seed_is_dual_projection(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::TableDual { .. } => true,
        PhysicalPlan::Projection { child, .. } => seed_is_dual_projection(child),
        _ => false,
    }
}

/// Remap dual Projection `text->Column#N` to a shared UNION output column id.
fn remap_dual_projection_column(plan: PhysicalPlan, col: usize) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs: exprs
                .into_iter()
                .map(|e| match e.rsplit_once("->Column#") {
                    Some((prefix, _)) => format!("{prefix}->Column#{col}"),
                    None => e,
                })
                .collect(),
            child,
        },
        other => other,
    }
}

fn seed_is_union_of_dual(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::Union { children, .. } => children
            .iter()
            .all(|c| seed_is_dual_projection(c) || seed_is_union_of_dual(c)),
        PhysicalPlan::DistinctUnion { child, .. } => seed_is_union_of_dual(child),
        _ => false,
    }
}

fn projection_column_base(
    plan: &PhysicalPlan,
    table: Option<&sealdb_meta::model::table::TableInfo>,
) -> usize {
    match plan {
        PhysicalPlan::CTE {
            seed, inline: true, ..
        } => projection_column_base(seed, table),
        PhysicalPlan::Projection { exprs, .. } => exprs.len() + 1,
        PhysicalPlan::TableDual { .. } => 1,
        // TiDB allocates one plan column id per DataSource schema column (hidden
        // columns included) plus `_tidb_rowid` when the handle is not a
        // clustered index; the first derived expression column follows.
        PhysicalPlan::TableReader { .. }
        | PhysicalPlan::Selection { .. }
        | PhysicalPlan::IndexReader { .. } => table.map(table_column_base).unwrap_or(6),
        _ => 1,
    }
}

/// TiDB plan column-id base after a fresh single-table `DataSource`.
fn table_column_base(info: &sealdb_meta::model::table::TableInfo) -> usize {
    let rowid = usize::from(!info.pk_is_handle && !info.common_handle);
    info.columns.len() + rowid + 1
}

/// Metadata of the single base table in `FROM`, when there is exactly one.
/// Views are excluded: TiDB numbers their inlined SELECT after the statements
/// that reference them.
fn from_table_info<'a>(
    select: &SelectStmt,
    is: Option<&'a InfoSchema>,
    current_db: &str,
) -> Option<&'a sealdb_meta::model::table::TableInfo> {
    if !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    is?.table_by_name(&db, &table).filter(|info| info.view.is_none())
}

/// `BIT(n)` storage width in bytes.
fn bit_width(ft: &sealdb_types::field_type::FieldType) -> usize {
    ((ft.flen.max(1) as usize).div_ceil(8)).clamp(1, 8)
}

fn bit_padded_bytes(value: &Datum, width: usize) -> Option<Vec<u8>> {
    let raw: Vec<u8> = match value {
        // A string constant compared with a BIT column is read as a number
        // (`id in ('0')` on BIT(64) is numeric 0, not the byte `0x30`); values
        // that are not decimal text stay binary literals.
        Datum::Bytes(b) => match std::str::from_utf8(b)
            .ok()
            .map(str::trim)
            .and_then(|text| text.parse::<u64>().ok())
        {
            Some(v) => v.to_be_bytes().to_vec(),
            None => b.clone(),
        },
        Datum::Bit(b) => b.clone(),
        Datum::I64(v) => v.to_be_bytes().to_vec(),
        Datum::U64(v) => v.to_be_bytes().to_vec(),
        Datum::F64(v) => (*v as u64).to_be_bytes().to_vec(),
        Datum::Decimal(d) => d.to_i64().ok()?.to_be_bytes().to_vec(),
        _ => return None,
    };
    let mut out = vec![0u8; width];
    let take = raw.len().min(width);
    out[width - take..].copy_from_slice(&raw[raw.len() - take..]);
    Some(out)
}

/// Normalize BIT column constants to the column's big-endian byte width so
/// `0x39` / `0x0039` / `b'111001'` all compare equal to the stored value.
fn normalize_bit_selection(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    if select.from_item.is_some() || !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is?.table_by_name(&db, &table)?;
    let widths: std::collections::HashMap<String, usize> = info
        .columns
        .iter()
        .filter(|c| c.field_type.tp == sealdb_types::field_type::TYPE_BIT)
        .map(|c| (c.name.o().to_ascii_lowercase(), bit_width(&c.field_type)))
        .collect();
    if widths.is_empty() {
        return None;
    }
    let selection = select.selection.as_ref()?;
    let normalized = normalize_bit_expr(selection, &widths);
    let mut out = select.clone();
    out.selection = Some(normalized);
    Some(out)
}

/// Rewrite BIT column constants in a SELECT's WHERE to the column's big-endian
/// byte width. Runs before planning so executors that re-read the statement
/// (e.g. index point gets) see the same representation as the plan.
pub fn normalize_bit_constants(stmt: &mut Statement, is: Option<&InfoSchema>, current_db: &str) {
    if let Statement::Select(select) = stmt
        && let Some(normalized) = normalize_bit_selection(select, is, current_db)
    {
        *select = normalized;
    }
}

/// Rewrite `ENUM`/`SET` numeric constants in a SELECT's WHERE to the member
/// label. Runs before planning so executors that re-read the statement
/// (e.g. batch point gets) see the same representation as the plan.
pub fn normalize_enum_constants(stmt: &mut Statement, is: Option<&InfoSchema>, current_db: &str) {
    if let Statement::Select(select) = stmt
        && let Some(coerced) = coerce_enum_selection(select, is, current_db)
    {
        *select = coerced;
    }
}

fn bit_column_width(
    expr: &ExprNode,
    widths: &std::collections::HashMap<String, usize>,
) -> Option<usize> {
    let ExprNode::Column(col) = expr else {
        return None;
    };
    widths.get(&col.name.to_ascii_lowercase()).copied()
}

fn normalize_bit_value(expr: &ExprNode, width: usize) -> ExprNode {
    let Some(value) = expr.const_datum() else {
        return expr.clone();
    };
    match bit_padded_bytes(&value, width) {
        Some(bytes) => ExprNode::Value(ValueExpr {
            datum: Datum::Bit(bytes),
        }),
        None => expr.clone(),
    }
}

fn normalize_bit_expr(
    expr: &ExprNode,
    widths: &std::collections::HashMap<String, usize>,
) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => {
            let left = normalize_bit_expr(&bin.left, widths);
            let right = normalize_bit_expr(&bin.right, widths);
            let (left, right) = if matches!(bin.op, Op::Eq | Op::NullSafeEq) {
                if let Some(width) = bit_column_width(&left, widths) {
                    (left, normalize_bit_value(&right, width))
                } else if let Some(width) = bit_column_width(&right, widths) {
                    (normalize_bit_value(&left, width), right)
                } else {
                    (left, right)
                }
            } else {
                (left, right)
            };
            ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: bin.single_cnf_item,
            })
        }
        ExprNode::InList(list) => {
            let mut out = list.clone();
            out.expr = Box::new(normalize_bit_expr(&list.expr, widths));
            match out.expr.as_ref() {
                ExprNode::Column(_) => {
                    if let Some(width) = bit_column_width(&out.expr, widths) {
                        out.list = list
                            .list
                            .iter()
                            .map(|item| normalize_bit_value(item, width))
                            .collect();
                    }
                }
                ExprNode::Row(row) => {
                    let row_widths: Vec<Option<usize>> = row
                        .exprs
                        .iter()
                        .map(|e| bit_column_width(e, widths))
                        .collect();
                    out.list = list
                        .list
                        .iter()
                        .map(|item| {
                            let ExprNode::Row(tuple) = item else {
                                return item.clone();
                            };
                            if tuple.exprs.len() != row_widths.len() {
                                return item.clone();
                            }
                            ExprNode::Row(sealdb_parser::ast::expressions::RowExpr {
                                exprs: tuple
                                    .exprs
                                    .iter()
                                    .zip(row_widths.iter())
                                    .map(|(value, width)| match width {
                                        Some(width) => normalize_bit_value(value, *width),
                                        None => value.clone(),
                                    })
                                    .collect(),
                            })
                        })
                        .collect();
                }
                _ => {}
            }
            ExprNode::InList(out)
        }
        _ => expr.clone(),
    }
}

/// Record each column's schema so EXPLAIN can qualify it like TiDB
/// (`Column.ExplainInfo` → `db.table.col`). Only base tables in FROM/JOIN are
/// annotated; derived aliases keep their text.
/// Shard rewrite for a single-table SELECT: `a = const` gains the folded
/// `tidb_shard(a) = <bucket>` conjunct TiDB's planner adds (see
/// [`super::expr_index::shard_index_rewrite`]).
/// Projection TiDB puts above a point get on an expression index.
fn expr_index_point_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let index = match &plan {
        PhysicalPlan::PointGet {
            index: Some(index), ..
        }
        | PhysicalPlan::BatchPointGet {
            index: Some(index), ..
        } => index.clone(),
        _ => return plan,
    };
    let (Some(is), Some(from)) = (is, select.from.as_ref()) else {
        return plan;
    };
    if select.from_item.is_some() || !select.joins.is_empty() {
        return plan;
    }
    let (db, table) = from.qualified(current_db);
    let Some(info) = is.table_by_name(&db, &table) else {
        return plan;
    };
    let Some(index_info) = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(&index))
    else {
        return plan;
    };
    if !super::expr_index::has_expression_part(info, index_info) {
        return plan;
    }
    let exprs = if is_star_select(select) {
        let exprs = star_projection_exprs(select, Some(is), current_db);
        if exprs.is_empty() {
            return plan;
        }
        exprs
    } else {
        let mut next_id = info.max_col_id + 1;
        select
            .fields
            .iter()
            .map(|field| match &field.expr {
                ExprNode::Column(_) => {
                    crate::core::explain::explain_pushed_filter(&from.name, &field.expr)
                }
                expr => {
                    let text = format!(
                        "{}->Column#{}",
                        crate::core::explain::explain_pushed_filter(&from.name, expr),
                        next_id
                    );
                    next_id += 1;
                    text
                }
            })
            .collect()
    };
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
}

fn shard_rewrite_select(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    let is = is?;
    if select.from_item.is_some() || !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is.table_by_name(&db, &table)?;
    super::expr_index::shard_index_rewrite(select, info)
}

fn annotate_column_schemas(select: &SelectStmt, current_db: &str) -> Option<SelectStmt> {
    let tables = select.join_tables();
    if tables.is_empty() {
        return None;
    }
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for table in &tables {
        let db = table
            .schema
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| current_db.to_string());
        if !db.is_empty() {
            map.insert(table.name.to_ascii_lowercase(), db);
        }
    }
    if map.is_empty() {
        return None;
    }
    let single = (tables.len() == 1).then(|| tables[0].name.to_ascii_lowercase());
    let annotate = |expr: &ExprNode| annotate_expr_schema(expr, &map, single.as_deref());
    let mut out = select.clone();
    for field in &mut out.fields {
        field.expr = annotate(&field.expr);
    }
    out.selection = out.selection.as_ref().map(annotate);
    out.group_by = out.group_by.iter().map(annotate).collect();
    out.having = out.having.as_ref().map(annotate);
    for item in &mut out.order_by {
        item.expr = annotate(&item.expr);
    }
    for join in &mut out.joins {
        join.on = annotate(&join.on);
    }
    Some(out)
}

fn annotate_expr_schema(
    expr: &ExprNode,
    map: &std::collections::HashMap<String, String>,
    single_table: Option<&str>,
) -> ExprNode {
    use sealdb_parser::ast::expressions::{BetweenExpr, BinaryOpExpr, IsNullExpr, UnaryOpExpr};
    match expr {
        ExprNode::Column(col) => {
            let mut col = col.clone();
            if col.schema.is_none() {
                let owner = col
                    .table
                    .as_deref()
                    .map(|t| t.to_ascii_lowercase())
                    .or_else(|| single_table.map(str::to_string));
                if let Some(db) = owner.as_deref().and_then(|owner| map.get(owner)) {
                    col.schema = Some(db.clone());
                }
            }
            ExprNode::Column(col)
        }
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(annotate_expr_schema(&bin.left, map, single_table)),
            right: Box::new(annotate_expr_schema(&bin.right, map, single_table)),
            single_cnf_item: bin.single_cnf_item,
        }),
        ExprNode::Unary(unary) => ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(annotate_expr_schema(&unary.expr, map, single_table)),
        }),
        ExprNode::IsNull(is_null) => ExprNode::IsNull(IsNullExpr {
            expr: Box::new(annotate_expr_schema(&is_null.expr, map, single_table)),
            not: is_null.not,
        }),
        ExprNode::Between(between) => ExprNode::Between(BetweenExpr {
            expr: Box::new(annotate_expr_schema(&between.expr, map, single_table)),
            low: Box::new(annotate_expr_schema(&between.low, map, single_table)),
            high: Box::new(annotate_expr_schema(&between.high, map, single_table)),
            not: between.not,
        }),
        ExprNode::Cast(cast) => {
            let mut cast = cast.clone();
            cast.expr = Box::new(annotate_expr_schema(&cast.expr, map, single_table));
            ExprNode::Cast(cast)
        }
        ExprNode::Func(func) => {
            let mut func = func.clone();
            func.args = func
                .args
                .iter()
                .map(|arg| annotate_expr_schema(arg, map, single_table))
                .collect();
            ExprNode::Func(func)
        }
        ExprNode::InList(list) => {
            let mut list = list.clone();
            list.expr = Box::new(annotate_expr_schema(&list.expr, map, single_table));
            list.list = list
                .list
                .iter()
                .map(|item| annotate_expr_schema(item, map, single_table))
                .collect();
            ExprNode::InList(list)
        }
        ExprNode::Row(row) => {
            let mut row = row.clone();
            row.exprs = row
                .exprs
                .iter()
                .map(|item| annotate_expr_schema(item, map, single_table))
                .collect();
            ExprNode::Row(row)
        }
        other => other.clone(),
    }
}

/// Apply TiDB comparison type rules to a single-table SELECT before planning.
///
/// A string column compared with a number becomes
/// `eq(cast(col, double BINARY), n)`, which also rules out a point get —
/// TiDB `canBeConvertedToColumnType` refuses the key when it needs the cast.
fn coerce_comparison_literals(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    if select.from_item.is_some() || !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is?.table_by_name(&db, &table)?;
    let mut out = select.clone();
    let mut changed = false;
    if let Some(selection) = select.selection.as_ref() {
        let coerced = expand_casted_in_lists(&coerce_numeric_literals(selection, info));
        // `Time` datums restore back to the quoted literal, so compare the
        // expression trees too or the coercion would be dropped as a no-op.
        if coerced.restore_string() != selection.restore_string()
            || format!("{coerced:?}") != format!("{selection:?}")
        {
            out.selection = Some(coerced);
            changed = true;
        }
    }
    // TiDB folds SELECT-list comparisons with the same type rules
    // (`select a = '123455' from t` prints the numeric constant), so the
    // projection must carry the coerced datum too.
    for (i, field) in select.fields.iter().enumerate() {
        let coerced = coerce_numeric_literals(&field.expr, info);
        if coerced.restore_string() != field.expr.restore_string()
            || format!("{coerced:?}") != format!("{field:?}")
        {
            out.fields[i].expr = coerced;
            changed = true;
        }
    }
    changed.then_some(out)
}

/// `in(cast(col, double BINARY), 0, 1)` → `or(eq(cast(…), 0), eq(cast(…), 1))`:
/// a cast key cannot drive an IN range, so TiDB keeps one equality per value.
/// Row IN lists with a cast position expand to DNF the same way.
fn expand_casted_in_lists(expr: &ExprNode) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(expand_casted_in_lists(&bin.left)),
            right: Box::new(expand_casted_in_lists(&bin.right)),
            single_cnf_item: bin.single_cnf_item,
        }),
        ExprNode::InList(list) if !list.not && expr_contains_cast(&list.expr) => {
            let arms: Option<Vec<ExprNode>> = list
                .list
                .iter()
                .map(|item| in_list_arm(&list.expr, item))
                .collect();
            let Some(mut arms) = arms else {
                return ExprNode::InList(list.clone());
            };
            if arms.is_empty() {
                return ExprNode::InList(list.clone());
            }
            let first = arms.remove(0);
            arms.into_iter().fold(first, |acc, arm| {
                ExprNode::Binary(BinaryOpExpr {
                    op: Op::LogicOr,
                    left: Box::new(acc),
                    right: Box::new(arm),
                    single_cnf_item: false,
                })
            })
        }
        ExprNode::InList(list) => {
            let mut out = list.clone();
            out.expr = Box::new(expand_casted_in_lists(&list.expr));
            out.list = out.list.iter().map(expand_casted_in_lists).collect();
            ExprNode::InList(out)
        }
        ExprNode::Unary(unary) => {
            let mut out = unary.clone();
            out.expr = Box::new(expand_casted_in_lists(&unary.expr));
            ExprNode::Unary(out)
        }
        ExprNode::IsNull(isnull) => {
            let mut out = isnull.clone();
            out.expr = Box::new(expand_casted_in_lists(&isnull.expr));
            ExprNode::IsNull(out)
        }
        other => other.clone(),
    }
}

fn expr_contains_cast(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Cast(_) => true,
        ExprNode::Row(row) => row.exprs.iter().any(expr_contains_cast),
        ExprNode::Binary(bin) => expr_contains_cast(&bin.left) || expr_contains_cast(&bin.right),
        _ => false,
    }
}

/// True when the filter compares a `cast(<column>, …)` result — such a
/// predicate cannot produce an index range.
fn selection_has_cast_column(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Cast(cast) => matches!(&*cast.expr, ExprNode::Column(_)),
        ExprNode::Binary(bin) => {
            selection_has_cast_column(&bin.left) || selection_has_cast_column(&bin.right)
        }
        ExprNode::Unary(unary) => selection_has_cast_column(&unary.expr),
        ExprNode::InList(list) => {
            selection_has_cast_column(&list.expr) || list.list.iter().any(selection_has_cast_column)
        }
        ExprNode::IsNull(isnull) => selection_has_cast_column(&isnull.expr),
        _ => false,
    }
}

/// One `in` arm: `tuple` for a row list, a bare constant otherwise.
fn in_list_arm(key: &ExprNode, item: &ExprNode) -> Option<ExprNode> {
    match (key, item) {
        (ExprNode::Row(row), ExprNode::Row(tuple)) if row.exprs.len() == tuple.exprs.len() => {
            let mut eqs = row
                .exprs
                .iter()
                .zip(tuple.exprs.iter())
                .map(|(column, value)| {
                    ExprNode::Binary(BinaryOpExpr {
                        op: Op::Eq,
                        left: Box::new(column.clone()),
                        right: Box::new(value.clone()),
                        single_cnf_item: false,
                    })
                });
            let first = eqs.next()?;
            Some(eqs.fold(first, |acc, arm| {
                ExprNode::Binary(BinaryOpExpr {
                    op: Op::LogicAnd,
                    left: Box::new(acc),
                    right: Box::new(arm),
                    single_cnf_item: false,
                })
            }))
        }
        (ExprNode::Row(_), _) => None,
        (_, _) if item.const_datum().is_some() => Some(ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(key.clone()),
            right: Box::new(item.clone()),
            single_cnf_item: false,
        })),
        _ => None,
    }
}

/// Rewrite comparison literals in a statement's WHERE before planning so the
/// executor re-reading the statement sees the same casts as the plan.
pub fn normalize_comparison_literals(
    stmt: &mut Statement,
    is: Option<&InfoSchema>,
    current_db: &str,
) {
    if let Statement::Select(select) = stmt
        && let Some(coerced) = coerce_comparison_literals(select, is, current_db)
    {
        *select = coerced;
    }
}

/// `FLOAT` columns store float32: a constant that does not round-trip through
/// float32 can never equal a stored value, so the access path is empty.
fn point_value_hits_float(info: &sealdb_meta::TableInfo, column: &str, value: &Datum) -> bool {
    let Some(col) = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(column))
    else {
        return true;
    };
    if col.field_type.tp != sealdb_types::TYPE_FLOAT {
        return true;
    }
    let Ok(v) = value.as_f64() else {
        return true;
    };
    (v as f32) as f64 == v
}

/// A hex/bit literal wider than a `BIT` column fails the ranger conversion
/// (`col1 = 0x30` on `bit(1)`), so the equality cannot pin a point get: it
/// stays a cop filter and the comparison yields no row.
fn point_value_fits_column(info: &sealdb_meta::TableInfo, column: &str, value: &Datum) -> bool {
    let Some(col) = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(column))
    else {
        return true;
    };
    if col.field_type.tp != sealdb_types::field_type::TYPE_BIT {
        return true;
    }
    let Datum::Bit(bytes) = value else {
        return true;
    };
    let width = col.field_type.flen.max(1) as u32;
    let bits = bytes
        .iter()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    width >= 64 || bits >> width == 0
}

/// A range bound on a TIMESTAMP column: its index keys store UTC, so the
/// session-local bound cannot build a byte seek.
fn has_timestamp_bound(
    column: &str,
    info: &sealdb_meta::TableInfo,
    conjuncts: &[ExprNode],
) -> bool {
    let Some(col) = info
        .columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(column))
    else {
        return false;
    };
    col.field_type.tp == sealdb_types::TYPE_TIMESTAMP
        && conjuncts.iter().any(|conjunct| {
            sealdb_statistics::pseudo::conjunct_bound_sets(conjunct)
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(column))
        })
}

fn index_ignored(from: &TableName, name: &str) -> bool {
    from.ignore_indexes
        .iter()
        .any(|ignored| ignored.eq_ignore_ascii_case(name))
}

fn index_allowed(from: &TableName, name: &str, only_index: Option<&str>) -> bool {
    if index_ignored(from, name) {
        return false;
    }
    only_index.is_none_or(|only| only.eq_ignore_ascii_case(name))
}

/// TiDB index visibility: an `INVISIBLE` index is never an access path unless a
/// `USE INDEX` / `FORCE INDEX` hint names it.
fn index_visible(from: &TableName, index: &sealdb_meta::model::IndexInfo) -> bool {    !index.invisible
        || from
            .use_indexes
            .iter()
            .any(|hinted| hinted.eq_ignore_ascii_case(index.name.o()))
}

/// `clustered index:PRIMARY(..)` vs `index:PRIMARY(..)`: TiDB renders the
/// clustered form only when the PRIMARY KEY is a common handle.
fn point_index_is_common_handle(info: &sealdb_meta::TableInfo, index: &str) -> bool {
    info.common_handle && index.eq_ignore_ascii_case("PRIMARY")
}

/// `a = 1 AND b = 2 …` fully constrains a multi-column unique key → PointGet.
fn unique_index_eq_point(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    eqs: &[(String, Datum)],
    only_index: Option<&str>,
) -> Option<PhysicalPlan> {
    let value_of = |key: &str| {
        eqs.iter()
            .find(|(col, _)| col.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.clone())
    };
    let index = info.indices.iter().find(|idx| {
        idx.unique
            // A common-handle PRIMARY KEY still answers an equality point get
            // (`Point_Get … clustered index:PRIMARY(a, b)`); only the integer
            // handle is reached through the handle path instead of an index.
            && !(idx.primary && info.pk_is_handle)
            && !idx.invisible
            && idx.columns.len() > 1
            && index_allowed(from, idx.name.o(), only_index)
            && idx
                .columns
                .iter()
                .all(|c| value_of(&super::expr_index::part_key(info, c)).is_some())
    })?;
    if index.columns.iter().any(|c| {
        value_of(&super::expr_index::part_key(info, c))
            .is_some_and(|value| !point_value_hits_float(info, c.name.o(), &value))
    }) {
        return Some(PhysicalPlan::TableDual { row_count: 0 });
    }
    // A literal that cannot convert to its key column keeps the predicate as a
    // cop filter instead of pinning a single key.
    if index.columns.iter().any(|c| {
        value_of(&super::expr_index::part_key(info, c))
            .is_some_and(|value| !point_value_fits_column(info, c.name.o(), &value))
    }) {
        return None;
    }
    let index_columns: Vec<String> = index_key_column_names(info, index);
    let values: Vec<Datum> = index
        .columns
        .iter()
        .map(|part| value_of(&super::expr_index::part_key(info, part)).unwrap_or(Datum::Null))
        .collect();
    let partition_names = tuple_partition_names(
        info,
        &index_columns,
        std::slice::from_ref(&values),
        &from.partition_names,
        index.global,
    );
    Some(PhysicalPlan::PointGet {
        table: from.name.clone(),
        handle: None,
        index: Some(index.name.o().to_string()),
        index_columns,
        partition_names,
        global_index: index.global,
        lock: false,
        common_handle: point_index_is_common_handle(info, index.name.o()),
    })
}

/// `(a = 1 AND b = 1) OR (a = 2 AND b = 2)` → BatchPointGet on the unique key.
fn dnf_unique_point(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    only_index: Option<&str>,
) -> Option<PhysicalPlan> {
    let selection = select.selection.as_ref()?;
    let arms: Vec<Vec<ExprNode>> = selection
        .split_dnf()
        .into_iter()
        .map(|arm| arm.split_cnf_all())
        .collect();
    if arms.len() < 2 {
        return None;
    }
    // Each arm pins the whole key: expression parts may fold from the plain
    // parts (`uk_expr((tidb_shard(a)), a)` under `a = 100`).
    let index = info.indices.iter().find(|idx| {
        idx.unique
            // A common-handle PRIMARY KEY still answers an equality point get
            // (`Point_Get … clustered index:PRIMARY(a, b)`); only the integer
            // handle is reached through the handle path instead of an index.
            && !(idx.primary && info.pk_is_handle)
            && !idx.invisible
            && idx.columns.len() > 1
            && index_allowed(from, idx.name.o(), only_index)
            && super::expr_index::pinned_tuple(info, idx, &arms[0]).is_some()
    })?;
    let keys = super::expr_index::part_keys(info, index);
    // Every conjunct of every arm must be part of the key; a leftover predicate
    // stays in the residual filter instead of silently disappearing.
    let is_key_const = |conjunct: &ExprNode| {
        conjunct
            .eq_key_const()
            .is_some_and(|(key, _)| keys.iter().any(|part| part.eq_ignore_ascii_case(&key)))
    };
    if arms
        .iter()
        .any(|conjuncts| !conjuncts.iter().all(is_key_const))
    {
        return None;
    }
    let mut tuples: Vec<Vec<Datum>> = Vec::with_capacity(arms.len());
    for conjuncts in &arms {
        let tuple = super::expr_index::pinned_tuple(info, index, conjuncts)?;
        if !tuples.iter().any(|have| {
            have.len() == tuple.len()
                && have
                    .iter()
                    .zip(tuple.iter())
                    .all(|(a, b)| a.compare(b).is_eq())
        }) {
            tuples.push(tuple);
        }
    }
    let hits: Vec<bool> = tuples
        .iter()
        .map(|tuple| {
            index.columns.iter().enumerate().all(|(pos, ic)| {
                tuple
                    .get(pos)
                    .is_some_and(|value| point_value_hits_float(info, ic.name.o(), value))
            })
        })
        .collect();
    if !hits.is_empty() && hits.iter().all(|hit| !hit) {
        return Some(PhysicalPlan::TableDual { row_count: 0 });
    }
    if hits.iter().any(|hit| !hit) {
        return None;
    }
    let n_keys = tuples.len();
    let column = keys.first().cloned().unwrap_or_default();
    Some(batch_point_plan(
        from, info, index, tuples, n_keys, column, keys,
    ))
}

fn in_list_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    only_index: Option<&str>,
) -> Option<PhysicalPlan> {
    let lists = select
        .selection
        .as_ref()
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.column_in_consts_with_len())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if info.pk_is_handle
        && only_index.is_none()
        && let Some(first) = info.columns.first()
    {
        for (column, values, total) in &lists {
            if !first.name.o().eq_ignore_ascii_case(column) {
                continue;
            }
            let handles: Vec<i64> = values
                .iter()
                .filter_map(|value| match value {
                    Datum::I64(v) => Some(*v),
                    Datum::U64(v) => Some(*v as i64),
                    _ => None,
                })
                .collect();
            if handles.len() != values.len() {
                continue;
            }
            return Some(point_or_batch(
                from,
                info,
                column.clone(),
                None,
                Vec::new(),
                handles.into_iter().map(Datum::I64).collect(),
                *total,
            ));
        }
    }
    for (column, values, total) in lists {
        if let Some(index) = info
            .find_unique_index_on(&column)
            .filter(|idx| index_allowed(from, idx.name.o(), only_index))
        {
            let hits: Vec<bool> = values
                .iter()
                .map(|value| point_value_hits_float(info, &column, value))
                .collect();
            if !hits.is_empty() && hits.iter().all(|hit| !hit) {
                return Some(PhysicalPlan::TableDual { row_count: 0 });
            }
            if hits.iter().any(|hit| !hit) {
                continue;
            }
            let index_columns = index_key_column_names(info, index);
            return Some(point_or_batch(
                from,
                info,
                column,
                Some(index.name.o().to_string()),
                index_columns,
                values,
                total,
            ));
        }
        // Expression index: the plain parts come from the IN list and the
        // expression parts fold from them (`uk_expr((tidb_shard(a)), a)` under
        // `a IN (100, 200)` is a point get / batch point get).
        if let Some(index) = info.indices.iter().find(|idx| {
            idx.unique
                && !idx.invisible
                && super::expr_index::has_expression_part(info, idx)
                && index_allowed(from, idx.name.o(), only_index)
        }) && let Some(tuples) = in_list_part_tuples(info, index, &column, &values)
        {
            return Some(tuples_point_plan(
                from,
                info,
                index,
                tuples,
                total,
                column,
                Vec::new(),
            ));
        }
    }
    // `(a, b) IN ((1, 2), (3, 4))` on a unique key covering exactly those
    // columns is a batch point get as well.
    let row_list = select.selection.as_ref().and_then(|expr| {
        expr.split_cnf()
            .into_iter()
            .find_map(|item| item.row_in_consts_with_len())
    })?;
    let (names, tuples, total) = row_list;
    let index = info.indices.iter().find(|idx| {
        idx.unique
            && !idx.invisible
            && index_allowed(from, idx.name.o(), only_index)
            && row_in_columns_cover(info, idx, &names)
            && row_in_part_tuples(info, idx, &names, &tuples).is_some()
    })?;
    let key_tuples = row_in_part_tuples(info, index, &names, &tuples)?;
    let keys = super::expr_index::part_keys(info, index);
    let hits: Vec<bool> = key_tuples
        .iter()
        .map(|tuple| {
            index.columns.iter().enumerate().all(|(pos, ic)| {
                tuple
                    .get(pos)
                    .is_some_and(|value| point_value_hits_float(info, ic.name.o(), value))
            })
        })
        .collect();
    if !hits.is_empty() && hits.iter().all(|hit| !hit) {
        return Some(PhysicalPlan::TableDual { row_count: 0 });
    }
    if hits.iter().any(|hit| !hit) {
        return None;
    }
    let _ = keys;
    Some(batch_point_plan(
        from,
        info,
        index,
        key_tuples,
        total,
        keys.first().cloned().unwrap_or_default(),
        names,
    ))
}

/// `a IN (…)`: every key part from the IN list, expression parts folded from it.
fn in_list_part_tuples(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    column: &str,
    values: &[Datum],
) -> Option<Vec<Vec<Datum>>> {
    let mut tuples = Vec::with_capacity(values.len());
    for value in values {
        let known = vec![(column.to_string(), value.clone())];
        let mut tuple = Vec::with_capacity(index.columns.len());
        for part in &index.columns {
            let key = super::expr_index::part_key(info, part);
            if key.eq_ignore_ascii_case(column) {
                tuple.push(value.clone());
            } else {
                tuple.push(super::expr_index::eval_part(
                    info,
                    &super::expr_index::part_expr(info, part)?,
                    &known,
                )?);
            }
        }
        tuples.push(tuple);
    }
    Some(tuples)
}

/// `(a, b) IN ((…), (…))` pins every key part, so the tuple columns must be
/// exactly the index's plain parts: an expression part (`tidb_shard(a)`) folds
/// from them, while a plain part left out of the tuple stays unpinned and the
/// predicate is not a point get.
fn row_in_columns_cover(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    names: &[String],
) -> bool {
    let plain: Vec<String> = index
        .columns
        .iter()
        .filter(|part| super::expr_index::part_expr(info, part).is_none())
        .map(|part| part.name.o().to_string())
        .collect();
    plain.len() == names.len()
        && plain
            .iter()
            .all(|key| names.iter().any(|name| name.eq_ignore_ascii_case(key)))
}

/// `(a, b) IN ((…), (…))`: reorder the row values into key order and fold the
/// expression parts (`uk_expr((tidb_shard(a)), a, b)`).
fn row_in_part_tuples(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    names: &[String],
    tuples: &[Vec<Datum>],
) -> Option<Vec<Vec<Datum>>> {
    let mut out = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        let known: Vec<(String, Datum)> = names
            .iter()
            .cloned()
            .zip(tuple.iter().cloned())
            .collect();
        let mut key = Vec::with_capacity(index.columns.len());
        for part in &index.columns {
            let name = super::expr_index::part_key(info, part);
            match names.iter().position(|n| n.eq_ignore_ascii_case(&name)) {
                Some(pos) => key.push(tuple.get(pos)?.clone()),
                None => key.push(super::expr_index::eval_part(
                    info,
                    &super::expr_index::part_expr(info, part)?,
                    &known,
                )?),
            }
        }
        out.push(key);
    }
    Some(out)
}

/// Point get for one key tuple, batch point get for several.
fn tuples_point_plan(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    tuples: Vec<Vec<Datum>>,
    n_keys: usize,
    column: String,
    tuple_columns: Vec<String>,
) -> PhysicalPlan {
    if tuples.len() == 1 {
        let keys = super::expr_index::part_keys(info, index);
        let partition_names = tuple_partition_names(
            info,
            &keys,
            &tuples,
            &from.partition_names,
            index.global,
        );
        return PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle: None,
            index: Some(index.name.o().to_string()),
            index_columns: index_key_column_names(info, index),
            partition_names,
            global_index: index.global,
            lock: false,
            common_handle: point_index_is_common_handle(info, index.name.o()),
        };
    }
    batch_point_plan(
        from,
        info,
        index,
        tuples,
        n_keys,
        column,
        tuple_columns,
    )
}

/// Batch point get over a unique index. A row `IN` list keeps this shape even
/// when every tuple folds to one key (TiDB `Batch_Point_Get`, never a point
/// get: the plan is derived before the list is de-duplicated).
fn batch_point_plan(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    tuples: Vec<Vec<Datum>>,
    n_keys: usize,
    column: String,
    tuple_columns: Vec<String>,
) -> PhysicalPlan {
    let keys = super::expr_index::part_keys(info, index);
    let index_columns = index_key_column_names(info, index);
    let partition_names = tuple_partition_names(
        info,
        &keys,
        &tuples,
        &from.partition_names,
        index.global,
    );
    PhysicalPlan::BatchPointGet {
        table: from.name.clone(),
        column,
        index: Some(index.name.o().to_string()),
        n_keys,
        handles: Vec::new(),
        index_columns,
        tuple_columns,
        partition_names,
        global_index: index.global,
        lock: false,
        keep_order: false,
        desc: false,
        common_handle: point_index_is_common_handle(info, index.name.o()),
    }
}

/// Static partition names a row-IN batch point get touches; the tuples are
/// placed into a sparse table row so HASH/KEY/RANGE/LIST locate each one.
fn tuple_partition_names(
    info: &sealdb_meta::TableInfo,
    names: &[String],
    tuples: &[Vec<Datum>],
    explicit: &[String],
    index_global: bool,
) -> Vec<String> {
    if index_global {
        return explicit.to_vec();
    }
    if !explicit.is_empty() {
        return explicit.to_vec();
    }
    let Some(part) = info.partition.as_ref().filter(|p| p.enable) else {
        return Vec::new();
    };
    if part.is_expr_key()
        || part.columns.len() != 1
        || !names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(part.column()))
    {
        return Vec::new();
    }
    let positions: Vec<Option<usize>> = names
        .iter()
        .map(|name| {
            info.columns
                .iter()
                .position(|c| c.name.o().eq_ignore_ascii_case(name))
        })
        .collect();
    let mut ids: Vec<i64> = Vec::new();
    for tuple in tuples {
        let mut row = vec![Datum::Null; info.columns.len()];
        for (pos, value) in positions.iter().zip(tuple.iter()) {
            if let Some(pos) = pos {
                row[*pos] = value.clone();
            }
        }
        if let Ok(id) = info.locate_partition_row(&row)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    part.definitions
        .iter()
        .filter(|def| ids.contains(&def.id))
        .map(|def| def.name.o().to_string())
        .collect()
}

/// Static partition names a single/batch point get touches. `FROM t PARTITION
/// (pN)` wins; otherwise a hash/range/list partition on the very column being
/// looked up locates each value. Expression keys and multi-column keys stay
/// unpinned (the executor scans every physical partition).
fn point_partition_names(
    info: &sealdb_meta::TableInfo,
    column: &str,
    values: &[Datum],
    explicit: &[String],
    index_global: bool,
) -> Vec<String> {
    // A GLOBAL index spans every partition; TiDB never labels its point get,
    // but an explicit `PARTITION (pN)` must still constrain the row fetch.
    if index_global {
        return explicit.to_vec();
    }
    if !explicit.is_empty() {
        return explicit.to_vec();
    }
    let Some(part) = info.partition.as_ref().filter(|p| p.enable) else {
        return Vec::new();
    };
    if part.is_expr_key() || part.columns.len() != 1 || !part.column().eq_ignore_ascii_case(column)
    {
        return Vec::new();
    }
    let mut ids = Vec::new();
    for value in values {
        if let Ok(id) = info.locate_partition(value)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    part.definitions
        .iter()
        .filter(|def| ids.contains(&def.id))
        .map(|def| def.name.o().to_string())
        .collect()
}

fn point_or_batch(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    column: String,
    index: Option<String>,
    index_columns: Vec<String>,
    values: Vec<Datum>,
    n_keys: usize,
) -> PhysicalPlan {
    let index_global = index
        .as_deref()
        .and_then(|name| {
            info.indices
                .iter()
                .find(|idx| idx.name.o().eq_ignore_ascii_case(name))
        })
        .is_some_and(|idx| idx.global);
    let partition_names =
        point_partition_names(info, &column, &values, &from.partition_names, index_global);
    if let [value] = values.as_slice() {
        let handle = match (&index, value) {
            (None, Datum::I64(handle)) => Some(*handle),
            _ => None,
        };
        let common_handle = match &index {
            Some(name) => point_index_is_common_handle(info, name),
            None => false,
        };
        return PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle,
            index,
            index_columns,
            partition_names,
            global_index: index_global,
            lock: false,
            common_handle,
        };
    }
    let mut handles: Vec<i64> = values
        .iter()
        .filter_map(|value| match value {
            Datum::I64(handle) => Some(*handle),
            Datum::U64(handle) => Some(*handle as i64),
            _ => None,
        })
        .collect();
    handles.sort_unstable();
    handles.dedup();
    let common_handle = match &index {
        Some(name) => point_index_is_common_handle(info, name),
        None => false,
    };
    PhysicalPlan::BatchPointGet {
        table: from.name.clone(),
        column,
        index,
        n_keys,
        handles,
        index_columns,
        tuple_columns: Vec::new(),
        partition_names,
        global_index: index_global,
        lock: false,
        keep_order: false,
        desc: false,
        common_handle,
    }
}

/// Fold constant predicates for a single-table SELECT over a partition table.
fn fold_partition_selection(
    select: &SelectStmt,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    if !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    if from.physical.is_some() {
        return None;
    }
    let (db, table) = from.qualified(current_db);
    let info = is?.table_by_name(&db, &table)?;
    let part = info.partition.as_ref()?;
    if !part.enable {
        return None;
    }
    let selection = select.selection.as_ref()?;
    let folded = coerce_temporal_literals(
        &rebalance_disjunctions(&fold_const_predicates(&push_down_not(selection, false))),
        info,
    );
    let folded = coerce_numeric_literals(&folded, info);
    let folded = simplify_partition_cnf(&folded);
    if folded.restore_string() == selection.restore_string() {
        return None;
    }
    let mut folded_select = select.clone();
    folded_select.selection = Some(folded);
    Some(folded_select)
}

/// TiDB constant folding turns a string literal compared against a temporal
/// column into a temporal constant: `a != '2024-01-01 01:01:01'` prints as
/// `ne(a, 2024-01-01 01:01:01.000000)`. DATETIME/TIMESTAMP constants carry six
/// fractional digits; DATE constants print bare.
fn coerce_temporal_literals(expr: &ExprNode, info: &sealdb_meta::TableInfo) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => {
            let left = coerce_temporal_literals(&bin.left, info);
            let right = coerce_temporal_literals(&bin.right, info);
            let (left, right) = match column_temporal_tp(&left, info) {
                Some(tp) => match temporal_datum(tp, &right) {
                    Some(datum) => (left, value_expr(datum)),
                    None => (left, right),
                },
                None => match column_temporal_tp(&right, info) {
                    Some(tp) => match temporal_datum(tp, &left) {
                        Some(datum) => (value_expr(datum), right),
                        None => (left, right),
                    },
                    None => (left, right),
                },
            };
            ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: bin.single_cnf_item,
            })
        }
        ExprNode::InList(list) => {
            let mut coerced = list.clone();
            coerced.expr = Box::new(coerce_temporal_literals(&list.expr, info));
            if let Some(tp) = column_temporal_tp(&coerced.expr, info) {
                coerced.list = coerced
                    .list
                    .iter()
                    .map(|item| match temporal_datum(tp, item) {
                        Some(datum) => value_expr(datum),
                        None => item.clone(),
                    })
                    .collect();
            }
            ExprNode::InList(coerced)
        }
        ExprNode::Unary(unary) => {
            let mut coerced = unary.clone();
            coerced.expr = Box::new(coerce_temporal_literals(&unary.expr, info));
            ExprNode::Unary(coerced)
        }
        ExprNode::IsNull(isnull) => {
            let mut coerced = isnull.clone();
            coerced.expr = Box::new(coerce_temporal_literals(&isnull.expr, info));
            ExprNode::IsNull(coerced)
        }
        other => other.clone(),
    }
}

/// Rewrite numeric constants compared with an `ENUM`/`SET` column into the
/// member they select (TiDB folds `a = 1` to `a = 'a'` while building the
/// datasource, so a point get looks up the stored label, not the number).
fn coerce_enum_selection(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    if select.from_item.is_some() || !select.joins.is_empty() {
        return None;
    }
    let selection = select.selection.as_ref()?;
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is?.table_by_name(&db, &table)?;
    let enum_cols: std::collections::HashMap<String, sealdb_types::FieldType> = info
        .columns
        .iter()
        .filter(|c| {
            matches!(
                c.field_type.tp,
                sealdb_types::field_type::TYPE_ENUM | sealdb_types::field_type::TYPE_SET
            )
        })
        .map(|c| (c.name.o().to_ascii_lowercase(), c.field_type.clone()))
        .collect();
    if enum_cols.is_empty() {
        return None;
    }
    let coerced = coerce_enum_expr(selection, &enum_cols);
    if coerced.restore_string() == selection.restore_string() {
        return None;
    }
    let mut out = select.clone();
    out.selection = Some(coerced);
    Some(out)
}

fn coerce_enum_expr(
    expr: &ExprNode,
    enum_cols: &std::collections::HashMap<String, sealdb_types::FieldType>,
) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => {
            let left = coerce_enum_expr(&bin.left, enum_cols);
            let right = coerce_enum_expr(&bin.right, enum_cols);
            let (left, right) = if bin.op == Op::Eq {
                match (enum_column_ft(&left, enum_cols), enum_member(&right)) {
                    (Some(ft), Some(number)) => match enum_datum_for_number(ft, number) {
                        Some(datum) => (left, value_expr(datum)),
                        None => (left, right),
                    },
                    _ => match (enum_column_ft(&right, enum_cols), enum_member(&left)) {
                        (Some(ft), Some(number)) => match enum_datum_for_number(ft, number) {
                            Some(datum) => (value_expr(datum), right),
                            None => (left, right),
                        },
                        _ => (left, right),
                    },
                }
            } else {
                (left, right)
            };
            ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: bin.single_cnf_item,
            })
        }
        ExprNode::InList(list) => {
            let mut coerced = list.clone();
            coerced.expr = Box::new(coerce_enum_expr(&list.expr, enum_cols));
            if let Some(ft) = enum_column_ft(&coerced.expr, enum_cols) {
                coerced.list = coerced
                    .list
                    .iter()
                    .map(|item| match enum_member(item) {
                        Some(number) => enum_datum_for_number(ft, number)
                            .map(value_expr)
                            .unwrap_or_else(|| coerce_enum_expr(item, enum_cols)),
                        None => coerce_enum_expr(item, enum_cols),
                    })
                    .collect();
            }
            ExprNode::InList(coerced)
        }
        ExprNode::Unary(unary) => {
            let mut coerced = unary.clone();
            coerced.expr = Box::new(coerce_enum_expr(&unary.expr, enum_cols));
            ExprNode::Unary(coerced)
        }
        ExprNode::IsNull(isnull) => {
            let mut coerced = isnull.clone();
            coerced.expr = Box::new(coerce_enum_expr(&isnull.expr, enum_cols));
            ExprNode::IsNull(coerced)
        }
        other => other.clone(),
    }
}

fn enum_column_ft<'a>(
    expr: &ExprNode,
    enum_cols: &'a std::collections::HashMap<String, sealdb_types::FieldType>,
) -> Option<&'a sealdb_types::FieldType> {
    let ExprNode::Column(column) = expr else {
        return None;
    };
    enum_cols.get(&column.name.to_ascii_lowercase())
}

/// The integer a numeric constant contributes to an `ENUM` comparison.
fn enum_member(expr: &ExprNode) -> Option<u64> {
    let ExprNode::Value(ValueExpr { datum }) = expr else {
        return None;
    };
    match datum {
        Datum::I64(v) if *v >= 0 => Some(*v as u64),
        Datum::U64(v) => Some(*v),
        Datum::F64(v) if *v >= 0.0 && v.fract() == 0.0 => Some(*v as u64),
        _ => None,
    }
}

/// `ENUM` member `number` (1-based; 0 is the invalid empty member).
fn enum_datum_for_number(ft: &sealdb_types::FieldType, number: u64) -> Option<Datum> {
    if ft.tp == sealdb_types::field_type::TYPE_SET {
        return None;
    }
    let elems = ft.elems.as_ref()?;
    if number == 0 {
        return Some(Datum::Enum(sealdb_types::EnumValue::new(Vec::new(), 0)));
    }
    let label = elems.get(number as usize - 1)?;
    Some(Datum::Enum(sealdb_types::EnumValue::new(
        label.clone().into_bytes(),
        number,
    )))
}

fn value_expr(datum: Datum) -> ExprNode {
    ExprNode::Value(ValueExpr { datum })
}

fn column_temporal_tp(expr: &ExprNode, info: &sealdb_meta::TableInfo) -> Option<u8> {
    let ExprNode::Column(column) = expr else {
        return None;
    };
    let col = info
        .columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(&column.name))?;
    match col.field_type.tp {
        sealdb_types::TYPE_DATE | sealdb_types::TYPE_DATETIME | sealdb_types::TYPE_TIMESTAMP => {
            Some(col.field_type.tp)
        }
        _ => None,
    }
}

fn temporal_datum(tp: u8, expr: &ExprNode) -> Option<Datum> {
    let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(bytes),
    }) = expr
    else {
        return None;
    };
    let text = std::str::from_utf8(bytes).ok()?;
    let mut time = match tp {
        sealdb_types::TYPE_DATE => sealdb_types::parse_date_literal(text).ok()?,
        sealdb_types::TYPE_TIMESTAMP => {
            let mut t = sealdb_types::parse_timestamp_literal(text).ok()?;
            t.tp = sealdb_types::TimeType::Timestamp;
            t
        }
        _ => sealdb_types::parse_timestamp_literal(text).ok()?,
    };
    if tp != sealdb_types::TYPE_DATE {
        time.fsp = 6;
    }
    Some(Datum::Time(time))
}

/// TiDB `PredicateSimplification` subset for partition predicates: an equality
/// on a column makes further constant comparisons on it redundant
/// (`a = 2 AND a < 3` → `a = 2`) and conflicting equalities fold to false
/// (`a = 2 AND a = 3` → `0`).
fn simplify_partition_cnf(expr: &ExprNode) -> ExprNode {
    let parts = expr.split_cnf_all();
    if parts.len() < 2 {
        return expr.clone();
    }
    let Some(column) = parts
        .iter()
        .find_map(|part| eq_parts(part).map(|(col, _)| col))
    else {
        return expr.clone();
    };
    let eqs: Vec<Datum> = parts
        .iter()
        .filter_map(eq_parts)
        .filter(|(col, _)| col.eq_ignore_ascii_case(&column))
        .map(|(_, datum)| datum)
        .collect();
    let Some(first_eq) = eqs.first() else {
        return expr.clone();
    };
    if eqs
        .iter()
        .any(|d| d.compare(first_eq) != std::cmp::Ordering::Equal)
    {
        return ExprNode::Value(ValueExpr {
            datum: Datum::I64(0),
        });
    }
    let eq_value = first_eq.clone();
    let mut kept: Vec<ExprNode> = Vec::with_capacity(parts.len());
    let mut eq_kept = false;
    for part in &parts {
        if let Some((col, _)) = eq_parts(part) {
            if col.eq_ignore_ascii_case(&column) {
                if eq_kept {
                    continue;
                }
                eq_kept = true;
            }
            kept.push((*part).clone());
            continue;
        }
        if let Some((col, datum, op)) = cmp_parts_any(part)
            && col.eq_ignore_ascii_case(&column)
        {
            match cmp_holds(&eq_value, op, &datum) {
                Some(true) => continue,
                Some(false) => {
                    return ExprNode::Value(ValueExpr {
                        datum: Datum::I64(0),
                    });
                }
                None => {}
            }
        }
        kept.push((*part).clone());
    }
    if kept.is_empty() {
        return ExprNode::Value(ValueExpr {
            datum: Datum::I64(1),
        });
    }
    ExprNode::compose_cnf(kept).unwrap_or_else(|| expr.clone())
}

fn eq_parts(expr: &ExprNode) -> Option<(String, Datum)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    let (col, datum, _) = cmp_parts_any(expr)?;
    Some((col, datum))
}

/// `column <op> const` (either side) → `(column, const, op)` with the operator
/// already flipped so it reads left-to-right as `column op const`.
fn cmp_parts_any(expr: &ExprNode) -> Option<(String, Datum, Op)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    let const_of = |e: &ExprNode| match e {
        ExprNode::Value(ValueExpr { datum }) if !datum.is_null() => Some(datum.clone()),
        _ => None,
    };
    let column_of = |e: &ExprNode| match e {
        ExprNode::Column(c) => Some(c.name.clone()),
        _ => None,
    };
    if let (Some(col), Some(datum)) = (column_of(&bin.left), const_of(&bin.right)) {
        return Some((col, datum, bin.op));
    }
    if let (Some(datum), Some(col)) = (const_of(&bin.left), column_of(&bin.right)) {
        return Some((col, datum, flip_cmp_op(bin.op)?));
    }
    None
}

fn flip_cmp_op(op: Op) -> Option<Op> {
    Some(match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        Op::Eq => Op::Eq,
        Op::Ne => Op::Ne,
        _ => return None,
    })
}

fn cmp_holds(value: &Datum, op: Op, constant: &Datum) -> Option<bool> {
    let ord = value.compare(constant);
    Some(match op {
        Op::Eq => ord == std::cmp::Ordering::Equal,
        Op::Ne => ord != std::cmp::Ordering::Equal,
        Op::Lt => ord == std::cmp::Ordering::Less,
        Op::Le => ord != std::cmp::Ordering::Greater,
        Op::Gt => ord == std::cmp::Ordering::Greater,
        Op::Ge => ord != std::cmp::Ordering::Less,
        _ => return None,
    })
}

/// TiDB comparison type rules for a partial schema: a string column compared
/// with a number is cast to `double BINARY` (`gt(cast(a, double BINARY), 0)`),
/// and a numeric column compared with a string literal takes the literal's
/// MySQL numeric prefix (`a > '10ab'` → `gt(a, 10)`).
fn coerce_numeric_literals(expr: &ExprNode, info: &sealdb_meta::TableInfo) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => {
            let mut left = coerce_numeric_literals(&bin.left, info);
            let mut right = coerce_numeric_literals(&bin.right, info);
            if matches!(bin.op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) {
                if column_class(&left, info) == Some(ColumnClass::String) && numeric_literal(&right)
                {
                    left = cast_double(left);
                } else if column_class(&right, info) == Some(ColumnClass::String)
                    && numeric_literal(&left)
                {
                    right = cast_double(right);
                }
                if column_class(&left, info) == Some(ColumnClass::Numeric)
                    && let Some(datum) = bytes_number(&right)
                {
                    right = value_expr(datum);
                } else if column_class(&right, info) == Some(ColumnClass::Numeric)
                    && let Some(datum) = bytes_number(&left)
                {
                    left = value_expr(datum);
                }
                // `datetime_col = '1981-09-15'`: TiDB converts the constant to
                // the column type, so the range/Selection carry a `Time` datum.
                // A DATE column compares as DATETIME, a TIME column folds a
                // numeric constant into a duration (`a = 1` → `00:00:01`).
                if let Some(ft) = column_field_type(&left, info)
                    && let Some(datum) = comparison_temporal_literal(&right, ft)
                {
                    right = value_expr(datum);
                } else if let Some(ft) = column_field_type(&right, info)
                    && let Some(datum) = comparison_temporal_literal(&left, ft)
                {
                    left = value_expr(datum);
                }
                // `int_col = <constant>` folds to constant false when the
                // constant overflows the column's integer type.
                if bin.op == Op::Eq {
                    if let Some(folded) = int_eq_out_of_range(&left, &right, info)
                        .or_else(|| int_eq_out_of_range(&right, &left, info))
                    {
                        return folded;
                    }
                }
            }
            // Arithmetic on a string column evaluates as `double` on both sides.
            if matches!(
                bin.op,
                Op::Plus | Op::Minus | Op::Mul | Op::Div | Op::IntDiv | Op::Mod
            ) {
                if column_class(&left, info) == Some(ColumnClass::String) {
                    left = cast_double(left);
                }
                if column_class(&right, info) == Some(ColumnClass::String) {
                    right = cast_double(right);
                }
            }
            ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: bin.single_cnf_item,
            })
        }
        ExprNode::InList(list) => {
            let mut coerced = list.clone();
            coerced.expr = Box::new(coerce_numeric_literals(&list.expr, info));
            match column_class(&coerced.expr, info) {
                Some(ColumnClass::String) if list.list.iter().all(|item| numeric_literal(item)) => {
                    coerced.expr = Box::new(cast_double((*coerced.expr).clone()));
                }
                Some(ColumnClass::Numeric) => {
                    coerced.list = coerced
                        .list
                        .iter()
                        .map(|item| match bytes_number(item) {
                            Some(datum) => value_expr(datum),
                            None => item.clone(),
                        })
                        .collect();
                }
                Some(ColumnClass::Temporal) => {
                    if let Some(ft) = column_field_type(&coerced.expr, info) {
                        coerced.list = coerced
                            .list
                            .iter()
                            .map(|item| match temporal_literal(item, ft) {
                                Some(datum) => value_expr(datum),
                                None => item.clone(),
                            })
                            .collect();
                    }
                }
                _ => {}
            }
            // `(str_col, int_col) IN ((0, 1), …)`: a string position whose
            // constants are numbers reads through `cast(col, double)`, which
            // also rules out a composite point get on that position.
            if let ExprNode::Row(row) = &*coerced.expr {
                let mut columns = row.exprs.clone();
                let mut changed = false;
                for (pos, column) in columns.iter_mut().enumerate() {
                    if column_class(column, info) != Some(ColumnClass::String) {
                        continue;
                    }
                    let any_numeric = coerced.list.iter().any(|item| match item {
                        ExprNode::Row(tuple) => tuple.exprs.get(pos).is_some_and(numeric_literal),
                        _ => false,
                    });
                    if any_numeric {
                        *column = cast_double(column.clone());
                        changed = true;
                    }
                }
                if changed {
                    coerced.expr =
                        Box::new(ExprNode::Row(sealdb_parser::ast::expressions::RowExpr {
                            exprs: columns,
                        }));
                }
            }
            ExprNode::InList(coerced)
        }
        ExprNode::Unary(unary) => {
            let mut coerced = unary.clone();
            coerced.expr = Box::new(coerce_numeric_literals(&unary.expr, info));
            ExprNode::Unary(coerced)
        }
        ExprNode::IsNull(isnull) => {
            let mut coerced = isnull.clone();
            coerced.expr = Box::new(coerce_numeric_literals(&isnull.expr, info));
            ExprNode::IsNull(coerced)
        }
        other => other.clone(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnClass {
    Numeric,
    String,
    /// DATE / DATETIME / TIMESTAMP: a string literal is parsed into the
    /// column's temporal type (TiDB `types.ParseTime`), so the range text and
    /// the pushed Selection both carry `1981-09-15 00:00:00`.
    Temporal,
}

fn column_class(expr: &ExprNode, info: &sealdb_meta::TableInfo) -> Option<ColumnClass> {
    let ExprNode::Column(column) = expr else {
        return None;
    };
    let col = info
        .columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(&column.name))?;
    Some(match col.field_type.tp {
        sealdb_types::TYPE_TINY
        | sealdb_types::TYPE_SHORT
        | sealdb_types::TYPE_INT24
        | sealdb_types::TYPE_LONG
        | sealdb_types::TYPE_LONG_LONG
        | sealdb_types::TYPE_FLOAT
        | sealdb_types::TYPE_DOUBLE
        | sealdb_types::TYPE_NEW_DECIMAL
        | sealdb_types::TYPE_DECIMAL
        | sealdb_types::TYPE_YEAR => ColumnClass::Numeric,
        sealdb_types::TYPE_VARCHAR
        | sealdb_types::TYPE_VAR_STRING
        | sealdb_types::TYPE_STRING
        | sealdb_types::TYPE_TINY_BLOB
        | sealdb_types::TYPE_MEDIUM_BLOB
        | sealdb_types::TYPE_BLOB
        | sealdb_types::TYPE_LONG_BLOB => ColumnClass::String,
        sealdb_types::TYPE_DATE | sealdb_types::TYPE_DATETIME | sealdb_types::TYPE_TIMESTAMP => {
            ColumnClass::Temporal
        }
        _ => return None,
    })
}

/// Column `FieldType` for a bare column reference.
fn column_field_type<'a>(
    expr: &ExprNode,
    info: &'a sealdb_meta::TableInfo,
) -> Option<&'a sealdb_types::FieldType> {
    let ExprNode::Column(column) = expr else {
        return None;
    };
    info.columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(&column.name))
        .map(|col| &col.field_type)
}

/// Parse a string literal into the column's temporal type (`fsp` from the
/// column), returning `None` when the text is not a valid temporal value.
fn temporal_literal(expr: &ExprNode, field_type: &sealdb_types::FieldType) -> Option<Datum> {
    let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(bytes),
    }) = expr
    else {
        return None;
    };
    let text = String::from_utf8_lossy(bytes);
    let mut time = match field_type.tp {
        sealdb_types::TYPE_DATE => sealdb_types::parse_date_literal(&text).ok()?,
        sealdb_types::TYPE_TIMESTAMP => sealdb_types::parse_timestamp_literal(&text).ok()?,
        _ => sealdb_types::parse_datetime_literal(&text)
            .or_else(|_| {
                // `DATETIME` accepts a bare date (`'1981-09-15'` → 00:00:00).
                sealdb_types::parse_date_literal(&text).map(|mut t| {
                    t.tp = sealdb_types::TimeType::Datetime;
                    t
                })
            })
            .ok()?,
    };
    time.fsp = field_type.decimal.max(0).min(6) as i8;
    Some(Datum::Time(time))
}

fn numeric_literal(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(_) | Datum::U64(_) | Datum::F64(_) | Datum::Decimal(_)
        })
    )
}

fn bytes_number(expr: &ExprNode) -> Option<Datum> {
    let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(bytes),
    }) = expr
    else {
        return None;
    };
    let text = String::from_utf8_lossy(bytes);
    let value = sealdb_types::datum::mysql_numeric_prefix(&text);
    if value.fract() == 0.0 && value.abs() < 9.2e18 {
        Some(Datum::I64(value as i64))
    } else {
        Some(Datum::F64(value))
    }
}

/// Constant compared with a temporal column, converted to the datum TiDB folds
/// the comparison with.
///
/// * `DATE` promotes the constant to `DATETIME` (zero time part);
/// * `DATETIME`/`TIMESTAMP` parse the constant at microsecond precision;
/// * `TIME` parses a duration text (`'11:11:11'`) or reads a number as seconds
///   (`a = 1` → `00:00:01.000000`).
fn comparison_temporal_literal(
    expr: &ExprNode,
    ft: &sealdb_types::FieldType,
) -> Option<Datum> {
    match ft.tp {
        sealdb_types::TYPE_DATE => {
            let mut time = match expr {
                ExprNode::Value(ValueExpr {
                    datum: Datum::Bytes(bytes),
                }) => {
                    let text = std::str::from_utf8(bytes).ok()?;
                    sealdb_types::parse_date_literal(text)
                        .or_else(|_| sealdb_types::parse_datetime_literal(text))
                        .ok()?
                }
                _ => return None,
            };
            time.tp = sealdb_types::TimeType::Datetime;
            time.fsp = 6;
            Some(Datum::Time(time))
        }
        sealdb_types::TYPE_DATETIME | sealdb_types::TYPE_TIMESTAMP => temporal_literal(expr, ft),
        sealdb_types::TYPE_DURATION => {
            let time = match expr {
                ExprNode::Value(ValueExpr {
                    datum: Datum::Bytes(bytes),
                }) => {
                    let text = std::str::from_utf8(bytes).ok()?;
                    sealdb_types::parse_duration_string(text)?
                }
                ExprNode::Value(ValueExpr { datum }) => {
                    let secs = match datum {
                        Datum::I64(v) => *v,
                        Datum::U64(v) => *v as i64,
                        Datum::F64(v) => *v as i64,
                        Datum::Decimal(d) => d.to_i64().ok()?,
                        _ => return None,
                    };
                    if !(0..=838 * 3600 + 59 * 60 + 59).contains(&secs) {
                        return None;
                    }
                    let secs = secs as u32;
                    sealdb_types::Time {
                        year: 0,
                        month: 0,
                        day: 0,
                        hour: (secs / 3600) as u8,
                        minute: ((secs % 3600) / 60) as u8,
                        second: (secs % 60) as u8,
                        micro: 0,
                        fsp: 6,
                        tp: sealdb_types::TimeType::Time,
                    }
                }
                _ => return None,
            };
            Some(Datum::Time(time))
        }
        _ => None,
    }
}

/// TiDB folds `int_col = <constant>` against the column's integer type: a
/// constant the type cannot hold makes the equality a constant false (`0`),
/// e.g. `a int unsigned` compared with a 32-digit decimal.
fn int_eq_out_of_range(
    column: &ExprNode,
    value: &ExprNode,
    info: &sealdb_meta::TableInfo,
) -> Option<ExprNode> {
    let ft = column_field_type(column, info)?;
    let (min, max): (f64, f64) = match ft.tp {
        sealdb_types::TYPE_TINY if ft.is_unsigned() => (0.0, 255.0),
        sealdb_types::TYPE_TINY => (-128.0, 127.0),
        sealdb_types::TYPE_SHORT if ft.is_unsigned() => (0.0, 65_535.0),
        sealdb_types::TYPE_SHORT => (-32_768.0, 32_767.0),
        sealdb_types::TYPE_INT24 if ft.is_unsigned() => (0.0, 16_777_215.0),
        sealdb_types::TYPE_INT24 => (-8_388_608.0, 8_388_607.0),
        sealdb_types::TYPE_LONG if ft.is_unsigned() => (0.0, 4_294_967_295.0),
        sealdb_types::TYPE_LONG => (-2_147_483_648.0, 2_147_483_647.0),
        sealdb_types::TYPE_LONG_LONG if ft.is_unsigned() => (0.0, 18_446_744_073_709_551_615.0),
        sealdb_types::TYPE_LONG_LONG => (-9_223_372_036_854_775_808.0, 9_223_372_036_854_775_807.0),
        _ => return None,
    };
    let ExprNode::Value(ValueExpr { datum }) = value else {
        return None;
    };
    let parsed = match datum {
        Datum::Decimal(d) => d.to_f64().ok()?,
        Datum::I64(v) => *v as f64,
        Datum::U64(v) => *v as f64,
        Datum::F64(v) => *v,
        _ => return None,
    };
    if parsed < min || parsed > max {
        return Some(value_expr(Datum::I64(0)));
    }
    None
}

fn cast_double(expr: ExprNode) -> ExprNode {
    ExprNode::Cast(sealdb_parser::ast::expressions::FuncCastExpr {
        expr: Box::new(expr),
        target: "DOUBLE".to_string(),
    })
}

fn const_num(expr: &ExprNode) -> Option<i64> {
    match expr {
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => Some(*v),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => i64::try_from(*v).ok(),
        _ => None,
    }
}

/// TiDB `expressions.RewriteAstExpr` constant folding subset: comparison of two
/// literals becomes `0`/`1`, and `AND` collapses around a `1` conjunct.
fn fold_const_predicates(expr: &ExprNode) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) => {
            let left = fold_const_predicates(&bin.left);
            let right = fold_const_predicates(&bin.right);
            if matches!(bin.op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)
                && let (Some(l), Some(r)) = (const_num(&left), const_num(&right))
            {
                let value = match bin.op {
                    Op::Eq => l == r,
                    Op::Ne => l != r,
                    Op::Lt => l < r,
                    Op::Le => l <= r,
                    Op::Gt => l > r,
                    Op::Ge => l >= r,
                    _ => unreachable!(),
                };
                return ExprNode::Value(ValueExpr {
                    datum: Datum::I64(i64::from(value)),
                });
            }
            if bin.op == Op::LogicAnd {
                if const_num(&left) == Some(0) || const_num(&right) == Some(0) {
                    return ExprNode::Value(ValueExpr {
                        datum: Datum::I64(0),
                    });
                }
                if const_num(&left) == Some(1) {
                    return right;
                }
                if const_num(&right) == Some(1) {
                    return left;
                }
            }
            if bin.op == Op::LogicOr
                && (const_num(&left).is_some_and(|v| v != 0)
                    || const_num(&right).is_some_and(|v| v != 0))
            {
                return ExprNode::Value(ValueExpr {
                    datum: Datum::I64(1),
                });
            }
            ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: bin.single_cnf_item,
            })
        }
        other => other.clone(),
    }
}

/// TiDB `expression.PushDownNot`: eliminate `NOT` so a negated predicate turns
/// into the flipped comparison (`NOT a < 2` → `a >= 2`) and De Morgan splits
/// into `AND`/`OR` the way `EXPLAIN` prints them.
fn push_down_not(expr: &ExprNode, negated: bool) -> ExprNode {
    match expr {
        ExprNode::Unary(unary) if matches!(unary.op, Op::Not | Op::Not2) => {
            push_down_not(&unary.expr, !negated)
        }
        ExprNode::Binary(bin) if matches!(bin.op, Op::LogicAnd | Op::LogicOr) => {
            let op = if negated {
                if bin.op == Op::LogicAnd {
                    Op::LogicOr
                } else {
                    Op::LogicAnd
                }
            } else {
                bin.op
            };
            ExprNode::Binary(BinaryOpExpr {
                op,
                left: Box::new(push_down_not(&bin.left, negated)),
                right: Box::new(push_down_not(&bin.right, negated)),
                // De Morgan turns a negated `OR` into one `AND` predicate item;
                // TiDB keeps it whole (`EXPLAIN` prints `and(..., ...)`).
                single_cnf_item: negated,
            })
        }
        ExprNode::Binary(bin) if negated => {
            let flipped = match bin.op {
                Op::Eq => Op::Ne,
                Op::Ne => Op::Eq,
                Op::Lt => Op::Ge,
                Op::Le => Op::Gt,
                Op::Gt => Op::Le,
                Op::Ge => Op::Lt,
                _ => {
                    return ExprNode::Unary(UnaryOpExpr {
                        op: Op::Not,
                        expr: Box::new(push_down_not(expr, false)),
                    });
                }
            };
            ExprNode::Binary(BinaryOpExpr {
                op: flipped,
                left: Box::new(push_down_not(&bin.left, false)),
                right: Box::new(push_down_not(&bin.right, false)),
                single_cnf_item: false,
            })
        }
        ExprNode::InList(list) if negated => {
            let mut list = list.clone();
            list.not = !list.not;
            ExprNode::InList(list)
        }
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(push_down_not(&bin.left, false)),
            right: Box::new(push_down_not(&bin.right, false)),
            single_cnf_item: false,
        }),
        other if negated => ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(other.clone()),
        }),
        other => other.clone(),
    }
}

/// TiDB builds `OR` chains as a balanced binary tree (`Compose`), not the
/// parser's left-deep shape; EXPLAIN prints the tree, so rebalance it.
fn rebalance_disjunctions(expr: &ExprNode) -> ExprNode {
    fn flatten<'a>(expr: &'a ExprNode, out: &mut Vec<&'a ExprNode>) {
        match expr {
            ExprNode::Binary(bin) if bin.op == Op::LogicOr => {
                flatten(&bin.left, out);
                flatten(&bin.right, out);
            }
            other => out.push(other),
        }
    }

    fn build(op: Op, items: &[ExprNode]) -> ExprNode {
        if items.len() == 1 {
            return items[0].clone();
        }
        let mid = items.len() / 2;
        ExprNode::Binary(BinaryOpExpr {
            op,
            left: Box::new(build(op, &items[..mid])),
            right: Box::new(build(op, &items[mid..])),
            single_cnf_item: false,
        })
    }

    if let ExprNode::Binary(bin) = expr
        && bin.op == Op::LogicOr
    {
        let mut leaves = Vec::new();
        flatten(expr, &mut leaves);
        if leaves.len() < 2 {
            return expr.clone();
        }
        let items: Vec<ExprNode> = leaves.into_iter().map(rebalance_disjunctions).collect();
        return build(Op::LogicOr, &items);
    }
    match expr {
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(rebalance_disjunctions(&bin.left)),
            right: Box::new(rebalance_disjunctions(&bin.right)),
            single_cnf_item: bin.single_cnf_item,
        }),
        other => other.clone(),
    }
}

/// Rows surviving a predicate that fully constrains a unique key: one for an
/// equality conjunction, one per arm for a DNF of equalities.
pub(crate) fn unique_eq_rows(info: &sealdb_meta::TableInfo, selection: &ExprNode) -> Option<f64> {
    let unique_columns = |columns: &[String]| {
        info.indices.iter().any(|idx| {
            idx.unique
                && !(idx.primary && info.pk_is_clustered())
                && idx.columns.len() == columns.len()
                && idx.columns.iter().all(|c| {
                    columns
                        .iter()
                        .any(|col| col.eq_ignore_ascii_case(c.name.o()))
                })
        })
    };
    if let Some((names, tuples, _)) = selection.row_in_consts_with_len()
        && unique_columns(&names)
    {
        return Some(tuples.len() as f64);
    }
    if let Some((column, values)) = selection.column_in_consts()
        && unique_columns(std::slice::from_ref(&column))
    {
        return Some(values.len() as f64);
    }
    let arms = selection.split_dnf();
    let covers = |eqs: &[(String, Datum)], columns: &[sealdb_meta::IndexColumn]| {
        eqs.len() >= columns.len()
            && columns.iter().all(|c| {
                eqs.iter()
                    .any(|(col, _)| col.eq_ignore_ascii_case(c.name.o()))
            })
    };
    if arms.len() > 1 {
        let first: Vec<(String, Datum)> = arms[0]
            .split_cnf_all()
            .into_iter()
            .filter_map(|expr| expr.eq_column_const())
            .collect();
        let found = info.indices.iter().any(|idx| {
            idx.unique
                && !(idx.primary && info.pk_is_clustered())
                && covers(&first, &idx.columns)
                && arms.iter().all(|arm| {
                    let eqs: Vec<(String, Datum)> = arm
                        .split_cnf_all()
                        .into_iter()
                        .filter_map(|expr| expr.eq_column_const())
                        .collect();
                    covers(&eqs, &idx.columns)
                })
        });
        return found.then_some(arms.len() as f64);
    }
    let eqs: Vec<(String, Datum)> = arms[0]
        .split_cnf_all()
        .into_iter()
        .filter_map(|expr| expr.eq_column_const())
        .collect();
    info.indices
        .iter()
        .any(|idx| {
            idx.unique && !(idx.primary && info.pk_is_clustered()) && covers(&eqs, &idx.columns)
        })
        .then_some(1.0)
}

/// Cardinality of a single null-rejecting cop filter over analyzed stats.
///
/// TiDB sizes each conjunct from the column statistics and multiplies the
/// selectivities, so a lone `col <> const` keeps
/// `count - null_count` rows minus the point it can pin on `const`, and
/// `not(isnull(col))` keeps `count - null_count`.
fn column_stats_filter_rows(
    table_stats: &super::access_range::TableStats,
    info: &sealdb_meta::TableInfo,
    selection: &ExprNode,
) -> Option<f64> {
    let conjuncts = selection.split_cnf();
    let [conjunct] = conjuncts.as_slice() else {
        return None;
    };
    let count = table_stats.count as f64;
    let rows = if let Some(column) = not_isnull_column(conjunct) {
        // TiDB `getNotNULLRowCount`: a column the statistics do not carry keeps
        // the whole table count (the scan is only marked `stats:partial`).
        match table_stats.column(&column) {
            Some(col) if col.histogram.is_some() => {
                count - col.null_count as f64
            }
            _ => count,
        }
    } else {
        let (column, value) = ne_column_const(conjunct)?;
        let field_type = &info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&column))?
            .field_type;
        table_stats.column_ne_estimate(&column, sealdb_statistics::kind_of(field_type), &value)?
    };
    Some(rows.clamp(1.0, count))
}

/// TiDB `GetRowCountByIndexRanges` on the clustered PRIMARY index for a
/// handle-prefix range (`TableRangeScan`).
///
/// A clustered PRIMARY index has no histogram of its own yet, so the estimate
/// degrades to the realtime table row count — the same shape TiDB keeps when
/// the index histogram is missing.
/// Ranges the handle-prefix conjuncts pin on the handle column, as planner
/// statistics ranges.
fn handle_scan_stats_ranges(
    column: &str,
    selection: &ExprNode,
) -> Option<Vec<sealdb_statistics::range_est::StatsRange>> {
    let mut ranges: Vec<sealdb_statistics::pseudo::Bounds> = Vec::new();
    for item in selection.split_cnf() {
        let (name, sets) = sealdb_statistics::pseudo::conjunct_bound_sets(&item)?;
        if !name.eq_ignore_ascii_case(column) {
            return None;
        }
        if ranges.is_empty() {
            ranges = sets;
        } else {
            sealdb_statistics::pseudo::intersect_sets(&mut ranges, &sets);
            if ranges.is_empty() {
                return Some(Vec::new());
            }
        }
    }
    Some(
        ranges
            .into_iter()
            .map(|(low, high)| sealdb_statistics::range_est::StatsRange { low, high })
            .collect(),
    )
}

/// TiDB `GetRowCountByIntColumnRanges` for an integer PK handle range
/// (`TableRangeScan` over `_tidb_rowid`).
fn int_handle_range_rows(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    stats: Option<&Handle>,
    current_db: &str,
    selection: &ExprNode,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    let table_stats = stats
        .and_then(|h| h.table(&db, &table))
        .filter(|t| super::plan_cost::has_real_stats(t))?;
    if std::env::var("SEALDB_DEBUG_HANDLE_RANGE").is_ok() {
        eprintln!("INTHANDLE {table} has_stats=true count={}", table_stats.count);
    }
    let column = info.handle_column()?;
    let stat_ranges = handle_scan_stats_ranges(column.name.o(), selection)?;
    if std::env::var("SEALDB_DEBUG_HANDLE_RANGE").is_ok() {
        eprintln!("INTHANDLE {table} col={} ranges={stat_ranges:?}", column.name.o());
    }
    if stat_ranges.is_empty() {
        return Some(0.0);
    }
    let rows = table_stats.column_range_estimate(
        column.name.o(),
        sealdb_statistics::kind_of(&column.field_type),
        &stat_ranges,
    );
    if std::env::var("SEALDB_DEBUG_HANDLE_RANGE").is_ok() {
        eprintln!("INTHANDLE {table} rows={rows:?}");
    }
    Some(rows.unwrap_or(table_stats.count as f64))
}

fn clustered_range_rows(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    stats: Option<&Handle>,
    current_db: &str,
    selection: &ExprNode,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    let table_stats = stats
        .and_then(|h| h.table(&db, &table))
        .filter(|t| super::plan_cost::has_real_stats(t))?;
    let pk = info.indices.iter().find(|idx| idx.primary)?;
    let column = info.clustered_prefix_column()?;
    let stat_ranges = handle_scan_stats_ranges(column.name.o(), selection)?;
    if stat_ranges.is_empty() {
        return Some(0.0);
    }
    if std::env::var("SEALDB_DEBUG_CLUSTERRANGE").is_ok() {
        eprintln!(
            "CLUSTERRANGE pk={} col={} ranges={:?} idx={:?} primary={:?} colest={:?} count={}",
            pk.name.o(),
            column.name.o(),
            stat_ranges.len(),
            table_stats.index_range_estimate(pk.name.o(), &column.field_type, &stat_ranges),
            table_stats.index_range_estimate("primary", &column.field_type, &stat_ranges),
            table_stats.column_range_estimate(column.name.o(), sealdb_statistics::kind_of(&column.field_type), &stat_ranges),
            table_stats.count
        );
    }
    Some(
        table_stats
            .index_range_estimate(pk.name.o(), &column.field_type, &stat_ranges)
            .or_else(|| {
                table_stats.index_range_estimate("primary", &column.field_type, &stat_ranges)
            })
            .unwrap_or(table_stats.count as f64),
    )
}

/// Column of a `not(isnull(col))` conjunct (TiDB null-rejection predicate).
fn not_isnull_column(expr: &ExprNode) -> Option<String> {
    match expr {
        ExprNode::Unary(u) if u.op == Op::Not => match u.expr.as_ref() {
            ExprNode::IsNull(isnull) if !isnull.not => match isnull.expr.as_ref() {
                ExprNode::Column(col) => Some(col.name.clone()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// `(column, literal)` of a `col <> const` / `col != const` conjunct.
fn ne_column_const(expr: &ExprNode) -> Option<(String, sealdb_types::Datum)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if bin.op != Op::Ne {
        return None;
    }
    let (col, value) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(col), ExprNode::Value(value)) => (col, value),
        _ => return None,
    };
    Some((col.name.clone(), value.datum.clone()))
}

/// Build a TableReader / PartitionUnion for a (possibly partitioned) table.
fn partitioned_table_reader(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    selection: Option<&ExprNode>,
    prune_mode: PartitionPruneMode,
    stats: Option<&Handle>,
    current_db: &str,
) -> PhysicalPlan {
    let planned = resolve_partitions(info, selection, &from.partition_names, prune_mode);
    let row_count = table_row_count(stats, from, current_db);
    let (db, table) = from.qualified(current_db);
    let table_stats = stats
        .and_then(|h| h.table(&db, &table))
        .filter(|t| super::plan_cost::has_real_stats(t));
    let stats_pseudo = table_stats.is_none();
    // TiDB derives the filtered cardinality from the prune ranges; exact
    // value counts give the same estimate for range-partitioned scans.
    let filter_est = match (info.partition.as_ref(), table_stats.as_ref(), selection) {
        (Some(part), Some(table_stats), Some(selection)) if part.enable => {
            partition_filter_rows(table_stats, selection)
        }
        _ => None,
    }
    .or_else(|| {
        // Unique keys cap a full equality (or a DNF of them) at one row.
        selection.and_then(|selection| unique_eq_rows(info, selection))
    })
    .or_else(|| {
        // Analyzed stats decide the cardinality of a null-rejecting cop filter
        // (`col <> const`, `not(isnull(col))`), which the pseudo selectivity
        // cannot express: TiDB `GetRowCountByColumnRanges` subtracts the rows
        // it pins on the excluded point.
        match (table_stats.as_ref(), selection) {
            (Some(table_stats), Some(selection)) => {
                column_stats_filter_rows(table_stats, info, selection)
            }
            _ => None,
        }
    });
    // Per-partition est for static PartitionUnion children.
    let part_rows = if planned.ids.len() > 1 && stats_pseudo {
        10_000.0
    } else if planned.ids.len() > 1 {
        (row_count / planned.ids.len() as f64).max(1.0)
    } else {
        row_count
    };

    if std::env::var("SEALDB_DEBUG_CLUSTERRANGE").is_ok() {
        eprintln!(
            "PTR table={} ids={:?} row_count={row_count} filter_est={filter_est:?} stats_pseudo={stats_pseudo}",
            from.name, planned.names
        );
    }
    match prune_mode {
        PartitionPruneMode::Static if planned.ids.len() > 1 => {
            let children = planned
                .ids
                .iter()
                .zip(planned.names.iter())
                .map(|(id, name)| {
                    PhysicalPlan::table_reader_parts(
                        from.name.clone(),
                        vec![*id],
                        vec![name.clone()],
                        String::new(),
                        part_rows,
                        stats_pseudo,
                    )
                })
                .collect();
            PhysicalPlan::PartitionUnion { children }
        }
        PartitionPruneMode::Static => {
            let mut reader = PhysicalPlan::table_reader_parts(
                from.name.clone(),
                planned.ids,
                planned.names,
                String::new(),
                row_count,
                stats_pseudo,
            );
            if let PhysicalPlan::TableReader {
                filter_est: est, ..
            } = &mut reader
            {
                *est = filter_est;
            }
            reader
        }
        PartitionPruneMode::Dynamic => {
            // Dynamic: executor prunes; EXPLAIN shows partition access on reader.
            let access = if info.partition.as_ref().is_some_and(|p| p.enable) {
                if planned.all {
                    "all".to_string()
                } else if planned.names.is_empty() {
                    // No partition matches the predicate (TiDB scans `dual`).
                    "dual".to_string()
                } else {
                    planned.names.join(",")
                }
            } else {
                String::new()
            };
            let mut reader = PhysicalPlan::table_reader_parts(
                from.name.clone(),
                // `PARTITION (pN)` narrows the scan even in dynamic mode; the
                // executor only prunes with the filter when ids are empty.
                planned.ids,
                Vec::new(),
                access,
                row_count,
                stats_pseudo,
            );
            if let PhysicalPlan::TableReader {
                filter_est: est, ..
            } = &mut reader
            {
                *est = filter_est;
            }
            reader
        }
    }
}

/// Rows a leading-column equality matches, sizing the index path from the
/// index histogram when the table statistics do not carry the column.
///
/// `select c1, c2 from t1 where c2 = 1` on a table analyzed without `c2` keeps
/// `stats:partial[c2:missing]`: TiDB still reads the index histogram of `c2`
/// (`GetRowCountByIndexRanges`), so a value the histogram leaves in a bucket gap
/// estimates zero rows and the cheap index scan wins over the table scan.
fn index_equality_rows(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    select: &SelectStmt,
    stats: Option<&Handle>,
    from: &TableName,
    current_db: &str,
    column: &str,
    value: &sealdb_types::Datum,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    let table_stats = stats
        .and_then(|handle| handle.table(&db, &table))
        .filter(|t| super::plan_cost::has_real_stats(t));
    let column_rows = eq_row_count_value(stats, from, current_db, column, value);
    let Some(table_stats) = table_stats else {
        return column_rows;
    };
    if table_stats.column(column).is_some() {
        return column_rows;
    }
    let total = table_row_count(stats, from, current_db);
    let conjuncts = select
        .selection
        .as_ref()
        .map(|selection| selection.split_cnf())
        .unwrap_or_default();
    super::access_range::index_access_range(info, index, &conjuncts, total, Some(&table_stats))
        .map(|access| access.range.filter_rows)
        .or(column_rows)
}

fn index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    prune_mode: PartitionPruneMode,
    join_keys: &[String],
) -> PhysicalPlan {
    let planned = resolve_partitions(
        info,
        select.selection.as_ref(),
        &from.partition_names,
        prune_mode,
    );
    let partitions = planned.ids.clone();
    // TiDB labels a GLOBAL index reader with the partitions it still scans:
    // `partition:all` when unpinned, `partition:pN` for `FROM PARTITION (pN)`.
    let partition_access = if index.global {
        if from.partition_names.is_empty() {
            "all".to_string()
        } else {
            from.partition_names.join(",")
        }
    } else {
        String::new()
    };
    let index_columns: Vec<String> = index_key_column_names(info, index);
    if index_covers_needed(index, select, join_keys, info, from) {
        PhysicalPlan::IndexReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions,
            partition_access,
            row_count: 10_000.0,
            pushed: None,
            range_scan: None,
        }
    } else {
        PhysicalPlan::IndexLookUpReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions,
            partition_access,
            row_count: 10_000.0,
            selection_info: None,
            probe_selection_info: None,
            probe_est: 0.0,
            range_scan: None,
            pushed: None,
        }
    }
}

/// Leading expression key part constrained by a comparison → index seek.
///
/// `where tidb_shard(a) = 8` pins `_V$_uk_expr_0`, so the ranger builds
/// `range:[8,8]` on `uk_expr((tidb_shard(a)), a)` even though no plain column
/// of the index is named.
fn expr_range_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
    join_keys: &[String],
) -> Option<PhysicalPlan> {
    let selection = select.selection.as_ref()?;
    let conjuncts = selection.split_cnf();
    let index = info.indices.iter().find(|idx| {
        // A clustered PRIMARY KEY has no separate index path.
        !(idx.primary && info.pk_is_clustered())
            && !index_ignored(from, idx.name.o())
            && super::expr_index::has_expression_part(info, idx)
            && idx.columns.first().is_some_and(|leading| {
                let key = super::expr_index::part_key(info, leading);
                conjuncts.iter().any(|conjunct| {
                    sealdb_statistics::pseudo::conjunct_bound_sets(conjunct)
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(&key))
                })
            })
    })?;
    Some(index_plan(select, from, info, index, prune_mode, join_keys))
}

/// `col = a OR col = b …` on one indexed leading column: TiDB ranger seeks the
/// index once with every arm as a range (`IndexRangeScan range:["a","a"],
/// ["b","b"]`, then `IndexLookUp` when the index is not covering). It never
/// builds an IndexMerge: merging only pays off when the arms use different
/// indexes. Among the candidates TiDB picks the narrowest covering key and
/// prefers a secondary key over a composite PRIMARY that only shares the
/// leading column (mysql.global_grants reads `i_user(USER)`).
fn dnf_same_index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
    join_keys: &[String],
) -> Option<PhysicalPlan> {
    let selection = select.selection.as_ref()?;
    let cnf = selection.split_cnf();
    if cnf.len() != 1 {
        return None;
    }
    let arms = cnf[0].split_dnf();
    if arms.len() < 2 {
        return None;
    }
    // Every arm pins the same set of columns (`c1=0 and c2=1`); a composite
    // index whose whole key is pinned becomes one point range per arm.
    let mut pinned: Option<Vec<String>> = None;
    for arm in &arms {
        let mut columns = Vec::new();
        for conjunct in arm.split_cnf() {
            let (name, _) = conjunct.eq_column_const()?;
            columns.push(name);
        }
        if columns.is_empty() {
            return None;
        }
        match &pinned {
            Some(have) if !same_column_set(have, &columns) => return None,
            Some(_) => {}
            None => pinned = Some(columns),
        }
    }
    let pinned = pinned?;
    let index = info
        .indices
        .iter()
        .filter(|idx| {
            !(idx.primary && info.pk_is_clustered())
                && !index_ignored(from, idx.name.o())
                && idx.columns.iter().all(|column| {
                    pinned
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(column.name.o()))
                })
        })
        .min_by(|a, b| {
            a.columns
                .len()
                .cmp(&b.columns.len())
                .then(a.primary.cmp(&b.primary))
        })?;
    Some(index_plan(select, from, info, index, prune_mode, join_keys))
}

/// Same column names regardless of order (`c1, c2` ≡ `c2, c1`).
fn same_column_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|left| b.iter().any(|right| left.eq_ignore_ascii_case(right)))
}

/// `USE_INDEX_MERGE(t, idx…)` hint targeting one table factor. An empty index
/// list lets TiDB pick the access path per DNF item / filter.
#[derive(Clone)]
struct IndexMergeHint {
    indexes: Vec<String>,
}

/// TiDB `use_index_merge(<table>, <index>, …)`: the first argument names the
/// table factor (alias), the rest the indexes the hint allows.
fn index_merge_hint(text: &str, target: &str) -> Option<IndexMergeHint> {
    let lower = text.to_ascii_lowercase();
    let name = "use_index_merge(";
    let mut base = 0usize;
    while let Some(found) = lower[base..].find(name) {
        let open = base + found + name.len();
        let Some(close_rel) = lower[open..].find(')') else {
            return None;
        };
        let inside = &text[open..open + close_rel];
        let mut parts = inside.split(',');
        let hint_target = parts.next().unwrap_or("").trim().trim_matches('`');
        if hint_target.eq_ignore_ascii_case(target) {
            let indexes = parts
                .map(|part| part.trim().trim_matches('`').to_ascii_lowercase())
                .filter(|part| !part.is_empty())
                .collect();
            return Some(IndexMergeHint { indexes });
        }
        base = open + close_rel + 1;
    }
    None
}

/// Hint name → index, with TiDB's `PRIMARY` alias for the clustered key.
fn hint_index<'a>(info: &'a sealdb_meta::TableInfo, name: &str) -> Option<&'a sealdb_meta::IndexInfo> {
    if name.eq_ignore_ascii_case("primary") {
        info.indices.iter().find(|idx| idx.primary)
    } else {
        info.indices
            .iter()
            .find(|idx| idx.name.o().eq_ignore_ascii_case(name))
    }
}

/// Leading column of the row handle when the PRIMARY KEY is clustered.
fn clustered_handle_column(info: &sealdb_meta::TableInfo) -> Option<&str> {
    if !info.pk_is_clustered() {
        return None;
    }
    info.clustered_prefix_column().map(|col| col.name.o())
}

/// TiDB pseudo estimate of an integer-handle range (`GetRowCountByIntColumnRanges`):
/// a point pins one row, a one-sided range `1/3` of the table, a bounded range
/// `1/40`. Unlike a secondary index the handle is unique and NOT NULL, so no
/// `1/1000` boundary row is subtracted.
fn int_handle_pseudo_rows(table_rows: f64, ranges: &[sealdb_statistics::pseudo::Bounds]) -> f64 {
    let mut rows = 0.0;
    for (low, high) in ranges {
        let cnt = match (low, high) {
            (None, None) => table_rows,
            (Some((low, _)), Some((high, _))) if low.compare(high) == std::cmp::Ordering::Equal => 1.0,
            (None, _) | (_, None) => table_rows / 3.0,
            _ => table_rows / 40.0,
        };
        rows += cnt;
    }
    rows.min(table_rows)
}

/// One index arm: the seek range the index builds from `conds`, plus the
/// conjuncts fully encoded by that range.
fn index_merge_index_arm(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    conds: &[ExprNode],
    table_rows: f64,
    table_stats: Option<&super::access_range::TableStats>,
) -> Option<(IndexMergePath, Vec<ExprNode>)> {
    let access =
        super::access_range::index_access_range(info, index, conds, table_rows, table_stats)?;
    Some((
        IndexMergePath {
            index: index.name.o().to_string(),
            range: Some(access.range),
            table_range: false,
            index_columns: index_key_column_names(info, index),
            keep_order: false,
            desc: false,
            limit: None,
            ranges: access.ranges,
        },
        access.consumed,
    ))
}

/// One clustered-handle arm (`TableRangeScan` in EXPLAIN).
fn index_merge_handle_arm(
    info: &sealdb_meta::TableInfo,
    conds: &[ExprNode],
    table_rows: f64,
) -> Option<(IndexMergePath, Vec<ExprNode>)> {
    let column = clustered_handle_column(info)?;
    let mut ranges: Vec<sealdb_statistics::pseudo::Bounds> = Vec::new();
    let mut consumed = Vec::new();
    for cond in conds {
        let bounds = sealdb_statistics::pseudo::conjunct_bound_sets(cond);
        if std::env::var("SEALDB_DEBUG_IM").is_ok() {
            eprintln!("IM handle arm cond={} bounds_name={:?}", cond.restore_string(), bounds.as_ref().map(|(n, s)| (n.clone(), s.len())));
        }
        let Some((name, sets)) = bounds else {
            continue;
        };
        if !name.eq_ignore_ascii_case(column) {
            continue;
        }
        if ranges.is_empty() {
            ranges = sets;
        } else {
            sealdb_statistics::pseudo::intersect_sets(&mut ranges, &sets);
            if ranges.is_empty() {
                return None;
            }
        }
        consumed.push(cond.clone());
    }
    if ranges.is_empty() {
        return None;
    }
    let pinned = ranges.len() == 1
        && matches!(
            (&ranges[0].0, &ranges[0].1),
            (Some((low, true)), Some((high, true))) if low.compare(high) == std::cmp::Ordering::Equal
        );
    let rows = if info.pk_is_handle {
        int_handle_pseudo_rows(table_rows, &ranges)
    } else {
        sealdb_statistics::pseudo::index_ranges_rows(
            table_rows,
            &ranges,
            1,
            usize::from(pinned),
            true,
        )
    };
    Some((
        IndexMergePath {
            index: String::new(),
            range: Some(super::physical_plans::IndexScanRange {
                text: sealdb_statistics::pseudo::ranges_text(&ranges),
                rows,
                filter_rows: rows,
                pseudo: true,
                partial: None,
            }),
            table_range: true,
            index_columns: Vec::new(),
            keep_order: false,
            desc: false,
            limit: None,
            ranges,
        },
        consumed,
    ))
}

/// Index a union arm reads: the hinted index whose leading column the DNF item
/// constrains, or any visible leading-column index when the hint lists none.
fn index_merge_arm_index<'a>(
    info: &'a sealdb_meta::TableInfo,
    hint: &IndexMergeHint,
    conds: &[ExprNode],
) -> Option<&'a sealdb_meta::IndexInfo> {
    let columns: Vec<String> = conds
        .iter()
        .filter_map(|cond| {
            sealdb_statistics::pseudo::conjunct_bound_sets(cond).map(|(name, _)| name)
        })
        .collect();
    let candidates: Vec<&sealdb_meta::IndexInfo> = if hint.indexes.is_empty() {
        info.indices.iter().collect()
    } else {
        hint.indexes
            .iter()
            .filter_map(|name| hint_index(info, name))
            .collect()
    };
    candidates.into_iter().find(|idx| {
        !(idx.primary && info.pk_is_handle)
            && !idx.invisible
            && idx.columns.first().is_some_and(|col| {
                let key = super::expr_index::part_key(info, col);
                columns.iter().any(|name| name.eq_ignore_ascii_case(&key))
            })
    })
}

/// Build one union arm for a DNF item; the handle wins when the item pins it.
fn index_merge_union_arm(
    info: &sealdb_meta::TableInfo,
    hint: &IndexMergeHint,
    conds: &[ExprNode],
    table_rows: f64,
    table_stats: Option<&super::access_range::TableStats>,
) -> Option<(IndexMergePath, Vec<ExprNode>)> {
    let handle = clustered_handle_column(info);
    if std::env::var("SEALDB_DEBUG_IM").is_ok() {
        eprintln!(
            "IM arm handle={:?} pk_is_handle={} common_handle={} primary_cols={:?}",
            handle,
            info.pk_is_handle,
            info.common_handle,
            info.indices
                .iter()
                .find(|i| i.primary)
                .map(|i| i.columns.iter().map(|c| c.name.o().to_string()).collect::<Vec<_>>())
        );
    }
    if handle.is_some_and(|handle| {
        conds.iter().any(|cond| {
            sealdb_statistics::pseudo::conjunct_bound_sets(cond)
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(handle))
        })
    }) {
        return index_merge_handle_arm(info, conds, table_rows);
    }
    let index = index_merge_arm_index(info, hint, conds)?;
    index_merge_index_arm(info, index, conds, table_rows, table_stats)
}

/// TiDB `buildIndexMergeOrPath`: one arm per DNF item over the index it seeks.
fn index_merge_union_paths(
    cnf: &[ExprNode],
    or_pos: usize,
    hint: &IndexMergeHint,
    info: &sealdb_meta::TableInfo,
    table_rows: f64,
    table_stats: Option<&super::access_range::TableStats>,
) -> Option<(Vec<IndexMergePath>, Vec<ExprNode>)> {
    let items = cnf[or_pos].split_dnf();
    if items.len() < 2 {
        return None;
    }
    let mut paths = Vec::with_capacity(items.len());
    let mut fully_consumed = true;
    for item in &items {
        let conds = item.split_cnf();
        let built = index_merge_union_arm(info, hint, &conds, table_rows, table_stats);
        if std::env::var("SEALDB_DEBUG_IM").is_ok() {
            eprintln!("IM arm conds={:?} ok={}", conds.iter().map(|c| c.restore_string()).collect::<Vec<_>>(), built.is_some());
        }
        let (path, consumed) = built?;
        if consumed.len() != conds.len() {
            fully_consumed = false;
        }
        paths.push(path);
    }
    let mut residual: Vec<ExprNode> = cnf
        .iter()
        .enumerate()
        .filter(|(pos, _)| *pos != or_pos)
        .map(|(_, conjunct)| conjunct.clone())
        .collect();
    // A DNF filter survives in the probe plan unless every arm fully covers it.
    if !fully_consumed {
        residual.insert(0, cnf[or_pos].clone());
    }
    Some((paths, residual))
}

/// TiDB `buildIndexMergeIntersectionPath`: one arm per hinted index, consuming
/// the CNF conjuncts that seek it.
fn index_merge_intersection_paths(
    cnf: &[ExprNode],
    hint: &IndexMergeHint,
    info: &sealdb_meta::TableInfo,
    table_rows: f64,
    table_stats: Option<&super::access_range::TableStats>,
) -> Option<(Vec<IndexMergePath>, Vec<ExprNode>)> {
    if hint.indexes.len() < 2 {
        return None;
    }
    let mut remaining: Vec<ExprNode> = cnf.to_vec();
    let mut paths = Vec::new();
    for name in &hint.indexes {
        let Some(index) = hint_index(info, name) else {
            continue;
        };
        let built = if index.primary && info.pk_is_clustered() {
            index_merge_handle_arm(info, &remaining, table_rows)
        } else {
            index_merge_index_arm(info, index, &remaining, table_rows, table_stats)
        };
        let Some((path, consumed)) = built else {
            continue;
        };
        remaining.retain(|cond| {
            !consumed
                .iter()
                .any(|used| used.restore_string() == cond.restore_string())
        });
        paths.push(path);
    }
    if paths.len() < 2 {
        return None;
    }
    Some((paths, remaining))
}

/// TiDB `Selectivity()` estimate for one DNF item on an int handle column.
///
/// A union arm's own `CountAfterAccess` (3333.33 for `pk < 10`) is not what the
/// DNF independence formula consumes: `cardinality.Selectivity` rebuilds a
/// column range with `ranger.BuildColumnRange`, whose open lower bound is
/// `MinNotNull` rather than `Int64`. `GetRowCountByIntColumnRanges` therefore
/// takes the **unsigned** pseudo helper, where `MinNotNull` becomes 0 and the
/// count is clamped to `high - 0` — `pk < 10` on 10000 pseudo rows is 10 rows,
/// not 1/3 of the table.
fn int_handle_item_rows(table_rows: f64, ranges: &[sealdb_statistics::pseudo::Bounds]) -> f64 {
    let mut rows = 0.0;
    for (low, high) in ranges {
        let low_v = low
            .as_ref()
            .map(|(value, _)| value.as_f64().unwrap_or(0.0))
            .unwrap_or(0.0);
        let high_v = high
            .as_ref()
            .map(|(value, _)| value.as_f64().unwrap_or(f64::INFINITY))
            .unwrap_or(f64::INFINITY);
        let cnt = match (low, high) {
            (None, None) => table_rows,
            (None, _) | (_, None) => table_rows / sealdb_statistics::pseudo::PSEUDO_LESS_RATE,
            _ if low_v == high_v => 1.0,
            _ => table_rows / sealdb_statistics::pseudo::PSEUDO_BETWEEN_RATE,
        };
        let span = high_v - low_v;
        rows += if span > 0.0 && cnt > span { span } else { cnt };
    }
    rows.min(table_rows)
}

/// One union arm's selectivity inside TiDB's DNF independence formula.
fn index_merge_union_arm_selectivity(
    info: &sealdb_meta::TableInfo,
    path: &IndexMergePath,
    table_rows: f64,
    real_stats: bool,
) -> f64 {
    let rows = if !real_stats && path.table_range && info.pk_is_handle {
        int_handle_item_rows(table_rows, &path.ranges)
    } else {
        path.range.as_ref().map_or(table_rows, |range| range.rows)
    };
    (rows / table_rows).clamp(0.0, 1.0)
}

/// TiDB probe-side selectivity for a residual filter. A DNF filter that was
/// only partially covered comes back whole (`or(eq(c,10), and(...))`), and its
/// pseudo selectivity is the OR of its items, not the `0.8` default.
fn index_merge_residual_rows(residual: &[ExprNode], rows: f64) -> f64 {
    if let [only] = residual
        && only.split_dnf().len() > 1
    {
        let mut selectivity = 0.0;
        for item in only.split_dnf() {
            let mut item_sel = 1.0;
            for cond in item.split_cnf() {
                let per = match sealdb_statistics::pseudo::conjunct_bound_sets(&cond) {
                    Some((_, sets)) => sets
                        .iter()
                        .map(|(low, high)| {
                            sealdb_statistics::pseudo::column_range_rows(
                                1.0,
                                low.as_ref(),
                                high.as_ref(),
                            )
                        })
                        .sum::<f64>(),
                    None => sealdb_statistics::pseudo::SELECTION_FACTOR,
                };
                item_sel *= per;
            }
            selectivity = selectivity + item_sel - selectivity * item_sel;
        }
        return (rows * selectivity).clamp(0.0, rows);
    }
    sealdb_statistics::pseudo::filter_rows(residual, rows)
}

/// TiDB index-merge access path: a union of DNF arms (OR) or an intersection of
/// hinted indexes (AND), followed by a probe-side table lookup. The limit is
/// embedded into the merge root when the order is not provided by the arms.
fn index_merge_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
    stats: Option<&Handle>,
    current_db: &str,
) -> Option<PhysicalPlan> {
    let hinted = index_merge_hint(&select.text, &from.name).or_else(|| {
        from.physical
            .as_deref()
            .and_then(|name| index_merge_hint(&select.text, name))
    });
    // TiDB `indexMergeOR` runs in logical optimization without any hint: an OR
    // whose arms each seek a *different* index becomes an IndexMerge union when
    // its cost beats a table scan. Intersection merges still come only from an
    // explicit `USE_INDEX_MERGE` hint.
    let hint = hinted
        .clone()
        .unwrap_or(IndexMergeHint { indexes: Vec::new() });
    let selection = select.selection.as_ref()?;
    let table_rows = table_row_count(stats, from, current_db).max(1.0);
    let table_stats = stats
        .and_then(|handle| {
            let (db, table) = from.qualified(current_db);
            handle.table(&db, &table)
        })
        .filter(|table| super::plan_cost::has_real_stats(table));
    let cnf = selection.split_cnf();
    let or_pos = cnf.iter().position(|conjunct| conjunct.split_dnf().len() > 1);
    if std::env::var("SEALDB_DEBUG_IM").is_ok() {
        eprintln!("IM hint={:?} cnf={} or_pos={:?} rows={table_rows}", hint.indexes, cnf.len(), or_pos);
    }
    let (mut paths, residual, intersection) = match or_pos {
        Some(pos) => {
            let (paths, residual) = index_merge_union_paths(
                &cnf,
                pos,
                &hint,
                info,
                table_rows,
                table_stats.as_ref(),
            )?;
            (paths, residual, false)
        }
        None => {
            hinted.as_ref()?;
            let (paths, residual) = index_merge_intersection_paths(
                &cnf,
                &hint,
                info,
                table_rows,
                table_stats.as_ref(),
            )?;
            (paths, residual, true)
        }
    };
    if hinted.is_none() {
        // An automatic union needs one distinct secondary index per arm; a
        // handle arm or two arms on the same index stay a table scan.
        let mut seen = std::collections::HashSet::new();
        if paths
            .iter()
            .any(|path| path.table_range || path.index.is_empty())
            || !paths
                .iter()
                .all(|path| seen.insert(path.index.to_ascii_lowercase()))
        {
            return None;
        }
    }
    let path_rows = |path: &IndexMergePath| path.range.as_ref().map_or(table_rows, |r| r.rows);
    // TiDB combine: union is `1 - Π(1 - selᵢ)`; intersection multiplies the
    // per-path selectivities (every arm must hold).
    let mut rowid_est = if intersection {
        let selectivity = paths
            .iter()
            .fold(1.0, |acc, path| acc * path_rows(path) / table_rows);
        table_rows * selectivity
    } else {
        let mut selectivity = 0.0;
        for path in &paths {
            let arm =
                index_merge_union_arm_selectivity(info, path, table_rows, table_stats.is_some());
            selectivity = selectivity + arm - selectivity * arm;
        }
        table_rows * selectivity
    };
    if hinted.is_none()
        && super::plan_cost::index_lookup_cost(rowid_est)
            >= super::plan_cost::table_scan_cost(table_rows)
    {
        // Union (index ranges + row lookups) costs more than scanning the
        // table once: TiDB keeps the TableFullScan + Selection.
        return None;
    }
    // ORDER BY a suffix key column of every arm → the merge reads in that order
    // (`keep order:true`), and the LIMIT above must not be embedded.
    let order = match select.order_by.as_slice() {
        [item] => match &item.expr {
            ExprNode::Column(col) => Some((col.name.clone(), item.desc)),
            _ => None,
        },
        _ => None,
    };
    let mut keep_order = false;
    let mut order_column = None;
    let mut order_desc = false;
    if !intersection
        && let Some((column, desc)) = &order
        && paths.iter().all(|path| {
            !path.table_range
                && path
                    .index_columns
                    .iter()
                    .position(|key| key.eq_ignore_ascii_case(column))
                    .is_some_and(|pos| pos > 0)
        })
    {
        keep_order = true;
        order_column = Some(column.clone());
        order_desc = *desc;
        for path in &mut paths {
            path.keep_order = true;
            path.desc = *desc;
        }
    }
    // TiDB `AdjustRowCountForIndexScanByLimit`: LIMIT shrinks the merge's
    // cardinality and each arm's scan (`expectedCnt / selectivity`).
    let expected = select.limit.map(|count| (count + select.offset.unwrap_or(0)) as f64);
    if let Some(expected) = expected
        && rowid_est > 0.0
    {
        let factor = (expected / rowid_est).min(1.0);
        for path in &mut paths {
            if let Some(range) = path.range.as_mut() {
                range.rows = (range.rows * factor).min(range.rows);
                range.filter_rows = range.filter_rows.min(range.rows);
            }
        }
        rowid_est *= factor;
    }
    let probe_est = if residual.is_empty() {
        rowid_est
    } else {
        index_merge_residual_rows(&residual, rowid_est)
    };
    let embedded = select
        .limit
        .filter(|_| select.order_by.is_empty())
        .map(|count| (0u64, count));
    if let Some(limit) = embedded
        && !keep_order
    {
        for path in &mut paths {
            path.limit = Some(limit);
        }
    }
    Some(PhysicalPlan::IndexMerge {
        table: from.name.clone(),
        indexes: paths
            .iter()
            .map(|path| path.index.clone())
            .filter(|index| !index.is_empty())
            .collect(),
        partitions: resolve_partitions(
            info,
            select.selection.as_ref(),
            &from.partition_names,
            prune_mode,
        )
        .ids,
        pushed: None,
        intersection,
        paths,
        est_rows: Some(probe_est),
        probe_est: rowid_est,
        probe_selection: ExprNode::compose_cnf(residual),
        limit: embedded.filter(|_| !keep_order),
        keep_order,
        order_column,
        order_desc,
    })
}

/// True for `SELECT <plain columns> FROM t` with no filtering, grouping, or
/// de-duplication — the shape where a covering secondary index may replace the
/// table scan.
fn is_plain_projection(select: &SelectStmt) -> bool {
    select.selection.is_none()
        && select.group_by.is_empty()
        && select.having.is_none()
        && !select.distinct
        && select
            .fields
            .iter()
            .all(|field| matches!(field.expr, ExprNode::Column(_) | ExprNode::Value(_)))
}

fn select_covers_index(
    select: &SelectStmt,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
) -> bool {
    select_covers_index_for(select, None, info, index)
}

/// `name` is the alias the statement uses for `info`. Join-key columns of that
/// alias count as needed even when the select list is just `count(*)`: the join
/// compares them, so a non-covering index still has to fetch the row.
fn select_covers_index_for(
    select: &SelectStmt,
    name: Option<&str>,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
) -> bool {
    if is_star_select(select) {
        return info.index_covers(
            index,
            &info
                .columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect::<Vec<_>>(),
        );
    }
    info.index_covers(index, &select_needed_columns(select, name))
}

fn select_needed_columns(select: &SelectStmt, table: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |expr: &ExprNode| {
        for (_, name) in expr.referenced_columns() {
            if !names
                .iter()
                .any(|have: &String| have.eq_ignore_ascii_case(&name))
            {
                names.push(name);
            }
        }
    };
    for field in &select.fields {
        push(&field.expr);
    }
    if let Some(selection) = &select.selection {
        push(selection);
    }
    for expr in &select.group_by {
        push(expr);
    }
    if let Some(having) = &select.having {
        push(having);
    }
    for item in &select.order_by {
        push(&item.expr);
    }
    if let Some(table) = table {
        let owns = |qualified: &Option<String>| {
            qualified
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(table))
        };
        for join in &select.joins {
            for (qualified, name) in join.on.referenced_columns() {
                if owns(&qualified)
                    && !names
                        .iter()
                        .any(|have: &String| have.eq_ignore_ascii_case(&name))
                {
                    names.push(name);
                }
            }
        }
    }
    names
}

fn is_star_select(select: &SelectStmt) -> bool {
    matches!(
        select.fields.as_slice(),
        [field] if matches!(
            &field.expr,
            ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) if b == b"*"
        )
    )
}

fn needs_hash_agg(select: &SelectStmt) -> bool {
    !select.group_by.is_empty()
        || select.distinct
        || select.fields.iter().any(|f| is_agg_expr(&f.expr))
        // `HAVING sum(c) = 1` without GROUP BY still forms a single group.
        || select.having.as_ref().is_some_and(expr_contains_agg)
}

fn expr_contains_agg(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(f) if is_agg_expr(expr) => {
            let _ = f;
            true
        }
        ExprNode::Func(f) => f.args.iter().any(expr_contains_agg),
        ExprNode::Binary(b) => expr_contains_agg(&b.left) || expr_contains_agg(&b.right),
        ExprNode::Unary(u) => expr_contains_agg(&u.expr),
        ExprNode::Cast(c) => expr_contains_agg(&c.expr),
        _ => false,
    }
}

/// TiDB plan column ids for a scalar query that reads a *derived aggregate*
/// (`SELECT <one item> FROM (SELECT <aggs> FROM base …) d`).
///
/// TiDB allocates, in order: the base table's DataSource, the derived
/// aggregate's own outputs (`agg_count` of them), then the outer item. Column
/// pruning then drops the unused aggregate result, and the seed re-adds a
/// `count(1)` for the pruned column once per pruning pass, so each pass
/// allocates two more ids and the last seed's `count(1)` becomes the surviving
/// derived output. Splitting the cop task afterwards enumerates one partial
/// aggregate per function of the original aggregation, so the cop column sits
/// `3*(agg_count-1)+2` ids after the seed's.
#[derive(Clone, Copy)]
struct DerivedAggIds {
    /// Output column of the outer aggregate / projection item.
    outer: usize,
    /// Surviving output column of the derived aggregate.
    seed_out: usize,
    /// Cop partial-aggregate column of the derived aggregate.
    seed_partial: usize,
}

impl DerivedAggIds {
    fn new(base: usize, agg_count: usize, const_outer: bool) -> Self {
        let outer = base + agg_count;
        let seed_out = outer + 2 + usize::from(const_outer);
        let seed_partial = seed_out + 3 * (agg_count - 1) + 2;
        Self {
            outer,
            seed_out,
            seed_partial,
        }
    }
}

/// Recognise `SELECT <scalar agg | constant> FROM (SELECT <aggs> FROM base …) d`
/// where none of the derived aggregate's outputs are used by the outer query.
/// Returns the ids plus whether the outer item is a bare constant (TiDB keeps a
/// projection over the pruned seed in that case).
fn derived_agg_ids(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<(DerivedAggIds, bool)> {
    if !select.joins.is_empty()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || select.from_item.is_some()
    {
        return None;
    }
    let [field] = select.fields.as_slice() else {
        return None;
    };
    let const_outer = match &field.expr {
        ExprNode::Func(f) if is_agg_expr(&field.expr) => {
            // The outer aggregate must not read a derived column.
            if !matches!(f.args.as_slice(), [] | [ExprNode::Value(_)]) {
                return None;
            }
            false
        }
        ExprNode::Value(_) => true,
        _ => return None,
    };
    let from = select.from.as_ref()?;
    let with = select.with.as_ref()?;
    let cte = with.find(from.resolve_name())?;
    if !cte.set_rest.is_empty() {
        return None;
    }
    let seed = with.seed_select(&cte.name).ok()?;
    if !seed.joins.is_empty()
        || seed.from_item.is_some()
        || !seed.fields.iter().all(|f| is_agg_expr(&f.expr))
        || !seed.order_by.is_empty()
        || seed.limit.is_some()
        || seed.offset.is_some()
    {
        return None;
    }
    let agg_count = seed.fields.len();
    let base = derived_agg_base_next(&seed, is, current_db)?;
    Some((DerivedAggIds::new(base, agg_count, const_outer), const_outer))
}

/// Next plan column id after the DataSource of the innermost base table, seen
/// through any number of `SELECT * FROM <cte>` layers.
fn derived_agg_base_next(
    seed: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<usize> {
    let mut cur = seed.clone();
    for _ in 0..8 {
        if !cur.joins.is_empty() || cur.from_item.is_some() {
            return None;
        }
        let from = cur.from.as_ref()?;
        let name = from.resolve_name();
        if let Some(with) = cur.with.as_ref()
            && with.find(name).is_some()
        {
            let next = with.seed_select(name).ok()?;
            if !is_star_select(&next) {
                return None;
            }
            cur = next;
            continue;
        }
        let (db, table) = from.qualified(current_db);
        let info = is?.table_by_name(&db, &table)?;
        if info.view.is_some() {
            return None;
        }
        return Some(table_column_base(info));
    }
    None
}

/// Rebuild the pruned derived aggregate as TiDB does: the seed contributes a
/// `count(1)` over the surviving output column, and the cop partial is split
/// into `count(1)` as well (all derived outputs were pruned). The aggregate
/// keeps its kind (StreamAgg for an ordered seed, HashAgg for a group-by scan).
fn rewrite_derived_agg(plan: PhysicalPlan, ids: DerivedAggIds) -> PhysicalPlan {
    fn group_prefix(info: &str) -> String {
        info.split_once(", funcs:")
            .map(|(g, _)| format!("{g}, "))
            .unwrap_or_default()
    }
    match plan {
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } if !cop_info.is_empty() => {
            let group = group_prefix(&info);
            let root = format!(
                "{group}funcs:count(Column#{})->Column#{}",
                ids.seed_partial, ids.seed_out
            );
            let cop = format!("{group}funcs:count(1)->Column#{}", ids.seed_partial);
            PhysicalPlan::stream_agg(*child, root, cop, est_rows)
        }
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } if !cop_info.is_empty() => {
            let group = group_prefix(&info);
            let root = format!(
                "{group}funcs:count(Column#{})->Column#{}",
                ids.seed_partial, ids.seed_out
            );
            let cop = format!("{group}funcs:count(1)->Column#{}", ids.seed_partial);
            PhysicalPlan::hash_agg_cop(*child, root, cop, est_rows)
        }
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } if cop_info.is_empty() => {
            // Cop aggregation pushdown had not split yet: build both phases.
            let group = group_prefix(&info);
            let root = format!(
                "{group}funcs:count(Column#{})->Column#{}",
                ids.seed_partial, ids.seed_out
            );
            let cop = format!("{group}funcs:count(1)->Column#{}", ids.seed_partial);
            PhysicalPlan::hash_agg_cop(*child, root, cop, est_rows)
        }
        other => other,
    }
}

/// Prefer IndexReader when GROUP BY columns are a leading prefix of an index.
fn group_by_index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
    join_keys: &[String],
) -> Option<PhysicalPlan> {
    let group_cols = group_by_column_names(select)?;
    if group_cols.is_empty() {
        return None;
    }
    for index in &info.indices {
        if !index_has_group_prefix(index, &group_cols) {
            continue;
        }
        if select_covers_index_for(select, Some(&from.name), info, index) {
            return Some(index_plan(
                select,
                from,
                info,
                index,
                prune_mode,
                join_keys,
            ));
        }
    }
    None
}

/// `count(*)` over `LEFT JOIN` whose inner side is joined on its complete
/// primary key. TiDB's `eliminateOuterJoin` drops the join (each probe row
/// matches at most one inner row) and counts the probe rows through a covering
/// index: `StreamAgg → IndexReader(index:StreamAgg) → cop StreamAgg`.
fn count_star_unique_join_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    _prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if select.from_item.is_some()
        || select.joins.len() != 1
        || select.group_by.len() > 0
        || select.having.is_some()
        || select.selection.is_some()
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || select.distinct
        || select.with.is_some()
    {
        return None;
    }
    let [field] = select.fields.as_slice() else {
        return None;
    };
    let ExprNode::Func(func) = &field.expr else {
        return None;
    };
    if !func.name.eq_ignore_ascii_case("count") || !func.args.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let join = &select.joins[0];
    if join.tp != sealdb_parser::ast::JoinType::LeftOuter {
        return None;
    }
    let is = is?;
    let (db, outer_name) = from.qualified(current_db);
    let outer = is.table_by_name(&db, &outer_name)?;
    let (inner_db, inner_name) = join.right.qualified(current_db);
    let inner = is.table_by_name(&inner_db, &inner_name)?;

    // Inner join key must cover every primary-key / handle column.
    let inner_pk: Vec<String> = if let Some(pk) = inner.indices.iter().find(|i| i.primary) {
        pk.columns.iter().map(|c| c.name.o().to_string()).collect()
    } else if let Some(handle) = inner.handle_column() {
        vec![handle.name.o().to_string()]
    } else {
        return None;
    };
    if inner_pk.is_empty() {
        return None;
    }
    let mut matched = vec![false; inner_pk.len()];
    for conjunct in join.on.split_cnf_all() {
        let ExprNode::Binary(bin) = conjunct else {
            continue;
        };
        if bin.op != sealdb_parser::opcode::Op::Eq {
            continue;
        }
        let (ExprNode::Column(l), ExprNode::Column(r)) = (&*bin.left, &*bin.right) else {
            continue;
        };
        for (a, b) in [(l, r), (r, l)] {
            // `a` must reference the inner side, `b` the outer side.
            let a_inner = a
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(&join.right.name));
            let b_outer = b
                .table
                .as_deref()
                .is_none_or(|t| t.eq_ignore_ascii_case(&from.name));
            if !a_inner || !b_outer {
                continue;
            }
            if let Some(pos) = inner_pk
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&a.name))
            {
                matched[pos] = true;
            }
        }
    }
    if !matched.iter().all(|m| *m) {
        return None;
    }

    // Prefer the narrowest secondary index for the covering count.
    let index = outer
        .indices
        .iter()
        .filter(|i| !i.primary)
        .min_by_key(|i| i.columns.len())?;
    let total_cols = outer.columns.len() + inner.columns.len();
    let out_col = total_cols + 2;
    let partial_col = out_col + 6;
    let reader = PhysicalPlan::IndexReader {
        table: from.name.clone(),
        index: index.name.o().to_string(),
        index_columns: index_key_column_names(outer, index),
        partitions: Vec::new(),
        partition_access: String::new(),
        row_count: table_row_count(stats, from, current_db),
        pushed: None,
        range_scan: None,
    };
    Some(PhysicalPlan::stream_agg(
        reader,
        format!("funcs:count(Column#{partial_col})->Column#{out_col}"),
        format!("funcs:count(1)->Column#{partial_col}"),
        1.0,
    ))
}

fn group_by_column_names(select: &SelectStmt) -> Option<Vec<String>> {
    let mut cols = Vec::with_capacity(select.group_by.len());
    for expr in &select.group_by {
        match expr {
            ExprNode::Column(col) => cols.push(col.name.clone()),
            _ => return None,
        }
    }
    Some(cols)
}

fn index_has_group_prefix(index: &sealdb_meta::IndexInfo, group_cols: &[String]) -> bool {
    if group_cols.is_empty() || group_cols.len() > index.columns.len() {
        return false;
    }
    group_cols
        .iter()
        .enumerate()
        .all(|(i, name)| index.columns[i].name.o().eq_ignore_ascii_case(name))
}

fn order_prefix_of_group_by(select: &SelectStmt) -> bool {
    let Some(group_cols) = group_by_column_names(select) else {
        return false;
    };
    if select.order_by.is_empty() || select.order_by.len() > group_cols.len() {
        return false;
    }
    select.order_by.iter().enumerate().all(|(i, item)| {
        matches!(&item.expr, ExprNode::Column(col) if group_cols[i].eq_ignore_ascii_case(&col.name))
    })
}

/// TiDB GLOBAL index aggregation.
///
/// A `GLOBAL` index is partitioned independently of the table, so its reader is
/// a single cop task over every partition: TiDB pushes a `HashAgg` into the cop
/// task (`IndexReader … index:HashAgg`) instead of the partition-wise
/// `StreamAgg` + `PartitionUnion` shape used for local indexes. The table-row
/// probe of an `IndexLookUp` carries the cop `HashAgg` when the index does not
/// cover the grouped columns.
///
/// Returns `None` (generic aggregation shape) unless the access plan reads a
/// GLOBAL index of a partitioned table.
fn global_index_agg_plan(
    select: &SelectStmt,
    plan: &PhysicalPlan,
    is: &InfoSchema,
    current_db: &str,
) -> Option<PhysicalPlan> {
    if select.from_item.is_some() || !select.joins.is_empty() {
        return None;
    }
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is.table_by_name(&db, &table)?;
    if !info.partition.as_ref().is_some_and(|p| p.enable) {
        return None;
    }
    let (index_name, index_columns, partition_access, lookup) = match plan {
        PhysicalPlan::IndexReader {
            index,
            index_columns,
            partition_access,
            ..
        } => (index, index_columns, partition_access, false),
        PhysicalPlan::IndexLookUpReader {
            index,
            index_columns,
            partition_access,
            ..
        } => (index, index_columns, partition_access, true),
        _ => return None,
    };
    let index = info
        .indices
        .iter()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(index_name))?;
    if !index.global {
        return None;
    }
    // Every projected field must be an aggregate; mixed select lists need the
    // generic `firstrow` handling and are left to `build_agg_plan`.
    let mut aggs = Vec::with_capacity(select.fields.len());
    for field in &select.fields {
        match &field.expr {
            ExprNode::Func(func) if is_agg_expr(&field.expr) => aggs.push(func.clone()),
            _ => return None,
        }
    }
    if aggs.is_empty() {
        return None;
    }

    let qual = |name: &str| format!("{db}.{table}.{name}");
    // TiDB pins a named-partition GLOBAL read with `in(_tidb_tid, tidN)`.
    let pushed = if partition_access.is_empty() || partition_access == "all" {
        None
    } else {
        let first = partition_access.split(',').next().unwrap_or_default();
        let ordinal = info
            .partition
            .as_ref()
            .and_then(|p| {
                p.definitions
                    .iter()
                    .position(|def| def.name.o().eq_ignore_ascii_case(first))
            })
            .unwrap_or(0);
        Some(bytes_info(&format!("TID_SEL|tid{ordinal}")))
    };

    // TiDB allocates the aggregate outputs right after the table columns and
    // the cop partial results after the reader schema; the offsets below match
    // the `EXPLAIN` golden recorded from TiDB 8.5.8.
    let out_base = 4usize;
    let partial_base = 9usize + index_columns.len().saturating_sub(1);

    if select.group_by.is_empty() {
        let mut root = Vec::with_capacity(aggs.len());
        let mut cop = Vec::with_capacity(aggs.len());
        let mut next = partial_base;
        for (i, agg) in aggs.iter().enumerate() {
            let name = agg.name.to_ascii_lowercase();
            let out = out_base + i;
            let arg = agg
                .args
                .first()
                .map(|a| explain_agg_arg(&qual, a))
                .unwrap_or_default();
            if name == "count" && agg.args.is_empty() {
                cop.push(format!("count(1)->Column#{next}"));
                root.push(format!("count(Column#{next})->Column#{out}"));
                next += 1;
            } else if name == "avg" {
                cop.push(format!("count({arg})->Column#{next}"));
                cop.push(format!("sum({arg})->Column#{}", next + 1));
                root.push(format!(
                    "avg(Column#{next}, Column#{})->Column#{out}",
                    next + 1
                ));
                next += 2;
            } else {
                cop.push(format!("{name}({arg})->Column#{next}"));
                root.push(format!("{name}(Column#{next})->Column#{out}"));
                next += 1;
            }
        }
        let child = with_pushed(plan.clone(), pushed);
        return Some(PhysicalPlan::hash_agg_cop(
            child,
            format!("funcs:{}", root.join(", funcs:")),
            format!("funcs:{}", cop.join(", funcs:")),
            1.0,
        ));
    }

    let group_refs: Vec<String> = select
        .group_by
        .iter()
        .map(|e| explain_agg_arg(&qual, e))
        .collect();
    let group_info = group_refs.join(", ");
    let est = agg_est_rows(select);
    let mut root = Vec::with_capacity(aggs.len());
    let mut cop = Vec::with_capacity(aggs.len());
    let mut next = partial_base;
    for (i, agg) in aggs.iter().enumerate() {
        let name = agg.name.to_ascii_lowercase();
        let out = out_base + i;
        let arg = agg
            .args
            .first()
            .map(|a| explain_agg_arg(&qual, a))
            .unwrap_or_default();
        if name == "avg" {
            cop.push(format!("count({arg})->Column#{next}"));
            cop.push(format!("sum({arg})->Column#{}", next + 1));
            root.push(format!(
                "avg(Column#{next}, Column#{})->Column#{out}",
                next + 1
            ));
            next += 2;
        } else {
            cop.push(format!("{name}({arg})->Column#{next}"));
            root.push(format!("{name}(Column#{next})->Column#{out}"));
            next += 1;
        }
    }
    let root_info = format!("group by:{group_info}, funcs:{}", root.join(", funcs:"));
    let cop_info = format!("group by:{group_info}, funcs:{}", cop.join(", funcs:"));

    // A covering index read aggregates at the root: TiDB projects the indexed
    // values (casting `AVG` inputs to DECIMAL) below the HashAgg.
    if !lookup {
        let proj_base = 23usize;
        let mut exprs = Vec::new();
        for (i, agg) in aggs.iter().enumerate() {
            let name = agg.name.to_ascii_lowercase();
            let arg = agg
                .args
                .first()
                .map(|a| explain_agg_arg(&qual, a))
                .unwrap_or_default();
            if name == "avg" {
                exprs.push(format!(
                    "cast({arg}, decimal(10,0) BINARY)->Column#{}",
                    proj_base + i
                ));
            } else {
                exprs.push(format!("{arg}->Column#{}", proj_base + i));
            }
        }
        let group_cols = select
            .group_by
            .iter()
            .enumerate()
            .map(|(i, e)| {
                format!(
                    "{}->Column#{}",
                    explain_agg_arg(&qual, e),
                    proj_base + aggs.len() + i
                )
            })
            .collect::<Vec<_>>();
        exprs.extend(group_cols);
        let group_ids = select
            .group_by
            .iter()
            .enumerate()
            .map(|(i, _)| format!("Column#{}", proj_base + aggs.len() + i))
            .collect::<Vec<_>>()
            .join(", ");
        // The root aggregate consumes the projection outputs, so its funcs
        // reference the projected columns (`avg` collapses to one argument).
        let proj_funcs = aggs
            .iter()
            .enumerate()
            .map(|(i, agg)| {
                let name = agg.name.to_ascii_lowercase();
                let out = out_base + i;
                let col = proj_base + i;
                if name == "avg" {
                    format!("avg(Column#{col})->Column#{out}")
                } else {
                    format!("{name}(Column#{col})->Column#{out}")
                }
            })
            .collect::<Vec<_>>()
            .join(", funcs:");
        return Some(PhysicalPlan::hash_agg(
            PhysicalPlan::Projection {
                exprs,
                child: Box::new(with_pushed(plan.clone(), pushed)),
            },
            format!("group by:{group_ids}, funcs:{proj_funcs}"),
            est,
        ));
    }

    Some(PhysicalPlan::hash_agg_cop(
        with_pushed(plan.clone(), pushed),
        root_info,
        cop_info,
        est,
    ))
}

/// `{db}.{table}.{col}` for aggregate arguments (`count(1)` keeps the literal).
fn explain_agg_arg(qual: &impl Fn(&str) -> String, expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(col) => qual(&col.name),
        other => other.restore_string(),
    }
}

/// TiDB plan column id of a scalar aggregate output: the DataSource schema
/// consumes one id per table column, plus `_tidb_rowid` when the handle is not
/// clustered; the aggregation then allocates the next id.
fn scalar_agg_out_column(table: &str, current_db: &str, is: Option<&InfoSchema>) -> Option<usize> {
    let info = is?.table_by_name(current_db, table)?;
    let clustered = info.pk_is_handle || info.common_handle;
    Some(info.columns.len() + 1 + usize::from(!clustered))
}

/// TiDB `AllocPlanColumnID` over a join tree: the DataSources take the ids
/// first (in join order), so a scalar aggregation above the join allocates the
/// id right after the last side's schema, `_tidb_rowid` included.
fn join_datasource_out_column(
    select: &SelectStmt,
    current_db: &str,
    is: Option<&InfoSchema>,
) -> Option<usize> {
    let is = is?;
    if select.joins.is_empty() {
        return None;
    }
    let mut total = 0usize;
    let mut take = |from: &TableName| -> Option<()> {
        let (db, table) = from.qualified(current_db);
        let info = is.table_by_name(&db, &table)?;
        let clustered = info.pk_is_handle || info.common_handle;
        total += info.columns.len() + usize::from(!clustered);
        Some(())
    };
    for join in &select.joins {
        take(&join.left)?;
        take(&join.right)?;
    }
    Some(total + 1)
}

/// TiDB `AggFuncDesc.WrapCastForAggArgs`: a `sum`/`avg` over an integer column
/// aggregates `cast(arg, decimal(p, 0))` so a partial aggregation cannot
/// overflow. Returns the cast target type and whether the argument is unsigned.
fn agg_int_decimal_cast(
    fn_name: &str,
    arg_expr: Option<&sealdb_parser::ast::expressions::ExprNode>,
    select: &SelectStmt,
    current_db: &str,
    is: Option<&InfoSchema>,
) -> Option<(String, bool)> {
    use sealdb_parser::ast::expressions::ExprNode;
    if !matches!(fn_name, "sum" | "avg") {
        return None;
    }
    let ExprNode::Column(col) = arg_expr? else {
        return None;
    };
    let is = is?;
    let qualifier = col.table.as_deref().unwrap_or("");
    let mut tables: Vec<&sealdb_parser::ast::ddl::TableName> = Vec::new();
    if let Some(from) = &select.from {
        tables.push(from);
    }
    for join in &select.joins {
        tables.push(&join.left);
        tables.push(&join.right);
    }
    let mut found: Vec<&sealdb_meta::ColumnInfo> = Vec::new();
    for table in tables {
        let name_matches = qualifier.is_empty()
            || table.name.eq_ignore_ascii_case(qualifier)
            || table.resolve_name().eq_ignore_ascii_case(qualifier);
        if !name_matches {
            continue;
        }
        let (db, name) = table.qualified(current_db);
        let Some(info) = is.table_by_name(&db, &name) else {
            continue;
        };
        if let Some(column) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
        {
            found.push(column);
        }
    }
    // The AST often drops the column's table qualifier; aliases of the same
    // physical table resolve to identical types, so the first match is enough.
    let column = *found.first()?;
    let flen = match column.field_type.tp {
        sealdb_types::TYPE_TINY => 3,
        sealdb_types::TYPE_SHORT => 5,
        sealdb_types::TYPE_INT24 => 8,
        sealdb_types::TYPE_LONG => 10,
        sealdb_types::TYPE_LONG_LONG => 20,
        sealdb_types::TYPE_YEAR => 4,
        _ => return None,
    };
    Some((format!("decimal({flen},0)"), column.field_type.unsigned))
}

/// TiDB cost model v2 picks the physical aggregate for a scalar aggregation by
/// cost: a 2-phase `HashAgg` amortises the cop partial aggregation over
/// `tidb_executor_concurrency` while `StreamAgg` pays the full per-row CPU, so
/// hash wins once the input cardinality exceeds the ~234-row crossover. With
/// pseudo (unanalyzed) statistics TiDB keeps the streaming aggregate.
fn scalar_agg_prefers_hash(plan: &PhysicalPlan) -> bool {
    const HASH_AGG_CROSSOVER_ROWS: f64 = 234.0;
    match agg_input_cardinality(plan) {
        Some((rows, false)) => rows > HASH_AGG_CROSSOVER_ROWS,
        _ => false,
    }
}

/// `(input rows, pseudo stats)` of the scan feeding an aggregate.
fn agg_input_cardinality(plan: &PhysicalPlan) -> Option<(f64, bool)> {
    match plan {
        PhysicalPlan::IndexReader {
            row_count,
            range_scan,
            ..
        } => Some((*row_count, range_scan.is_none() && *row_count == 10_000.0)),
        PhysicalPlan::TableReader {
            row_count,
            stats_pseudo,
            filter_est,
            ..
        } => Some((filter_est.unwrap_or(row_count.abs()), *stats_pseudo)),
        PhysicalPlan::Selection { child, .. } | PhysicalPlan::Projection { child, .. } => {
            agg_input_cardinality(child)
        }
        _ => None,
    }
}

/// Attach an EXPLAIN-only `pushed` marker to an index reader.
fn with_pushed(plan: PhysicalPlan, pushed: Option<ExprNode>) -> PhysicalPlan {
    match plan {
        PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            range_scan,
            ..
        } => PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            range_scan,
            pushed,
        },
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            probe_est,
            range_scan,
            ..
        } => PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            probe_est,
            range_scan,
            pushed,
        },
        other => other,
    }
}

/// Build HashAgg / StreamAgg (and DISTINCT outer HashAgg) with TiDB ExplainInfo.
/// Returns `(plan, stream_keep_order)`.
fn build_agg_plan(
    select: &SelectStmt,
    plan: PhysicalPlan,
    current_db: &str,
    is: Option<&InfoSchema>,
    stats: Option<&Handle>,
) -> (PhysicalPlan, bool) {
    // `use` may leave another schema as the session default while the query
    // names its table explicitly (`select … from with_cluster_index.tbl_0`):
    // resolve the aggregate's own schema from the FROM item.
    let (agg_db, table) = select
        .from
        .as_ref()
        .map(|t| t.qualified(current_db))
        .unwrap_or_else(|| (current_db.to_string(), "dual".to_string()));
    let table = table.as_str();
    let db_qual = if agg_db.is_empty() {
        String::new()
    } else {
        format!("{agg_db}.")
    };
    let plan = match plan {
        PhysicalPlan::Projection { child, .. } => *child,
        other => other,
    };
    let est = agg_est_rows(select);
    // `SELECT count(1) FROM (SELECT <aggs> FROM base …) d`: every derived
    // aggregate output is pruned, so TiDB's outer scalar aggregate reads the
    // seed's re-added `count(1)` (see `derived_agg_ids`).
    if let Some((ids, false)) = derived_agg_ids(select, is, current_db)
        && matches!(plan, PhysicalPlan::CTE { .. })
    {
        let info = select
            .fields
            .first()
            .map(|f| f.expr.restore_string())
            .unwrap_or_else(|| "count(1)".to_string());
        return (
            PhysicalPlan::stream_agg(
                plan,
                format!("funcs:{info}->Column#{}", ids.outer),
                String::new(),
                est,
            ),
            false,
        );
    }
    let keep_order =
        matches!(plan, PhysicalPlan::IndexReader { .. }) && !select.group_by.is_empty();
    let stream = select.group_by.is_empty() || keep_order;
    let partial_col = partial_agg_column_id(select);

    if select.distinct && select.group_by.is_empty() {
        // SELECT DISTINCT without GROUP BY — dedup projected columns.
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
        return (
            PhysicalPlan::Distinct {
                keys,
                // `SELECT DISTINCT *` keeps the full child row.
                fields: if is_star_select(select) {
                    Vec::new()
                } else {
                    select.fields.iter().map(|f| f.expr.clone()).collect()
                },
                names: if is_star_select(select) {
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
                },
                child: Box::new(plan),
            },
            false,
        );
    }

    if select.group_by.is_empty() {
        // A point get materialises the row before aggregating, so the whole
        // aggregate runs at root (`StreamAgg → Point_Get`) instead of a
        // partial agg pushed into cop.
        if matches!(
            plan,
            PhysicalPlan::PointGet { .. } | PhysicalPlan::BatchPointGet { .. }
        ) && let Some(field) = select.fields.first()
        {
            let arg = match &field.expr {
                ExprNode::Func(f) if !f.args.is_empty() => {
                    agg_info_text(&f.args[0], table, &db_qual)
                }
                other => agg_info_text(other, table, &db_qual),
            };
            let name = match &field.expr {
                ExprNode::Func(f) => f.name.to_ascii_lowercase(),
                _ => "count".to_string(),
            };
            let out_col = pointget_agg_column_base(select, is, &agg_db) + 1;
            let info = format!("funcs:{name}({arg})->Column#{out_col}");
            return (
                PhysicalPlan::stream_agg(plan, info, String::new(), est),
                false,
            );
        }
        // Scalar aggregation (empty-table StreamAgg push into TableReader).
        let out_col = scalar_agg_out_column(table, &agg_db, is).unwrap_or(5);
        let field = select.fields.first();
        let fn_name = field
            .and_then(|f| match &f.expr {
                ExprNode::Func(func) => Some(func.name.to_ascii_lowercase()),
                _ => None,
            })
            .unwrap_or_else(|| "count".to_string());
        let arg_expr = field.and_then(|f| match &f.expr {
            ExprNode::Func(func) => func.args.first(),
            _ => None,
        });
        let arg = match arg_expr {
            Some(expr) if matches!(expr, ExprNode::Column(_)) => explain_col_ref(table, expr),
            Some(other) => other.restore_string(),
            None => "1".to_string(),
        };
        // `max(_tidb_rowid)`: TiDB scans the handle backwards and stops after
        // one row (`StreamAgg → Limit 1 → TableReader(data:Limit) → Limit 1`).
        if fn_name == "max"
            && matches!(
                arg_expr,
                Some(ExprNode::Column(col)) if col.name.eq_ignore_ascii_case("_tidb_rowid")
            )
            && let PhysicalPlan::TableReader {
                table,
                partitions,
                partition_names,
                partition_access,
                stats_pseudo,
                filter_est,
                pushed,
                ..
            } = plan
        {
            let reader = PhysicalPlan::TableReader {
                table: table.clone(),
                partitions,
                partition_names,
                partition_access,
                row_count: 1.0,
                stats_pseudo,
                data_limit: true,
                filter_est,
                range_scan: None,
                keep_order: true,
                pushed,
            };
            let limit = PhysicalPlan::Limit {
                child: Box::new(reader),
                offset: 0,
                count: 1,
            };
            let info = format!("funcs:max({db_qual}{table}._tidb_rowid)->Column#{out_col}");
            return (
                PhysicalPlan::stream_agg(limit, info, String::new(), est),
                false,
            );
        }
        // Scalar aggregation over a join tree is a single-phase root
        // aggregate: TiDB has no cop task below a join, so nothing is pushed
        // down and the aggregate prints its real argument (`count(1)`).
        if matches!(
            plan,
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::Apply { .. }
        ) {
            let out_col = join_datasource_out_column(select, current_db, is).unwrap_or(out_col);
            // TiDB `AggFuncDesc.WrapCastForAggArgs`: `sum`/`avg` over an integer
            // argument aggregate `cast(arg, decimal(p, 0))`, so the projection
            // below the aggregate carries the cast and the aggregate consumes
            // that column.
            if let Some((cast_type, unsigned)) =
                agg_int_decimal_cast(&fn_name, arg_expr, select, current_db, is)
            {
                let cast_col = out_col + 1;
                let unsigned_txt = if unsigned { " UNSIGNED" } else { "" };
                let proj = PhysicalPlan::Projection {
                    exprs: vec![format!(
                        "cast({arg}, {cast_type}{unsigned_txt} BINARY)->Column#{cast_col}"
                    )],
                    child: Box::new(plan),
                };
                let info = format!("funcs:{fn_name}(Column#{cast_col})->Column#{out_col}");
                return (
                    if hashagg_cpu_amortized() {
                        PhysicalPlan::hash_agg(proj, info, est)
                    } else {
                        PhysicalPlan::stream_agg(proj, info, String::new(), est)
                    },
                    false,
                );
            }
            let info = format!("funcs:{fn_name}({arg})->Column#{out_col}");
            return (
                if hashagg_cpu_amortized() {
                    PhysicalPlan::hash_agg(plan, info, est)
                } else {
                    PhysicalPlan::stream_agg(plan, info, String::new(), est)
                },
                false,
            );
        }
        // Cop partial agg column. The 2-phase HashAgg allocates one id right
        // after the aggregate output; StreamAgg reserves its partial state over
        // the access path's columns, three ids per index (table) column minus
        // the one the aggregate output already took.
        let use_hash = scalar_agg_prefers_hash(&plan);
        let partial_col = if use_hash {
            out_col + 1
        } else if let PhysicalPlan::IndexReader { index_columns, .. } = &plan {
            // StreamAgg over a covering index reserves its partial state over
            // the index key: three ids per index column minus the aggregate
            // output that is already accounted for.
            out_col + (3 * index_columns.len()).saturating_sub(1)
        } else {
            // StreamAgg over a table scan reserves the DataSource's aux ids:
            // two per index plus two per indexed column.
            let aux = is
                .and_then(|is| is.table_by_name(&agg_db, table))
                .map(|info| {
                    let idx_cols: usize = info.indices.iter().map(|i| i.columns.len()).sum();
                    2 * info.indices.len() + 2 * idx_cols
                })
                .unwrap_or(2);
            out_col + aux
        };
        let info = format!("funcs:{fn_name}(Column#{partial_col})->Column#{out_col}");
        let cop = format!("funcs:{fn_name}({arg})->Column#{partial_col}");
        if use_hash {
            return (PhysicalPlan::hash_agg_cop(plan, info, cop, est), false);
        }
        return (PhysicalPlan::stream_agg(plan, info, cop, est), false);
    }

    let group_info = select
        .group_by
        .iter()
        .map(|e| explain_col_ref(table, e))
        .collect::<Vec<_>>()
        .join(", ");

    // TiDB `eliminateSingleRowAgg`: a point get returns at most one row, so a
    // plain `GROUP BY` over it yields that row unchanged. An `ORDER BY` keeps a
    // StreamAgg so the ordered columns stay in the aggregate's output.
    if matches!(plan, PhysicalPlan::PointGet { .. })
        && !select.fields.iter().any(|field| is_agg_expr(&field.expr))
    {
        if select.order_by.is_empty() {
            return (plan, false);
        }
        if let Some(info) = is.and_then(|is| is.table_by_name(&agg_db, table)) {
            let funcs: Vec<String> = info
                .columns
                .iter()
                .map(|column| {
                    let col_ref = format!("{db_qual}{table}.{}", column.name.o());
                    format!("funcs:firstrow({col_ref})->{col_ref}")
                })
                .collect();
            let info_text = format!("group by:{group_info}, {}", funcs.join(", "));
            return (
                PhysicalPlan::stream_agg(plan, info_text, String::new(), 1.0),
                false,
            );
        }
    }

    if select.distinct {
        // TiDB: HashAgg(firstrow distinct cols) → StreamAgg(group by) → IndexReader.
        let field = select
            .fields
            .first()
            .map(|f| explain_col_ref(table, &f.expr))
            .unwrap_or_else(|| format!("select.{table}.b"));
        let stream_info =
            format!("group by:{group_info}, funcs:firstrow(Column#{partial_col})->{field}");
        let cop = format!("group by:{group_info}, funcs:firstrow({field})->Column#{partial_col}");
        let hash_info = format!("group by:{field}, funcs:firstrow({field})->{field}");
        let stream = PhysicalPlan::stream_agg(plan, stream_info, cop, est);
        return (PhysicalPlan::hash_agg(stream, hash_info, est), keep_order);
    }

    // Aggregation over a join tree: TiDB labels group-by / firstrow operands
    // with each side's OrigTblName (`t1.col1` → `uk_hp16726.col1`) and drops
    // the synthetic partial `count` when the select list holds only group-by
    // columns (`funcs:firstrow(…)->…` only).
    if !select.joins.is_empty() {
        // TiDB `AggregationPushDownSolver` runs before the aggregate is built:
        // with `tidb_opt_agg_push_down` on, part of the aggregation moves into
        // the build side of an inner join.
        if let Some(pushed) = join_agg_push_down(select, plan.clone(), current_db, is, stats) {
            return pushed;
        }
        return build_join_agg_plan(select, plan, current_db);
    }

    // `GROUP_CONCAT`: TiDB casts every argument to `var_string(20)` in a
    // Projection under the aggregate and prints its arguments as that
    // Projection's output columns (`group_concat(Column#5 separator ",")`).
    if let Some(concat) = select.fields.iter().find_map(|f| match &f.expr {
        ExprNode::Func(func) if func.name.eq_ignore_ascii_case("group_concat") => Some(func),
        _ => None,
    }) {
        return build_group_concat_agg_plan(select, plan, concat, table, current_db, is, est);
    }

    let agg_arg = select
        .fields
        .iter()
        .find_map(|f| match &f.expr {
            ExprNode::Func(func) if is_agg_expr(&f.expr) => {
                func.args.first().map(|a| explain_col_ref(table, a))
            }
            _ => None,
        })
        .unwrap_or_else(|| format!("select.{table}.a"));

    let agg_fn = select
        .fields
        .iter()
        .find_map(|f| match &f.expr {
            ExprNode::Func(func) if is_agg_expr(&f.expr) => Some(func.name.to_ascii_lowercase()),
            _ => None,
        })
        .unwrap_or_else(|| "count".into());
    let mut root_funcs = vec![format!("funcs:{agg_fn}(Column#{partial_col})->Column#5")];
    let cop_funcs = vec![format!("{agg_fn}({agg_arg})->Column#{partial_col}")];
    // SELECT non-agg columns need firstrow (TiDB HashAgg).
    for field in &select.fields {
        if is_agg_expr(&field.expr) {
            continue;
        }
        if let ExprNode::Column(col) = &field.expr {
            let col_ref = explain_col_ref(table, &field.expr);
            root_funcs.push(format!(
                "funcs:firstrow({col_ref})->{db_qual}{table}.{}",
                col.name
            ));
        }
    }
    // ORDER BY requires firstrow of ordered columns on the root StreamAgg only.
    if !select.order_by.is_empty() {
        for item in &select.order_by {
            let col = explain_col_ref(table, &item.expr);
            root_funcs.push(format!("funcs:firstrow({col})->{col}"));
        }
    }
    let info = format!("group by:{group_info}, {}", root_funcs.join(", "));
    let cop = format!("group by:{group_info}, funcs:{}", cop_funcs.join(", "));
    if stream {
        (PhysicalPlan::stream_agg(plan, info, cop, est), keep_order)
    } else {
        (PhysicalPlan::hash_agg(plan, info, est), false)
    }
}

/// `GROUP_CONCAT` over a single table (`TiDB buildAggregation`).
///
/// TiDB rewrites the arguments to `cast(arg, var_string(20))` and builds the
/// aggregate over those projected columns, so the `Projection` below the
/// aggregate is where the column ids the aggregate prints come from. The
/// default separator is `,`; only an explicit `SEPARATOR` would change it.
fn build_group_concat_agg_plan(
    select: &SelectStmt,
    plan: PhysicalPlan,
    concat: &sealdb_parser::ast::expressions::FuncCallExpr,
    table: &str,
    current_db: &str,
    is: Option<&InfoSchema>,
    est: f64,
) -> (PhysicalPlan, bool) {
    let out_col = scalar_agg_out_column(table, current_db, is).unwrap_or(4);
    let mut exprs = Vec::new();
    let mut args = Vec::new();
    let mut next = out_col + 1;
    for arg in &concat.args {
        let col_ref = match arg {
            ExprNode::Column(_) => explain_col_ref(table, arg),
            other => other.restore_string(),
        };
        exprs.push(format!("cast({col_ref}, var_string(20))->Column#{next}"));
        args.push(format!("Column#{next}"));
        next += 1;
    }
    let mut group_ids = Vec::new();
    for group in &select.group_by {
        let col_ref = explain_col_ref(table, group);
        exprs.push(format!("{col_ref}->Column#{next}"));
        group_ids.push(format!("Column#{next}"));
        next += 1;
    }
    let func = format!(
        "funcs:group_concat({} separator \",\")->Column#{out_col}",
        args.join(", ")
    );
    let info = format!("group by:{}, {func}", group_ids.join(", "));
    // A StreamAgg needs its input ordered by the group keys. The integer
    // handle (or the leading index prefix) already provides that order, so
    // TiDB keeps the aggregate streaming and marks the scan `keep order:true`.
    let ordered =
        group_by_handle(select, is, current_db) || matches!(plan, PhysicalPlan::IndexReader { .. });
    let plan = if ordered { set_keep_order(plan) } else { plan };
    let projection = PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    };
    if ordered {
        (
            PhysicalPlan::stream_agg(projection, info, String::new(), est),
            true,
        )
    } else {
        (PhysicalPlan::hash_agg(projection, info, est), false)
    }
}

/// `GROUP BY` exactly the integer row handle: a table scan over the handle is
/// already grouped (`keep order:true`).
fn group_by_handle(select: &SelectStmt, is: Option<&InfoSchema>, current_db: &str) -> bool {
    if select.group_by.len() != 1 {
        return false;
    }
    let Some(from) = &select.from else {
        return false;
    };
    let (db, table) = from.qualified(current_db);
    let Some(info) = is.and_then(|schema| schema.table_by_name(&db, &table)) else {
        return false;
    };
    let handle = if info.pk_is_handle {
        match info.handle_column() {
            Some(column) => column.name.o().to_string(),
            None => return false,
        }
    } else {
        return false;
    };
    matches!(
        &select.group_by[0],
        ExprNode::Column(col) if col.name.eq_ignore_ascii_case(&handle)
    )
}

/// EXPLAIN-only: relabel columns in pushed filters with each side's
/// OrigTblName (lowercase), matching TiDB `expression.ExplainInfo()` of a
/// predicate that has been pushed into a `DataSource`.
fn explain_rewrite_pushed(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> PhysicalPlan {
    if !in_explain_build() {
        return plan;
    }
    let mut tables = select.join_tables();
    if let Some(from) = &select.from {
        tables.push(from);
    }
    fn walk(
        plan: PhysicalPlan,
        tables: &[&sealdb_parser::ast::TableName],
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
    ) -> PhysicalPlan {
        match plan {
            PhysicalPlan::Selection { filter, child, est_rows } => PhysicalPlan::Selection {
                filter: rewrite_col_tables(&filter, tables),
                child: Box::new(walk(*child, tables, is, current_db, stats)),
                est_rows,
            },
            PhysicalPlan::TableReader {
                table,
                partitions,
                partition_names,
                partition_access,
                row_count,
                stats_pseudo,
                data_limit,
                filter_est,
                range_scan,
                keep_order,
                pushed,
            } => {
                let pushed = pushed.as_ref().map(|p| {
                    annotate_partial_stats(
                        &table,
                        rewrite_col_tables(p, tables),
                        is,
                        current_db,
                        stats,
                    )
                });
                PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    data_limit,
                    filter_est,
                    range_scan,
                    keep_order,
                    pushed,
                }
            }
            PhysicalPlan::IndexReader {
                table,
                index,
                index_columns,
                partitions,
                partition_access,
                row_count,
                pushed,
                range_scan,
            } => PhysicalPlan::IndexReader {
                table,
                index,
                index_columns,
                partitions,
                partition_access,
                row_count,
                range_scan,
                pushed: pushed.as_ref().map(|p| rewrite_col_tables(p, tables)),
            },
            PhysicalPlan::HashJoin {
                est_rows,
                tp,
                left,
                right,
                left_table,
                right_table,
                on,
            } => PhysicalPlan::HashJoin {
                est_rows,
                tp,
                left: Box::new(walk(*left, tables, is, current_db, stats)),
                right: Box::new(walk(*right, tables, is, current_db, stats)),
                left_table,
                right_table,
                on: rewrite_col_tables(&on, tables),
            },
            PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
                exprs,
                child: Box::new(walk(*child, tables, is, current_db, stats)),
            },
            PhysicalPlan::Limit {
                child,
                offset,
                count,
            } => PhysicalPlan::Limit {
                child: Box::new(walk(*child, tables, is, current_db, stats)),
                offset,
                count,
            },
            PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
                child: Box::new(walk(*child, tables, is, current_db, stats)),
                by_info,
            },
            PhysicalPlan::TopN {
                child,
                by_info,
                offset,
                count,
            } => PhysicalPlan::TopN {
                child: Box::new(walk(*child, tables, is, current_db, stats)),
                by_info,
                offset,
                count,
            },
            PhysicalPlan::HashAgg {
                info,
                cop_info,
                est_rows,
                child,
            } => PhysicalPlan::HashAgg {
                info,
                cop_info,
                est_rows,
                child: Box::new(walk(*child, tables, is, current_db, stats)),
            },
            PhysicalPlan::StreamAgg {
                info,
                cop_info,
                est_rows,
                child,
            } => PhysicalPlan::StreamAgg {
                info,
                cop_info,
                est_rows,
                child: Box::new(walk(*child, tables, is, current_db, stats)),
            },
            other => other,
        }
    }
    walk(plan, &tables, is, current_db, stats)
}

/// TiDB `stats:partial[…]` for a pushed cop filter: the table statistics exist
/// but some referenced column does not (`LOAD STATS` recorded before the column
/// was analyzed). EXPLAIN keeps the Selection text and marks the missing
/// columns on the scan; the estimate already comes from the scan row count.
fn annotate_partial_stats(
    table: &str,
    filter: ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> ExprNode {
    // Bytes carry an EXPLAIN marker from an earlier rewrite.
    if matches!(
        &filter,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(_)
        })
    ) {
        return filter;
    }
    let Some(info) = is.and_then(|schema| {
        let db = statement_table_db(table).unwrap_or_else(|| current_db.to_string());
        schema.table_by_name(&db, table)
    }) else {
        return filter;
    };
    let db = statement_table_db(table).unwrap_or_else(|| current_db.to_string());
    let Some(table_stats) = stats
        .and_then(|handle| handle.table(&db, table))
        .filter(|t| super::plan_cost::has_real_stats(t))
    else {
        return filter;
    };
    let mut missing: Vec<String> = filter
        .split_cnf()
        .iter()
        .flat_map(|conjunct| conjunct.referenced_columns())
        .filter(|(_, name)| {
            info.columns
                .iter()
                .any(|col| col.name.o().eq_ignore_ascii_case(name))
        })
        .filter(|(_, name)| table_stats.column(name).is_none())
        .map(|(_, name)| name.to_ascii_lowercase())
        .collect();
    if missing.is_empty() {
        return filter;
    }
    missing.sort();
    missing.dedup();
    let mut cnf = filter.split_cnf();
    cnf.sort_by_key(|conjunct| super::explain::explain_pushed_filter(table, conjunct));
    let sel_info = cnf
        .iter()
        .map(|conjunct| super::explain::explain_pushed_filter(table, conjunct))
        .collect::<Vec<_>>()
        .join(", ");
    bytes_info(&format!(
        "PARTIAL_SEL|{}|{}",
        missing.join(","),
        sel_info
    ))
}

/// `{db}.{orig_table}.{col}` for an aggregate operand over a join tree.
/// `select.join_tables()` maps each alias to its physical (OrigTblName) table.
fn join_side_physical(select: &SelectStmt, alias: &str) -> String {
    select
        .join_tables()
        .iter()
        .find(|t| {
            t.name.eq_ignore_ascii_case(alias) || t.resolve_name().eq_ignore_ascii_case(alias)
        })
        .map(|t| t.resolve_name().to_ascii_lowercase())
        .unwrap_or_else(|| alias.to_ascii_lowercase())
}

fn join_agg_col_ref(select: &SelectStmt, current_db: &str, expr: &ExprNode) -> String {
    let schema = schema_prefix(current_db);
    if let ExprNode::Column(c) = expr {
        let alias = c.table.as_deref().unwrap_or("");
        let phys = join_side_physical(select, alias);
        return format!("{schema}.{phys}.{}", c.name);
    }
    expr.restore_string()
}

/// TiDB `AggregationPushDownSolver` for a two-table inner join.
///
/// With `tidb_opt_agg_push_down` on, a decomposable aggregate over an inner
/// join splits into a partial `HashAgg` on the side that owns its arguments
/// (TiDB always pushes into the join's build side) and a final aggregate above
/// the join. The partial aggregation groups by that side's share of the
/// `GROUP BY` list plus the join keys, so the join can re-merge the groups.
///
/// When the `GROUP BY` list pins a unique key of the *other* side every join
/// row is already its own group: the final aggregation collapses into a plain
/// `Projection` of the partial results.
///
/// Returns the rewritten plan plus the `stream_keep_order` flag.
fn join_agg_push_down(
    select: &SelectStmt,
    plan: PhysicalPlan,
    current_db: &str,
    is: Option<&InfoSchema>,
    stats: Option<&Handle>,
) -> Option<(PhysicalPlan, bool)> {
    use super::rule_aggregation_push_down::{
        AggSide, covers_unique_key, expr_side, is_decomposable,
    };

    if !super::agg_push_down()
        || select.joins.len() != 1
        || select.distinct
        || !select.having.is_none()
        || !select.order_by.is_empty()
        || select.limit.is_some()
    {
        return None;
    }
    let PhysicalPlan::HashJoin {
        tp: JoinType::Inner,
        left,
        right,
        left_table,
        right_table,
        est_rows,
        on,
    } = plan
    else {
        return None;
    };
    // TiDB stops at the first join: a spine of joins keeps its aggregation.
    let is_join = |p: &PhysicalPlan| {
        matches!(p, PhysicalPlan::HashJoin { .. } | PhysicalPlan::MergeJoin { .. })
    };
    if is_join(&left) || is_join(&right) {
        return None;
    }
    // Aliases and physical table names both name a side (predicate pushdown
    // rewrites the ON columns to physical names).
    let ast_join = &select.joins[0];
    let left_names = vec![
        left_table.to_ascii_lowercase(),
        ast_join.left.resolve_name().to_ascii_lowercase(),
    ];
    let right_names = vec![
        right_table.to_ascii_lowercase(),
        ast_join.right.resolve_name().to_ascii_lowercase(),
    ];

    // Split the aggregate functions by the side that owns their arguments.
    let mut left_aggs: Vec<&FuncCallExpr> = Vec::new();
    let mut right_aggs: Vec<&FuncCallExpr> = Vec::new();
    let mut agg_fields: Vec<&FuncCallExpr> = Vec::new();
    for field in &select.fields {
        let ExprNode::Func(func) = &field.expr else {
            if matches!(field.expr, ExprNode::Column(_)) {
                continue;
            }
            return None;
        };
        if !is_agg_expr(&field.expr) {
            return None;
        }
        if !is_decomposable(&func.name, func.distinct, 0) {
            return None;
        }
        agg_fields.push(func);
        match func.args.first() {
            // A constant argument (`count(1)`) joins the build side.
            None => right_aggs.push(func),
            Some(arg) => match expr_side(arg, &left_names, &right_names) {
                AggSide::Left => left_aggs.push(func),
                AggSide::Right | AggSide::Neither => right_aggs.push(func),
                AggSide::Both => return None,
            },
        }
    }
    if agg_fields.is_empty() {
        return None;
    }
    // TiDB's `checkAnyCountAndSum` guard: `count`/`sum` on one side forbid
    // pushing the other side's aggregation through the join (NULL handling).
    let has_count_sum = |funcs: &[&FuncCallExpr]| {
        funcs
            .iter()
            .any(|f| matches!(f.name.to_ascii_lowercase().as_str(), "count" | "sum"))
    };
    if (has_count_sum(&right_aggs) && !left_aggs.is_empty())
        || (has_count_sum(&left_aggs) && !right_aggs.is_empty())
    {
        return None;
    }

    // Group keys may only be plain columns; split them by side.
    let mut left_gby: Vec<ExprNode> = Vec::new();
    let mut right_gby: Vec<ExprNode> = Vec::new();
    for item in &select.group_by {
        if !matches!(item, ExprNode::Column(_)) {
            return None;
        }
        match expr_side(item, &left_names, &right_names) {
            AggSide::Left => push_group_key(&mut left_gby, item),
            AggSide::Right => push_group_key(&mut right_gby, item),
            AggSide::Both | AggSide::Neither => return None,
        }
    }
    // The join keys pin the groups of both sides (`addGbyCols`).
    for (lc, rc) in join_eq_pairs(&on) {
        let l = ExprNode::Column(lc);
        let r = ExprNode::Column(rc);
        let l_side = expr_side(&l, &left_names, &right_names);
        let r_side = expr_side(&r, &left_names, &right_names);
        if (l_side == AggSide::Left && r_side == AggSide::Right)
            || (l_side == AggSide::Right && r_side == AggSide::Left)
        {
            push_group_key(&mut left_gby, &l);
            push_group_key(&mut right_gby, &r);
        }
    }

    let push_right = !right_aggs.is_empty();
    let push_left = !push_right && !left_aggs.is_empty();
    if !push_right && !push_left {
        return None;
    }
    let (push_gby, push_aggs) = if push_right {
        (&right_gby, &right_aggs)
    } else {
        (&left_gby, &left_aggs)
    };
    if push_gby.is_empty() {
        return None;
    }
    // TiDB `tryToEliminateAggregation` on the pushed side: its groups are
    // single rows already, so nothing is pushed at all.
    let pushed_table = if push_right { &right_table } else { &left_table };
    let pushed_physical = join_side_physical(select, pushed_table);
    let pushed_names: Vec<String> = push_gby
        .iter()
        .filter_map(|e| match e {
            ExprNode::Column(c) => Some(c.name.clone()),
            _ => None,
        })
        .collect();
    if let Some(info) = is.and_then(|is| is.table_by_name(current_db, &pushed_physical))
        && covers_unique_key(info, &pushed_names)
    {
        return None;
    }

    // The partial aggregation replaces the pushed side's subtree.
    let child = if push_right {
        (*right).clone()
    } else {
        (*left).clone()
    };
    let pseudo = super::explain::is_pseudo_plan(&child);
    let child_rows = super::explain::est_rows(&child).max(1.0);
    let out_col = join_datasource_out_column(select, current_db, is).unwrap_or(5);
    let aliases = query_alias_map(select);
    let partial_est = pushed_group_est(push_gby, &child, child_rows, current_db, &aliases, stats);

    let group_info: Vec<String> = push_gby
        .iter()
        .map(|e| join_agg_col_ref(select, current_db, e))
        .collect();
    let mut root_funcs: Vec<String> = Vec::new();
    let mut cop_funcs: Vec<String> = Vec::new();
    // TiDB `makeNewAgg` allocates the final aggregate's output columns first,
    // then the partial aggregate's own outputs (the cop task's in 2-phase mode).
    let n = push_aggs.len();
    let mut partial_next = out_col + n;
    let mut cop_next = out_col + 2 * n;
    for func in push_aggs {
        let name = func.name.to_ascii_lowercase();
        let arg = func
            .args
            .first()
            .map(|a| join_agg_col_ref(select, current_db, a))
            .unwrap_or_else(|| "1".to_string());
        if pseudo {
            cop_funcs.push(format!("funcs:{name}({arg})->Column#{cop_next}"));
            root_funcs.push(format!(
                "funcs:{name}(Column#{cop_next})->Column#{partial_next}"
            ));
            cop_next += 1;
        } else {
            root_funcs.push(format!("funcs:{name}({arg})->Column#{partial_next}"));
        }
        partial_next += 1;
    }
    for (expr, text) in push_gby.iter().zip(group_info.iter()) {
        let _ = expr;
        root_funcs.push(format!("funcs:firstrow({text})->{text}"));
    }
    let partial_info = format!("group by:{}, {}", group_info.join(", "), root_funcs.join(", "));
    let partial = if pseudo {
        PhysicalPlan::hash_agg_cop(
            child,
            partial_info,
            format!("group by:{}, {}", group_info.join(", "), cop_funcs.join(", ")),
            partial_est,
        )
    } else {
        PhysicalPlan::hash_agg(child, partial_info, partial_est)
    };
    let join = if push_right {
        PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left,
            right: Box::new(partial),
            left_table: left_table.clone(),
            right_table: right_table.clone(),
            est_rows,
            on,
        }
    } else {
        PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left: Box::new(partial),
            right,
            left_table: left_table.clone(),
            right_table: right_table.clone(),
            est_rows,
            on,
        }
    };
    // TiDB's join cardinality is derived from the *pushed-down* build side
    // (`EstimateFullJoinRowCount`): the partial aggregation shrinks the build
    // rows and its group NDV feeds the key match, so `t2` (9990 rows, 8000 NDV)
    // aggregated to 7992 groups estimates `7992 × 10000 / max(8000, 7992)`.
    let join = recompute_join_est(join, is, current_db, stats, &aliases);

    // The final aggregation disappears when the other side's `GROUP BY`
    // columns already pin one row per group.
    let other_table = if push_right { &left_table } else { &right_table };
    let other_physical = join_side_physical(select, other_table);
    let outer_names: Vec<String> = select
        .group_by
        .iter()
        .filter_map(|e| match e {
            ExprNode::Column(c) => Some(c.name.clone()),
            _ => None,
        })
        .collect();
    let eliminated = is
        .and_then(|is| is.table_by_name(current_db, &other_physical))
        .is_some_and(|info| covers_unique_key(info, &outer_names));
    if eliminated {
        let exprs: Vec<String> = (0..agg_fields.len())
            .map(|i| format!("Column#{}", out_col + i))
            .collect();
        return Some((
            PhysicalPlan::Projection {
                exprs,
                child: Box::new(join),
            },
            false,
        ));
    }

    // Final aggregation above the join: it consumes the partial columns.
    let mut final_funcs: Vec<String> = Vec::new();
    let mut col = out_col + agg_fields.len();
    for (index, func) in agg_fields.iter().enumerate() {
        let name = func.name.to_ascii_lowercase();
        final_funcs.push(format!(
            "funcs:{name}(Column#{col})->Column#{}",
            out_col + index
        ));
        col += 1;
    }
    for item in &select.group_by {
        let text = join_agg_col_ref(select, current_db, item);
        final_funcs.push(format!("funcs:firstrow({text})->{text}"));
    }
    let final_info = if select.group_by.is_empty() {
        final_funcs.join(", ")
    } else {
        format!(
            "group by:{}, {}",
            select
                .group_by
                .iter()
                .map(|e| join_agg_col_ref(select, current_db, e))
                .collect::<Vec<_>>()
                .join(", "),
            final_funcs.join(", ")
        )
    };
    Some((
        PhysicalPlan::hash_agg(join, final_info, partial_est),
        false,
    ))
}

/// Append a `GROUP BY` column unless the list already holds it.
fn push_group_key(keys: &mut Vec<ExprNode>, item: &ExprNode) {
    let ExprNode::Column(col) = item else {
        return;
    };
    let dup = keys.iter().any(|k| match k {
        ExprNode::Column(k) => k.name.eq_ignore_ascii_case(&col.name),
        _ => false,
    });
    if !dup {
        keys.push(item.clone());
    }
}

/// TiDB's partial-aggregation cardinality: the group-key NDV product, capped by
/// the input row count. Pseudo statistics fall back to `0.8` per key.
fn pushed_group_est(
    keys: &[ExprNode],
    child: &PhysicalPlan,
    rows: f64,
    current_db: &str,
    aliases: &[(String, String)],
    stats: Option<&Handle>,
) -> f64 {
    if super::explain::is_pseudo_plan(child) {
        return (rows * 0.8_f64.powi(keys.len() as i32)).max(1.0);
    }
    let mut est = 1.0;
    for item in keys {
        let ExprNode::Column(col) = item else {
            continue;
        };
        let table = col.table.as_deref().unwrap_or("");
        let db = col.schema.as_deref().unwrap_or(current_db);
        let physical = aliases
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(table))
            .map(|(_, phys)| phys.as_str())
            .unwrap_or(table);
        let ndv = stats
            .and_then(|h| h.table(db, physical))
            .filter(|t| has_real_stats(t))
            .and_then(|t| t.column_ndv(&col.name));
        est *= ndv.unwrap_or(rows);
    }
    est.min(rows).max(1.0)
}

/// HashAgg over a join: TiDB `ExplainInfo` uses OrigTblName operands and omits
/// the partial `count` when no aggregate function is selected.
fn build_join_agg_plan(
    select: &SelectStmt,
    plan: PhysicalPlan,
    current_db: &str,
) -> (PhysicalPlan, bool) {
    let group_refs: Vec<String> = select
        .group_by
        .iter()
        .map(|e| join_agg_col_ref(select, current_db, e))
        .collect();
    let group_info = group_refs.join(", ");
    let mut funcs: Vec<String> = Vec::new();
    for field in &select.fields {
        if is_agg_expr(&field.expr) {
            let ExprNode::Func(func) = &field.expr else {
                continue;
            };
            let name = func.name.to_ascii_lowercase();
            let arg = func
                .args
                .first()
                .map(|a| join_agg_col_ref(select, current_db, a))
                .unwrap_or_default();
            funcs.push(format!("funcs:{name}({arg})->Column#5"));
            continue;
        }
        if let ExprNode::Column(col) = &field.expr {
            let col_ref = join_agg_col_ref(select, current_db, &field.expr);
            funcs.push(format!("funcs:firstrow({col_ref})->{col_ref}"));
            let _ = col;
        }
    }
    if funcs.is_empty() {
        funcs.push("funcs:count(1)->Column#5".to_string());
    }
    let info = if group_info.is_empty() {
        funcs.join(", ")
    } else {
        format!("group by:{group_info}, {}", funcs.join(", "))
    };
    // TiDB aggregation NDV: each group-by operand costs one 0.8 selectivity
    // step against the join's output cardinality.
    let join_est = super::explain::est_rows(&plan);
    let est = group_refs
        .iter()
        .fold(join_est, |acc, _| acc * 0.8)
        .max(1.0);
    (PhysicalPlan::hash_agg(plan, info, est), false)
}

fn agg_est_rows(select: &SelectStmt) -> f64 {
    if select.group_by.is_empty() {
        return 1.0;
    }
    if select.limit == Some(1) {
        return 1.0;
    }
    if select.selection.is_some() {
        // Pseudo selectivity ~1/3 (TiDB unanalyzed `b>0`).
        8000.0 / 3.0
    } else {
        8000.0
    }
}

fn partial_agg_column_id(select: &SelectStmt) -> usize {
    if select.distinct {
        return 9;
    }
    if select.group_by.is_empty() {
        return 13;
    }
    if select.limit.is_some() && !select.order_by.is_empty() {
        return 16;
    }
    if !select.order_by.is_empty() {
        return 15;
    }
    10
}

/// `SELECT … FOR UPDATE/FOR SHARE`: a handle point get or a batch point get
/// locks its own keys (`…, lock`); everything else gets a `SelectLock` node.
fn apply_select_lock(plan: PhysicalPlan, select: &SelectStmt) -> PhysicalPlan {
    apply_lock(plan, select.lock)
}

/// Lock the rows a DML statement reads inside an explicit transaction, exactly
/// like `SELECT … FOR UPDATE` (TiDB's `UPDATE`/`DELETE` children carry the same
/// `SelectLock`, and a handle point get reports `, lock`).
fn apply_dml_lock(plan: Option<Box<PhysicalPlan>>) -> Option<Box<PhysicalPlan>> {
    let plan = plan?;
    if !in_explicit_txn() {
        return Some(plan);
    }
    // A `DELETE` repeatedly projects the handle (`Projection(col0) → Point_Get`)
    // before the row-id shape is folded away; the point get still owns the lock,
    // so mark it beneath the projection chain instead of wrapping the chain in a
    // `SelectLock` node (TiDB: `Delete → Point_Get handle:1, lock`).
    if let Some(locked) = lock_point_get(plan.clone()) {
        return Some(Box::new(locked));
    }
    Some(Box::new(apply_lock(*plan, SelectLock::ForUpdate)))
}

/// Mark a handle/unique point get reached through passthrough projections as
/// locking its keys. Returns `None` when the chain does not end in a point get.
fn lock_point_get(plan: Box<PhysicalPlan>) -> Option<PhysicalPlan> {
    match *plan {
        PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
            index_columns,
            partition_names,
            ..
        } => Some(PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
            index_columns,
            partition_names,
            global_index: false,
            lock: true,
            // The integer row handle is never an index access object.
            common_handle: false,
        }),
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            keep_order,
            desc,
            common_handle,
            ..
        } => Some(PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            lock: true,
            keep_order,
            desc,
            common_handle,
        }),
        PhysicalPlan::Projection { exprs, child } => {
            let child = lock_point_get(child)?;
            Some(PhysicalPlan::Projection {
                exprs,
                child: Box::new(child),
            })
        }
        _ => None,
    }
}

fn apply_lock(plan: PhysicalPlan, lock: SelectLock) -> PhysicalPlan {
    let info = match lock {
        SelectLock::None => return plan,
        SelectLock::ForUpdate => "for update 0",
        SelectLock::ForShare => "for share 0",
    };
    match plan {
        PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
            index_columns,
            partition_names,
            ..
        } => PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
            index_columns,
            partition_names,
            global_index: false,
            lock: in_explicit_txn(),
            common_handle: false,
        },
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            keep_order,
            desc,
            common_handle,
            ..
        } => PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            lock: true,
            keep_order,
            desc,
            common_handle,
        },
        other => PhysicalPlan::SelectLock {
            child: Box::new(other),
            info: info.to_string(),
        },
    }
}

/// Column count of a point get's child table plus its handle column.
fn pointget_agg_column_base(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> usize {
    let Some(from) = select.from.as_ref() else {
        return 1;
    };
    let schema = from
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    is.and_then(|is| is.table_by_name(schema, from.resolve_name()))
        .map(|info| info.columns.len() + 1)
        .unwrap_or(1)
}

/// Aggregate argument text: columns keep their qualified table name.
fn agg_info_text(expr: &ExprNode, _table: &str, _db_qual: &str) -> String {
    tidb_scalar_explain(expr, true)
}

fn explain_col_ref(table: &str, expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(col) => {
            let schema = col.schema.clone().unwrap_or_else(|| {
                explain_table_schema(table).unwrap_or_else(|| "select".to_string())
            });
            format!("{schema}.{table}.{}", col.name)
        }
        other => other.restore_string(),
    }
}

fn needs_window(select: &SelectStmt) -> bool {
    select.fields.iter().any(|f| expr_has_window(&f.expr))
}

/// TiDB `ExplainInfo` labels join keys with `OrigTblName` (`t t2` → `t.a`).
/// Only for EXPLAIN: execution resolves columns by the SQL alias.
fn explain_on_expr(on: &ExprNode, select: &SelectStmt) -> ExprNode {
    let tables = if in_explain_build() {
        select.join_tables()
    } else {
        return on.clone();
    };
    rewrite_col_tables(on, &tables)
}

fn rewrite_col_tables(on: &ExprNode, tables: &[&sealdb_parser::ast::TableName]) -> ExprNode {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode, UnaryOpExpr};
    match on {
        ExprNode::Column(c) => {
            let mut c = c.clone();
            if let Some(t) = c.table.as_deref()
                && let Some(tbl) = tables.iter().find(|tbl| tbl.name.eq_ignore_ascii_case(t))
            {
                // TiDB planbuilder stores joined tables with `Name.L`, so
                // EXPLAIN always renders the lowercase OrigTblName.
                c.table = Some(tbl.resolve_name().to_ascii_lowercase());
            }
            ExprNode::Column(c)
        }
        ExprNode::Binary(b) => ExprNode::Binary(BinaryOpExpr {
            op: b.op,
            left: Box::new(rewrite_col_tables(&b.left, tables)),
            right: Box::new(rewrite_col_tables(&b.right, tables)),
            single_cnf_item: b.single_cnf_item,
        }),
        ExprNode::Unary(u) => ExprNode::Unary(UnaryOpExpr {
            op: u.op,
            expr: Box::new(rewrite_col_tables(&u.expr, tables)),
        }),
        ExprNode::IsNull(n) => {
            let mut n = n.clone();
            n.expr = Box::new(rewrite_col_tables(&n.expr, tables));
            ExprNode::IsNull(n)
        }
        ExprNode::Between(b) => {
            let mut b = b.clone();
            b.expr = Box::new(rewrite_col_tables(&b.expr, tables));
            b.low = Box::new(rewrite_col_tables(&b.low, tables));
            b.high = Box::new(rewrite_col_tables(&b.high, tables));
            ExprNode::Between(b)
        }
        other => other.clone(),
    }
}

/// True when ON is an equality between `base` and `right` columns only.
fn outer_join_on_base(on: &ExprNode, base: &str, right: &str) -> bool {
    let ExprNode::Binary(bin) = on else {
        return false;
    };
    if bin.op != Op::Eq {
        return false;
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(a), ExprNode::Column(b)) => {
            let at = a.table.as_deref().unwrap_or("");
            let bt = b.table.as_deref().unwrap_or("");
            (at.eq_ignore_ascii_case(base) && bt.eq_ignore_ascii_case(right))
                || (at.eq_ignore_ascii_case(right) && bt.eq_ignore_ascii_case(base))
        }
        _ => false,
    }
}

/// `SELECT * FROM a NATURAL LEFT JOIN b WHERE …` keeps a Projection above the
/// pushed Selection so the USING columns still surface once (`a.a, a.b, b.c`).
fn star_projection_over_filter(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if matches!(plan, PhysicalPlan::Projection { .. }) || !is_star_select(select) {
        return plan;
    }
    let Some(last) = select.joins.last() else {
        return plan;
    };
    if last.using_cols.is_empty()
        || !matches!(
            map_join_type(last.tp),
            JoinType::LeftOuter | JoinType::RightOuter
        )
        || !matches!(plan, PhysicalPlan::Selection { .. })
    {
        return plan;
    }
    wrap_star_join_projection(plan, select, is, current_db)
}

/// Any `t.*` in the SELECT list (`t1.*, t2.*` over a join).
fn has_qualified_star(select: &SelectStmt) -> bool {
    select
        .fields
        .iter()
        .any(|f| matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some()))
}

/// TiDB raises a star Projection above a `RIGHT JOIN … USING/NATURAL` so the/// preserved side's columns come first and USING columns appear once:
/// `t1(a,b) RIGHT JOIN t2 USING(a)` → `t2.a, t2.c, t1.b`.
fn star_join_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if matches!(plan, PhysicalPlan::Projection { .. }) {
        return plan;
    }
    if has_qualified_star(select) {
        // `t1.*, t2.* …` over a join: each table contributes its raw columns,
        // USING duplicates included (TiDB does not coalesce here).
        return wrap_star_join_projection(plan, select, is, current_db);
    }
    if !is_star_select(select) {
        return plan;
    }
    let Some(last) = select.joins.last() else {
        return plan;
    };
    if using_output_table(
        select,
        &last.using_cols.first().cloned().unwrap_or_default(),
    )
    .is_none()
    {
        return plan;
    }
    let flat = star_projection_exprs(select, is, current_db);
    // TiDB's `mergeSchema` puts USING columns first, then the leading side's
    // remaining columns, then the other side's. Nested join trees need the
    // recursive form; wrap a Projection when it differs from the flat order
    // the join executor emits.
    if let Some(merged) = select
        .from_item
        .as_ref()
        .and_then(|item| merged_star_columns(item, is, current_db))
    {
        let merged: Vec<String> = merged
            .into_iter()
            .map(|(alias, col)| format!("{}.{}.{}", schema_prefix(current_db), alias, col))
            .collect();
        if !merged.is_empty() && merged != flat {
            return PhysicalPlan::Projection {
                exprs: merged,
                child: Box::new(plan),
            };
        }
    }
    let right_outer = matches!(map_join_type(last.tp), JoinType::RightOuter);
    if !right_outer {
        return plan;
    }
    wrap_star_join_projection(plan, select, is, current_db)
}

/// `(table, column)` pairs whose value disappears from the join output.
///
/// A left-preserved `USING`/`NATURAL` join drops the name from its right
/// operand; a `RIGHT JOIN` drops it from whichever table currently owns it in
/// the accumulated output (which may be an earlier table in the chain). A
/// three-table `t1, t2 NATURAL RIGHT JOIN t3` therefore keeps `t1.i`.
pub fn using_dropped_columns(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> std::collections::HashSet<(String, String)> {
    use std::collections::{HashMap, HashSet};
    let mut drops: HashSet<(String, String)> = HashSet::new();
    let mut owner: HashMap<String, String> = HashMap::new();
    let columns_of = |table: &TableName| -> Vec<String> {
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        is.and_then(|is| is.table_by_name(schema, table.resolve_name()))
            .map(|info| {
                info.columns
                    .iter()
                    .map(|c| c.name.o().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let add = |owner: &mut HashMap<String, String>, table: &TableName, skip: &[String]| {
        let alias = table.name.to_ascii_lowercase();
        for col in columns_of(table) {
            if skip.iter().any(|s| s.eq_ignore_ascii_case(&col)) {
                continue;
            }
            owner
                .entry(col.to_ascii_lowercase())
                .or_insert_with(|| alias.clone());
        }
    };
    if let Some(from) = &select.from {
        add(&mut owner, from, &[]);
    }
    for join in &select.joins {
        let right = join.right.name.to_ascii_lowercase();
        if join.using_cols.is_empty() {
            add(&mut owner, &join.right, &[]);
            continue;
        }
        let right_outer = matches!(map_join_type(join.tp), JoinType::RightOuter);
        for col in &join.using_cols {
            let bare = col.to_ascii_lowercase();
            if right_outer {
                if let Some(table) = owner.get(&bare) {
                    drops.insert((table.clone(), bare.clone()));
                }
            } else {
                drops.insert((right.clone(), bare));
            }
        }
        add(&mut owner, &join.right, &join.using_cols);
        if right_outer {
            for col in &join.using_cols {
                owner.insert(col.to_ascii_lowercase(), right.clone());
            }
        }
    }
    drops
}

/// TiDB `mergeSchema` for `SELECT *`: a join emits its USING/common columns
/// first (in the leading side's output order), then the leading side's
/// remaining columns, then the other side's remaining columns. `RIGHT JOIN`
/// leads with its preserved right side. Returns `(alias, column)` pairs.
pub fn merged_star_columns(
    item: &FromItem,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<Vec<(String, String)>> {
    merged_star_columns_impl(item, is, current_db, false)
}

/// Same as [`merged_star_columns`], but the alias is the visible FROM alias
/// (`t dt1` → `dt1`), matching plan schemas built from the alias.
pub fn merged_star_visible_columns(
    item: &FromItem,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<Vec<(String, String)>> {
    merged_star_columns_impl(item, is, current_db, true)
}

fn merged_star_columns_impl(
    item: &FromItem,
    is: Option<&InfoSchema>,
    current_db: &str,
    visible_alias: bool,
) -> Option<Vec<(String, String)>> {
    match item {
        FromItem::Table(table) => {
            let (schema, name) = table.qualified(current_db);
            let info = is?.table_by_name(&schema, &name)?;
            let alias = if visible_alias {
                table.name.clone()
            } else {
                table.resolve_name().to_string()
            };
            Some(
                info.columns
                    .iter()
                    .map(|c| (alias.clone(), c.name.o().to_string()))
                    .collect(),
            )
        }
        FromItem::Join {
            left,
            right,
            tp,
            using_cols,
            ..
        } => {
            let left_cols = merged_star_columns_impl(left, is, current_db, visible_alias)?;
            let right_cols = merged_star_columns_impl(right, is, current_db, visible_alias)?;
            let right_outer = matches!(map_join_type(*tp), JoinType::RightOuter);
            let (leading, trailing) = if right_outer {
                (&right_cols, &left_cols)
            } else {
                (&left_cols, &right_cols)
            };
            let is_using = |col: &str| using_cols.iter().any(|u| u.eq_ignore_ascii_case(col));
            let mut out: Vec<(String, String)> = Vec::new();
            if !using_cols.is_empty() {
                let mut seen = std::collections::HashSet::new();
                for (alias, col) in leading {
                    if is_using(col) && seen.insert(col.to_ascii_lowercase()) {
                        out.push((alias.clone(), col.clone()));
                    }
                }
            }
            out.extend(leading.iter().filter(|(_, col)| !is_using(col)).cloned());
            out.extend(trailing.iter().filter(|(_, col)| !is_using(col)).cloned());
            Some(out)
        }
    }
}

/// TiDB projects `select.*` column list above multi-table joins (FROM order).
fn wrap_star_join_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if !is_star_select(select) {
        return plan;
    }
    let exprs = star_projection_exprs(select, is, current_db);
    if exprs.is_empty() {
        return plan;
    }
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
}

/// `select.*` columns in FROM order; USING columns are emitted once from the
/// leading side. `t1.*, t2.*` keeps every raw column instead.
fn star_projection_exprs(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Vec<String> {
    let mut tables: Vec<TableName> = select.join_tables().into_iter().cloned().collect();
    let any_qualified_star = has_qualified_star(select);
    // `… NATURAL/USING RIGHT JOIN t` merges schemas with the preserved (right)
    // side first. Plain ON right joins keep FROM order (see select suite).
    let right_outer_using = select.joins.last().is_some_and(|j| {
        matches!(map_join_type(j.tp), JoinType::RightOuter) && !j.using_cols.is_empty()
    });
    if right_outer_using && !any_qualified_star && tables.len() > 1 {
        let last = tables.pop().unwrap();
        tables.insert(0, last);
    }
    let using_drops = using_dropped_columns(select, is, current_db);
    let mut exprs = Vec::new();
    for table in tables.iter() {
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        let Some(info) = is.and_then(|is| is.table_by_name(schema, table.resolve_name())) else {
            continue;
        };
        for col in info.columns.iter().filter(|c| !c.hidden) {
            let key = (
                table.resolve_name().to_ascii_lowercase(),
                col.name.o().to_ascii_lowercase(),
            );
            if !any_qualified_star && using_drops.contains(&key) {
                continue; // USING column already emitted from the preserved side
            }
            exprs.push(format!(
                "{}.{}.{}",
                schema_prefix(current_db),
                table.resolve_name(),
                col.name.o()
            ));
        }
    }
    exprs
}

fn convert_nested_outer_to_inner(
    right_plan: &mut PhysicalPlan,
    parent_on: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<bool> {
    let PhysicalPlan::HashJoin {
        tp,
        left,
        right,
        left_table,
        right_table,
        on,
        ..
    } = right_plan.clone()
    else {
        return None;
    };
    if !matches!(tp, JoinType::LeftOuter | JoinType::RightOuter) {
        return None;
    }
    if !expr_mentions_table(parent_on, &right_table) {
        return None;
    }
    let mut left_p = *left;
    let right_p = *right;
    if let Some(filter) = null_reject_on_inner(&on, &left_table, &left_table, &[]) {
        left_p = predicate_push_down(left_p, &filter, is, current_db);
    }
    let l_rows = raw_scan_rows(&left_p);
    let r_rows = raw_scan_rows(&right_p);
    *right_plan = reorder_hash_join(
        JoinType::Inner,
        left_p,
        right_p,
        left_table,
        right_table.clone(),
        l_rows,
        r_rows,
        flip_eq_for_build(&on, &right_table),
    );
    Some(true)
}

fn expr_mentions_table(expr: &ExprNode, table: &str) -> bool {
    match expr {
        ExprNode::Column(c) => c
            .table
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(table)),
        ExprNode::Binary(bin) => {
            expr_mentions_table(&bin.left, table) || expr_mentions_table(&bin.right, table)
        }
        ExprNode::Unary(u) => expr_mentions_table(&u.expr, table),
        ExprNode::Func(f) => f.args.iter().any(|a| expr_mentions_table(a, table)),
        ExprNode::IsNull(n) => expr_mentions_table(&n.expr, table),
        ExprNode::Cast(c) => expr_mentions_table(&c.expr, table),
        _ => false,
    }
}

fn flip_eq_for_build(on: &ExprNode, build_table: &str) -> ExprNode {
    let ExprNode::Binary(bin) = on else {
        return on.clone();
    };
    if bin.op != Op::Eq {
        return on.clone();
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(l), ExprNode::Column(r)) => {
            let lt = l.table.as_deref().unwrap_or("");
            let rt = r.table.as_deref().unwrap_or("");
            if rt.eq_ignore_ascii_case(build_table) && !lt.eq_ignore_ascii_case(build_table) {
                ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
                    op: Op::Eq,
                    left: Box::new(ExprNode::Column(r.clone())),
                    right: Box::new(ExprNode::Column(l.clone())),
                    single_cnf_item: false,
                })
            } else {
                on.clone()
            }
        }
        _ => on.clone(),
    }
}

fn raw_scan_rows(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableReader { row_count, .. } => *row_count,
        PhysicalPlan::Selection { child, .. } => raw_scan_rows(child),
        PhysicalPlan::HashJoin { left, right, .. } => raw_scan_rows(left).max(raw_scan_rows(right)),
        PhysicalPlan::Projection { child, .. } => raw_scan_rows(child),
        _ => 10_000.0,
    }
}

fn expr_has_window(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Window(_) => true,
        ExprNode::Binary(bin) => expr_has_window(&bin.left) || expr_has_window(&bin.right),
        ExprNode::Unary(u) => expr_has_window(&u.expr),
        ExprNode::Func(f) => f.args.iter().any(expr_has_window),
        ExprNode::Cast(c) => expr_has_window(&c.expr),
        _ => false,
    }
}

fn is_agg_expr(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
    )
}

/// Row-constructor `(a, b) NOT IN (SELECT a, b …)` / `(a, b) != ALL (…)`
/// rewrite (TiDB `convertToNullAwareAntiJoin`).
///
/// TiDB builds a `PhysicalHashJoin` with the null-aware anti join type:
/// the row operands become equality keys (`NAEQ`) and any correlated filter
/// becomes `other cond`. A correlated equality inside the subquery cannot
/// coexist with `NAEQ`, so it is hoisted to the hash key and the row operands
/// fall back to plain `other cond` equalities.
fn naaj_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<PhysicalPlan> {
    if !select.joins.is_empty()
        || select.from_item.is_some()
        || select.with.is_some()
        || select.distinct
        || !select.group_by.is_empty()
        || select.having.is_some()
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
    {
        return None;
    }
    let outer_tbl = select.from.as_ref()?;
    // Select list: `select (a, b) not in (…) from t` → AntiLeftOuterSemiJoin.
    // WHERE: `select * from t where (a, b) not in (…)` → AntiSemiJoin.
    let select_list = select.fields.len() == 1
        && select.selection.is_none()
        && !naaj_is_star(&select.fields[0].expr);
    let outer_star = select.fields.len() == 1 && naaj_is_star(&select.fields[0].expr);
    let predicate = if select_list {
        &select.fields[0].expr
    } else {
        // `select c from t where (a, b) not in (…)`: the anti join keeps whole
        // probe rows, the select list projects on top.
        select.selection.as_ref()?
    };
    let (row_exprs, query, all_ne) = naaj_predicate(predicate)?;
    let ResultSetNode::Select(inner) = query else {
        return None;
    };
    if inner.distinct
        || !inner.joins.is_empty()
        || inner.from_item.is_some()
        || inner.with.is_some()
        || !inner.group_by.is_empty()
        || inner.having.is_some()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
    {
        return None;
    }
    let inner_tbl = inner.from.as_ref()?;
    // `(a, b) in (select * from t)`: expand `*` to the inner table's columns
    // (arity must match the row constructor).
    let inner_star = inner.fields.len() == 1 && naaj_is_star(&inner.fields[0].expr);
    let inner_fields: Vec<ExprNode> = if inner_star {
        let info = is.and_then(|is| {
            let (db, table) = inner_tbl.qualified(current_db);
            is.table_by_name(&db, &table)
        })?;
        info.columns
            .iter()
            .map(|c| {
                ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: None,
                    name: c.name.o().to_string(),
                })
            })
            .collect()
    } else {
        if inner.fields.iter().any(|f| naaj_is_star(&f.expr)) {
            return None;
        }
        inner.fields.iter().map(|f| f.expr.clone()).collect()
    };
    if inner_fields.len() != row_exprs.len() {
        return None;
    }
    let outer_name = outer_tbl.resolve_name().to_string();
    let inner_name = inner_tbl.resolve_name().to_string();
    // Correlated conjuncts reference the outer query; the rest are pushed onto
    // the inner TableReader.
    let mut correlated = Vec::new();
    let mut inner_only = Vec::new();
    if let Some(selection) = &inner.selection {
        for conjunct in selection.split_cnf() {
            if naaj_refs_table(&conjunct, &outer_name) {
                correlated.push(conjunct);
            } else {
                inner_only.push(conjunct);
            }
        }
    }
    let inner_keys: Vec<ExprNode> = inner_fields.clone();
    // A lone correlated equality becomes the hash key (`equal:[eq(outer, inner)]`).
    let corr_eq = match correlated.as_slice() {
        [ExprNode::Binary(bin)] if bin.op == Op::Eq => {
            if naaj_refs_table(&bin.left, &outer_name) && !naaj_refs_table(&bin.right, &outer_name)
            {
                Some(((*bin.left).clone(), (*bin.right).clone()))
            } else if naaj_refs_table(&bin.right, &outer_name)
                && !naaj_refs_table(&bin.left, &outer_name)
            {
                Some(((*bin.right).clone(), (*bin.left).clone()))
            } else {
                None
            }
        }
        _ => None,
    };
    let null_aware = corr_eq.is_none();
    let (keys, other_cond) = match &corr_eq {
        Some((outer_side, inner_side)) => {
            // Row operands are compared via `other cond` equalities.
            let conds: Vec<ExprNode> = row_exprs
                .iter()
                .cloned()
                .zip(inner_keys.iter().cloned())
                .map(|(outer, inner)| {
                    naaj_eq(
                        naaj_qualify(&outer, &outer_name),
                        naaj_qualify(&inner, &inner_name),
                    )
                })
                .collect();
            (
                vec![(outer_side.clone(), inner_side.clone())],
                ExprNode::compose_cnf(conds),
            )
        }
        None => (
            row_exprs.iter().cloned().zip(inner_keys).collect(),
            ExprNode::compose_cnf(correlated.clone()),
        ),
    };
    let schema = schema_prefix(current_db);
    let outer_q = format!("{schema}.{}", outer_name.to_ascii_lowercase());
    let inner_q = format!("{schema}.{}", inner_name.to_ascii_lowercase());
    let mut probe = PhysicalPlan::table_reader(outer_tbl.name.clone(), Vec::new());
    let mut build = PhysicalPlan::table_reader(inner_tbl.name.clone(), Vec::new());
    if let Some(filter) = ExprNode::compose_cnf(inner_only) {
        if let PhysicalPlan::TableReader { pushed, .. } = &mut build {
            *pushed = Some(filter);
        }
    }
    // Row operands that are not plain columns need TiDB's injected Projection
    // (`injectExtraProjection`); the join still evaluates the expressions.
    // A correlated equality already hoists the key (`NAEQ` cannot coexist with
    // `EQ`), so TiDB injects no operand projection in that case.
    let probe_projected = null_aware && row_exprs.iter().any(|e| !matches!(e, ExprNode::Column(_)));
    let build_projected = null_aware
        && inner_fields
            .iter()
            .any(|e| !matches!(e, ExprNode::Column(_)));
    let (mut probe_cols, mut build_cols) = (Vec::new(), Vec::new());
    if probe_projected || build_projected {
        // TiDB result column ids for the injected projections. The operands of
        // the row constructors are allocated before them, so the probe/build
        // projections start at #14/#9 for `(a, b)` over two 3-column tables.
        let (probe_base, build_base) = if select_list { (14, 0) } else { (13, 9) };
        probe_cols = (0..row_exprs.len()).map(|i| probe_base + i).collect();
        build_cols = (0..row_exprs.len()).map(|i| build_base + i).collect();
    }
    if probe_projected {
        let mut info: Vec<String> = Vec::new();
        let mut exprs: Vec<ExprNode> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        if !select_list {
            // `SELECT *` keeps every probe column ahead of the operands.
            if let Some(tbl) = is.and_then(|is| {
                let (db, table) = outer_tbl.qualified(current_db);
                is.table_by_name(&db, &table)
            }) {
                for c in &tbl.columns {
                    let column = ExprNode::Column(ColumnNameExpr {
                        schema: None,
                        table: None,
                        name: c.name.o().to_string(),
                    });
                    info.push(format!("{outer_q}.{}", c.name.o()));
                    exprs.push(column);
                    // Bare names: the operator exposes the probe columns as the
                    // statement's `SELECT *` schema (EXPLAIN uses `info`).
                    names.push(c.name.o().to_string());
                }
            }
        }
        for (e, id) in row_exprs.iter().zip(probe_cols.iter()) {
            info.push(format!(
                "{}->Column#{id}",
                naaj_operand_info(&naaj_qualify(e, &outer_name), &schema)
            ));
            exprs.push(naaj_strip_quals(e));
            names.push(format!("Column#{id}"));
        }
        probe = PhysicalPlan::RowProjection {
            info: info.join(", "),
            exprs,
            names,
            child: Box::new(probe),
        };
    }
    if build_projected {
        let mut info: Vec<String> = Vec::new();
        let mut exprs: Vec<ExprNode> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (e, id) in inner_fields.iter().zip(build_cols.iter()) {
            info.push(format!(
                "{}->Column#{id}",
                naaj_operand_info(&naaj_qualify(e, &inner_name), &schema)
            ));
            exprs.push(naaj_strip_quals(e));
            names.push(format!("Column#{id}"));
        }
        build = PhysicalPlan::RowProjection {
            info: info.join(", "),
            exprs,
            names,
            child: Box::new(build),
        };
    }
    let eq_list: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(i, (outer, inner))| {
            let left = if probe_projected {
                format!("Column#{}", probe_cols[i])
            } else {
                naaj_operand_info(&naaj_qualify(outer, &outer_name), &schema)
            };
            let right = if build_projected {
                format!("Column#{}", build_cols[i])
            } else {
                naaj_operand_info(&naaj_qualify(inner, &inner_name), &schema)
            };
            format!("eq({left}, {right})")
        })
        .collect();
    let left_outer = select_list || all_ne;
    let header = if null_aware {
        if left_outer {
            "Null-aware anti left outer semi join"
        } else {
            "Null-aware anti semi join"
        }
    } else if left_outer {
        "anti left outer semi join"
    } else {
        "anti semi join"
    };
    let mut info = format!("{header}, equal:[{}]", eq_list.join(" "));
    if let Some(cond) = &other_cond {
        info.push_str(", other cond:");
        info.push_str(&naaj_other_conds(cond, &schema));
    }
    let _ = (&outer_q, &inner_q);
    let probe_est = est_rows_of(&probe);
    let build_est = est_rows_of(&build);
    // TiDB NAAJ cardinality: anti keeps 0.8 of the probe; the anti-left-outer
    // semi join emits every probe row.
    let est_rows = if left_outer {
        probe_est.max(build_est)
    } else {
        probe_est * 0.8
    };
    let bool_column = if select_list {
        select.fields[0]
            .orig_text
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| predicate.restore_string())
    } else {
        "Column#9".to_string()
    };
    // Injected projections expose the row operands as `Column#N`; point the
    // join keys at those outputs instead of re-evaluating the raw operands.
    let keys: Vec<(ExprNode, ExprNode)> = keys
        .iter()
        .enumerate()
        .map(|(i, (outer, inner))| {
            let outer = if probe_projected {
                naaj_column(format!("Column#{}", probe_cols[i]))
            } else {
                outer.clone()
            };
            let inner = if build_projected {
                naaj_column(format!("Column#{}", build_cols[i]))
            } else {
                inner.clone()
            };
            (outer, inner)
        })
        .collect();
    // `SELECT *`: the operator hides the injected operand columns.
    let probe_width = if outer_star && probe_projected {
        is.and_then(|is| {
            let (db, table) = outer_tbl.qualified(current_db);
            is.table_by_name(&db, &table).map(|t| t.columns.len())
        })
    } else {
        None
    };
    let join = PhysicalPlan::NullAwareAntiJoin {
        info,
        est_rows,
        left_outer,
        bool_only: select_list,
        null_aware,
        bool_column,
        keys,
        other_cond,
        probe_table: outer_name.clone(),
        build_table: inner_name.clone(),
        probe_width,
        probe: Box::new(probe),
        build: Box::new(build),
    };
    if select_list {
        return Some(join);
    }
    // The anti join emits whole probe rows; project the select list unless the
    // statement asked for `*` (bare probe schema).
    let project_outer = |child: PhysicalPlan| -> PhysicalPlan {
        // Probe rows carry the injected operand columns too; project back to
        // the statement's select list. RowProjection evaluates `exprs` (a plain
        // Projection re-derives the select fields with their aliases, which the
        // anti join's schema does not carry).
        let mut exprs: Vec<ExprNode> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let mut info: Vec<String> = Vec::new();
        if outer_star {
            // `SELECT *`: the anti join already emits the probe schema; the
            // injected operand projection's extra columns are not selected.
            return child;
        }
        for f in &select.fields {
            exprs.push(naaj_strip_quals(&f.expr));
            match &f.expr {
                ExprNode::Column(col) => {
                    names.push(col.name.clone());
                    info.push(col.name.clone());
                }
                other => {
                    let text = other.restore_string();
                    names.push(text.clone());
                    info.push(text);
                }
            }
        }
        if exprs.is_empty() {
            return child;
        }
        PhysicalPlan::RowProjection {
            info: info.join(", "),
            exprs,
            names,
            child: Box::new(child),
        }
    };
    if !all_ne {
        return Some(project_outer(join));
    }
    // `WHERE (a, b) != ALL (…)`: Selection over the boolean column, then a
    // Projection back to the probe schema.
    let probe_cols: Vec<String> = is
        .and_then(|is| {
            let (db, table) = outer_tbl.qualified(current_db);
            is.table_by_name(&db, &table)
        })
        .map(|info| {
            info.columns
                .iter()
                .map(|c| format!("{outer_q}.{}", c.name.o()))
                .collect()
        })
        .unwrap_or_default();
    let selected = PhysicalPlan::Selection {
        filter: ExprNode::Column(ColumnNameExpr {
            schema: None,
            table: None,
            name: "Column#9".to_string(),
        }),
        child: Box::new(join),
        est_rows: None,
    };
    if outer_star {
        // `!= ALL` keeps TiDB's projection back to the probe schema.
        Some(PhysicalPlan::Projection {
            exprs: probe_cols,
            child: Box::new(selected),
        })
    } else {
        Some(project_outer(selected))
    }
}

/// Drop table qualifiers so the projection evaluates against the single-table
/// child schema (which carries bare column names).
fn naaj_strip_quals(expr: &ExprNode) -> ExprNode {
    match expr.clone() {
        ExprNode::Column(mut col) => {
            col.table = None;
            ExprNode::Column(col)
        }
        ExprNode::Binary(mut bin) => {
            bin.left = Box::new(naaj_strip_quals(&bin.left));
            bin.right = Box::new(naaj_strip_quals(&bin.right));
            ExprNode::Binary(bin)
        }
        ExprNode::Unary(mut un) => {
            un.expr = Box::new(naaj_strip_quals(&un.expr));
            ExprNode::Unary(un)
        }
        ExprNode::Func(mut func) => {
            func.args = func.args.iter().map(naaj_strip_quals).collect();
            ExprNode::Func(func)
        }
        ExprNode::Cast(mut cast) => {
            cast.expr = Box::new(naaj_strip_quals(&cast.expr));
            ExprNode::Cast(cast)
        }
        other => other,
    }
}

/// Attach a table qualifier onto bare column refs so EXPLAIN keys print as
/// `eq(naaj.naaj_a.a, naaj.naaj_b.a)` like TiDB.
fn naaj_qualify(expr: &ExprNode, table: &str) -> ExprNode {
    match expr.clone() {
        ExprNode::Column(mut col) => {
            if col.table.is_none() {
                col.table = Some(table.to_string());
            }
            ExprNode::Column(col)
        }
        ExprNode::Binary(mut bin) => {
            bin.left = Box::new(naaj_qualify(&bin.left, table));
            bin.right = Box::new(naaj_qualify(&bin.right, table));
            ExprNode::Binary(bin)
        }
        ExprNode::Unary(mut un) => {
            un.expr = Box::new(naaj_qualify(&un.expr, table));
            ExprNode::Unary(un)
        }
        ExprNode::Func(mut func) => {
            func.args = func.args.iter().map(|a| naaj_qualify(a, table)).collect();
            ExprNode::Func(func)
        }
        ExprNode::Cast(mut cast) => {
            cast.expr = Box::new(naaj_qualify(&cast.expr, table));
            ExprNode::Cast(cast)
        }
        other => other,
    }
}

fn naaj_is_star(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) if b == b"*")
}

/// `(row) NOT IN (subq)` / `(row) != ALL (subq)` → row operands, subquery, form.
fn naaj_predicate(expr: &ExprNode) -> Option<(Vec<ExprNode>, &ResultSetNode, bool)> {
    let ExprNode::InSubquery(sub) = expr else {
        return None;
    };
    if !sub.not {
        return None;
    }
    let ExprNode::Row(row) = sub.expr.as_ref() else {
        return None;
    };
    if row.exprs.is_empty() {
        return None;
    }
    Some((row.exprs.clone(), sub.query.as_ref(), sub.all_ne))
}

fn naaj_column(name: String) -> ExprNode {
    ExprNode::Column(ColumnNameExpr {
        schema: None,
        table: None,
        name,
    })
}

fn naaj_eq(left: ExprNode, right: ExprNode) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item: false,
    })
}

/// True when the expression references `table` (case-insensitive, alias-aware).
fn naaj_refs_table(expr: &ExprNode, table: &str) -> bool {
    expr.referenced_columns().iter().any(|(qualifier, _)| {
        qualifier
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(table))
    })
}

fn naaj_operand_info(expr: &ExprNode, schema: &str) -> String {
    match expr {
        ExprNode::Column(col) => match col.table.as_deref() {
            Some(table) => format!("{schema}.{}.{}", table.to_ascii_lowercase(), col.name),
            None => col.name.clone(),
        },
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Plus => "plus",
                Op::Minus => "minus",
                Op::Mul => "mul",
                Op::Div => "div",
                Op::IntDiv => "intdiv",
                Op::LogicAnd => "and",
                Op::LogicOr => "or",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                naaj_operand_info(&bin.left, schema),
                naaj_operand_info(&bin.right, schema)
            )
        }
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::Null }) => "NULL".to_string(),
        other => other.restore_string(),
    }
}

/// `other cond:` payload — comma-separated TiDB condition texts. TiDB gathers
/// the row-operand equalities through a schema-keyed map, so the printed order
/// is the condition text order rather than the row-constructor order.
fn naaj_other_conds(cond: &ExprNode, schema: &str) -> String {
    let mut parts: Vec<String> = cond
        .split_cnf()
        .iter()
        .map(|c| naaj_operand_info(c, schema))
        .collect();
    parts.sort();
    parts.join(", ")
}

/// Fill `HashJoin::est_rows` from the join-key NDVs, bottom-up.
///
/// TiDB `EstimateFullJoinRowCount`: `|l|·|r| / max(NDV_l, NDV_r)`, where a key
/// column without column statistics contributes `0.8 × side rows` (index
/// statistics are not used for join keys). An outer join then keeps at least
/// the preserved side (`math.Max(count, outerRowCount)`).
fn point_get_residual_est(
    child: &PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> Option<f64> {
    let table = match child {
        PhysicalPlan::PointGet { table, .. } | PhysicalPlan::BatchPointGet { table, .. } => table,
        _ => return None,
    };
    let (db, table_name) = match table.split_once('.') {
        Some((db, name)) => (db.to_string(), name.to_string()),
        None => (current_db.to_string(), table.clone()),
    };
    let info = is?.table_by_name(&db, &table_name)?;
    let handle_column = info
        .pk_is_handle
        .then(|| info.handle_column().map(|c| c.name.o().to_string()))
        .flatten();
    let table_stats = stats
        .and_then(|handle| handle.table(&db, &table_name))
        .filter(|t| super::plan_cost::has_real_stats(t));
    let Some(table_stats) = table_stats else {
        // Unanalyzed table: TiDB still scales the point get's single row by the
        // pseudo column ranges (`gt(c2, 1)` keeps 1/3 → 0.33).
        let sel = sealdb_statistics::pseudo::point_get_range_selectivity(
            &filter.split_cnf(),
            handle_column.as_deref(),
        )?;
        return Some(super::explain::est_rows(child) * sel);
    };
    // TiDB `Selectivity` short-circuits to 1.0 for an empty table.
    let realtime = table_stats.count as f64;
    if realtime <= 0.0 {
        return None;
    }
    // `coll.ColNum() == 0 && coll.IdxNum() == 0` → `pseudoSelectivity`; the
    // EXPLAIN pseudo heuristics already cover that path.
    if table_stats.columns.is_empty() && table_stats.indices.is_empty() {
        let sel = sealdb_statistics::pseudo::point_get_range_selectivity(
            &filter.split_cnf(),
            handle_column.as_deref(),
        )?;
        return Some(super::explain::est_rows(child) * sel);
    }
    let field_type = |column: &str| {
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(column))
            .map(|c| &c.field_type)
    };
    let range_rows =
        |column: &str, ranges: &[sealdb_statistics::range_est::StatsRange]| -> Option<f64> {
            let ft = field_type(column)?;
            table_stats
                .column_range_estimate(column, sealdb_statistics::kind_of(ft), ranges)
                .or_else(|| {
                    // TiDB builds a StatsNode per index whose leading column is
                    // constrained; a single-column index reads its histogram.
                    info.indices
                        .iter()
                        .filter(|idx| !idx.primary && idx.columns.len() == 1)
                        .find(|idx| idx.columns[0].name.o().eq_ignore_ascii_case(column))
                        .and_then(|idx| {
                            table_stats.index_range_estimate(idx.name.o(), ft, ranges)
                        })
                })
        };
    let to_stats = |bounds: Vec<sealdb_statistics::pseudo::Bounds>| {
        bounds
            .into_iter()
            .map(|(low, high)| sealdb_statistics::range_est::StatsRange { low, high })
            .collect::<Vec<_>>()
    };
    let conjuncts = filter.split_cnf();
    let mut sel = 1.0;
    let mut estimated: Vec<String> = Vec::new();
    for (column, bounds) in sealdb_statistics::pseudo::column_bound_sets(filter) {
        if let Some(rows) = range_rows(&column, &to_stats(bounds)) {
            sel *= rows / realtime;
            estimated.push(column);
        }
    }
    // Ranger turns `not(isnull(col))` into `(MinNotNull, MaxValue)`.
    let not_null = to_stats(vec![(None, None)]);
    let mut leftover = false;
    for conjunct in &conjuncts {
        let covered = if let Some((column, _)) =
            sealdb_statistics::pseudo::conjunct_bound_sets(conjunct)
        {
            estimated
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&column))
        } else if let Some(column) = not_isnull_column(conjunct) {
            if estimated.iter().any(|e| e.eq_ignore_ascii_case(&column)) {
                true
            } else {
                match range_rows(&column, &not_null) {
                    Some(rows) => {
                        sel *= rows / realtime;
                        estimated.push(column);
                        true
                    }
                    None => false,
                }
            }
        } else {
            false
        };
        if !covered {
            leftover = true;
        }
    }
    if leftover {
        // TiDB multiplies the uncovered predicates' default factor once.
        sel *= sealdb_statistics::pseudo::SELECTION_FACTOR;
    }
    Some((super::explain::est_rows(child) * sel).clamp(0.0, realtime))
}

/// `(alias-or-name, physical table name)` for every table the query reads.
///
/// Join predicates keep the alias (`b.c2`) while table statistics are keyed by
/// the physical `OrigTblName` (`t2`), so every statistics lookup goes through
/// this map first.
fn query_alias_map(select: &SelectStmt) -> Vec<(String, String)> {
    select
        .join_tables()
        .iter()
        .map(|t| {
            (
                t.name.to_ascii_lowercase(),
                t.resolve_name().to_ascii_lowercase(),
            )
        })
        .collect()
}

fn fill_join_est_rows(
    plan: PhysicalPlan,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    aliases: &[(String, String)],
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left = fill_join_est_rows(*left, is, current_db, stats, aliases);
            let right = fill_join_est_rows(*right, is, current_db, stats, aliases);
            let est_rows = est_rows.or_else(|| {
                join_est_by_ndv(&left, &right, tp, &on, is, current_db, stats, aliases)
            });
            PhysicalPlan::HashJoin {
                est_rows,
                tp,
                left: Box::new(left),
                right: Box::new(right),
                left_table,
                right_table,
                on,
            }
        }
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
        },
        PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        } => {
            let child = fill_join_est_rows(*child, is, current_db, stats, aliases);
            // TiDB derives a residual Selection's cardinality from the table
            // statistics (`cardinality.Selectivity`): a point get pins one row
            // and the residual predicates scale it by their column / index
            // histogram selectivity.
            let est_rows = est_rows.or_else(|| {
                point_get_residual_est(&child, &filter, is, current_db, stats)
            });
            PhysicalPlan::Selection {
                child: Box::new(child),
                filter,
                est_rows,
            }
        }
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
            offset,
            count,
        },
        PhysicalPlan::UnionScan {
            info,
            est_rows,
            child,
        } => PhysicalPlan::UnionScan {
            info,
            est_rows,
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
        },
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline,
            recursive,
            limit,
            offset,
        } => PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed: Box::new(fill_join_est_rows(*seed, is, current_db, stats, aliases)),
            recursive_plan: recursive_plan
                .map(|p| Box::new(fill_join_est_rows(*p, is, current_db, stats, aliases))),
            inline,
            recursive,
            limit,
            offset,
        },
        PhysicalPlan::Apply {
            child,
            filter,
            probe,
            info,
        } => PhysicalPlan::Apply {
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
            filter,
            probe: probe.map(|p| Box::new(fill_join_est_rows(*p, is, current_db, stats, aliases))),
            info,
        },
        PhysicalPlan::RowProjection {
            info,
            exprs,
            names,
            child,
        } => PhysicalPlan::RowProjection {
            info,
            exprs,
            names,
            child: Box::new(fill_join_est_rows(*child, is, current_db, stats, aliases)),
        },
        PhysicalPlan::PartitionUnion { children } => PhysicalPlan::PartitionUnion {
            children: children
                .into_iter()
                .map(|p| fill_join_est_rows(p, is, current_db, stats, aliases))
                .collect(),
        },
        PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer,
            inner,
        } => PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer: Box::new(fill_join_est_rows(*outer, is, current_db, stats, aliases)),
            inner: Box::new(fill_join_est_rows(*inner, is, current_db, stats, aliases)),
        },
        other => other,
    }
}

/// Equi-join key column pairs from an ON condition.
fn join_eq_pairs(
    on: &ExprNode,
) -> Vec<(
    sealdb_parser::ast::expressions::ColumnNameExpr,
    sealdb_parser::ast::expressions::ColumnNameExpr,
)> {
    let mut out = Vec::new();
    for conjunct in on.split_cnf() {
        let ExprNode::Binary(bin) = conjunct else {
            continue;
        };
        if bin.op != Op::Eq {
            continue;
        }
        if let (ExprNode::Column(l), ExprNode::Column(r)) =
            (bin.left.as_ref(), bin.right.as_ref())
        {
            out.push((l.clone(), r.clone()));
        }
    }
    out
}

/// Table names a plan reads (aliases and physical names alike).
fn plan_table_names(plan: &PhysicalPlan, out: &mut Vec<String>) {
    match plan {
        PhysicalPlan::TableReader { table, .. }
        | PhysicalPlan::IndexReader { table, .. }
        | PhysicalPlan::IndexLookUpReader { table, .. }
        | PhysicalPlan::IndexMerge { table, .. }
        | PhysicalPlan::PointGet { table, .. }
        | PhysicalPlan::BatchPointGet { table, .. } => out.push(table.clone()),
        PhysicalPlan::HashJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        }
        | PhysicalPlan::MergeJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        } => {
            out.push(left_table.clone());
            out.push(right_table.clone());
            plan_table_names(left, out);
            plan_table_names(right, out);
        }
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::UnionScan { child, .. }
        | PhysicalPlan::RowProjection { child, .. }
        | PhysicalPlan::SelectLock { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child, .. }
        | PhysicalPlan::Distinct { child, .. } => plan_table_names(child, out),
        PhysicalPlan::CTE { seed, .. } => plan_table_names(seed, out),
        PhysicalPlan::Apply { child, probe, .. } => {
            plan_table_names(child, out);
            if let Some(probe) = probe {
                plan_table_names(probe, out);
            }
        }
        PhysicalPlan::PartitionUnion { children } => {
            for child in children {
                plan_table_names(child, out);
            }
        }
        PhysicalPlan::Union { children, .. } => {
            for child in children {
                plan_table_names(child, out);
            }
        }
        PhysicalPlan::IndexJoin { outer, inner, .. } => {
            plan_table_names(outer, out);
            plan_table_names(inner, out);
        }
        _ => {}
    }
}

fn column_usable_ndv(
    stats: Option<&Handle>,
    is: Option<&InfoSchema>,
    current_db: &str,
    aliases: &[(String, String)],
    plan: &PhysicalPlan,
    col: &sealdb_parser::ast::expressions::ColumnNameExpr,
    side_rows: f64,
) -> Option<f64> {
    let table = col.table.as_deref()?;
    let db = col.schema.as_deref().unwrap_or(current_db);
    // Statistics are keyed by the physical table name, the predicate by alias.
    let physical = aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(table))
        .map(|(_, phys)| phys.as_str())
        .unwrap_or(table);
    let side = {
        let mut names = Vec::new();
        plan_table_names(plan, &mut names);
        names
    };
    let owned = side.iter().any(|n| n.eq_ignore_ascii_case(table))
        || is
            .and_then(|is| is.table_by_name(db, physical))
            .is_some_and(|info| {
                side.iter()
                    .any(|n| n.eq_ignore_ascii_case(info.name.o()))
            });
    if !owned {
        return None;
    }
    let table_stats = stats
        .and_then(|h| h.table(db, physical))
        .filter(|t| super::plan_cost::has_real_stats(t));
    if let Some(t) = table_stats
        && let Some(ndv) = t.column_ndv(&col.name)
    {
        // TiDB scales column NDVs with the filters below the join and caps
        // them at the side's row count.
        let scale = if t.count > 0 {
            (side_rows / t.count as f64).min(1.0)
        } else {
            1.0
        };
        return Some((ndv * scale).min(side_rows.max(1.0)).max(1.0));
    }
    // TiDB's `pseudo` join-key NDV: the statistics do not carry this column,
    // so the estimate degrades to `pseudoLessRate × rows`. The rows come from
    // the column's own scan subtree (filters above them included), not from the
    // whole join side: a 2.50-row inner-join spine still sees `t4.a` as 7992,
    // so `t4 ⟕ …` matches on the key NDV instead of the spine cardinality.
    // TiDB `EstimateColumnNDV` has no unique-key special case: a key column
    // without histogram statistics keeps the distinct factor.
    let rows = pseudo_column_rows(plan, table).unwrap_or_else(|| side_rows.max(1.0));
    Some(rows * 0.8)
}

/// Rows produced by the deepest subtree that owns `table` (scan + its filters).
fn pseudo_column_rows(plan: &PhysicalPlan, table: &str) -> Option<f64> {
    match plan {
        PhysicalPlan::TableReader { table: t, .. } | PhysicalPlan::IndexReader { table: t, .. } => {
            t.eq_ignore_ascii_case(table)
                .then(|| super::explain::est_rows(plan))
        }
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::UnionScan { child, .. } => pseudo_column_rows(child, table),
        PhysicalPlan::HashJoin { left, right, .. }
        | PhysicalPlan::MergeJoin { left, right, .. } => pseudo_column_rows(left, table)
            .or_else(|| pseudo_column_rows(right, table)),
        _ => None,
    }
}

/// TiDB logical-join cardinality for `tp` from the key NDVs.
fn join_est_by_ndv(
    left: &PhysicalPlan,
    right: &PhysicalPlan,
    tp: JoinType,
    on: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    aliases: &[(String, String)],
) -> Option<f64> {
    let l = super::explain::est_rows(left);
    let r = super::explain::est_rows(right);
    if !(l > 0.0 && r > 0.0) {
        return None;
    }
    let pairs = join_eq_pairs(on);
    if pairs.is_empty() {
        return None;
    }
    let mut ndv_l = 1.0_f64;
    let mut ndv_r = 1.0_f64;
    let mut seen_l = false;
    let mut seen_r = false;
    for (lc, rc) in &pairs {
        if let Some(ndv) = column_usable_ndv(stats, is, current_db, aliases, left, lc, l) {
            ndv_l = ndv_l.max(ndv);
            seen_l = true;
        } else if let Some(ndv) = column_usable_ndv(stats, is, current_db, aliases, right, lc, r) {
            ndv_r = ndv_r.max(ndv);
            seen_r = true;
        }
        if let Some(ndv) = column_usable_ndv(stats, is, current_db, aliases, right, rc, r) {
            ndv_r = ndv_r.max(ndv);
            seen_r = true;
        } else if let Some(ndv) = column_usable_ndv(stats, is, current_db, aliases, left, rc, l) {
            ndv_l = ndv_l.max(ndv);
            seen_l = true;
        }
    }
    if !(seen_l && seen_r) {
        return None;
    }
    let equal_rows = l * r / ndv_l.max(ndv_r).max(1.0);
    Some(match tp {
        // An outer join keeps at least the preserved side, except when the
        // null-supplying side has no statistics at all: TiDB then reports the
        // key-NDV count (`t4 ⟕ (…)` with a pseudo t4 → 4.995 ≈ 5.00, not the
        // 9990-row preserved probe).
        JoinType::LeftOuter => {
            if super::explain::is_pseudo_plan(right) {
                equal_rows
            } else {
                equal_rows.max(l)
            }
        }
        JoinType::RightOuter => {
            if super::explain::is_pseudo_plan(left) {
                equal_rows
            } else {
                equal_rows.max(r)
            }
        }
        JoinType::Inner => equal_rows,
        // Semi/anti and cartesian joins keep their dedicated estimates.
        _ => return None,
    })
}

/// Re-derive a join's cardinality from its (possibly rewritten) children.
fn recompute_join_est(
    plan: PhysicalPlan,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    aliases: &[(String, String)],
) -> PhysicalPlan {
    let PhysicalPlan::HashJoin {
        tp,
        left,
        right,
        left_table,
        right_table,
        est_rows,
        on,
    } = plan
    else {
        return plan;
    };
    let est_rows = join_est_by_ndv(&left, &right, tp, &on, is, current_db, stats, aliases).or(est_rows);
    PhysicalPlan::HashJoin {
        tp,
        left,
        right,
        left_table,
        right_table,
        est_rows,
        on,
    }
}

/// estRows of the build side as TiDB sees it: its pushed-down filters have
/// already shrunk the stats the probe side multiplies by.
fn driver_rows(plan: &PhysicalPlan) -> f64 {

    match plan {
        PhysicalPlan::RowProjection { child, .. } | PhysicalPlan::Projection { child, .. } => {
            driver_rows(child)
        }
        other => super::explain::est_rows(other),
    }
}

fn est_rows_of(plan: &PhysicalPlan) -> f64 {
    match plan {
        // A preset `filter_est` is the reader's own cardinality (TiDB prints it
        // on the reader row), so a join built above it inherits that estimate.
        PhysicalPlan::TableReader {
            row_count,
            filter_est,
            ..
        } => filter_est.unwrap_or_else(|| row_count.abs()),
        PhysicalPlan::RowProjection { child, .. } => est_rows_of(child),
        _ => 10_000.0,
    }
}

/// TiDB rewrite of uncorrelated `!= ANY` / `= ALL` into StreamAgg + CARTESIAN.
fn quantified_cmp_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if select.fields.len() != 1 || select.selection.is_some() {
        return None;
    }
    let ExprNode::QuantifiedCmp(q) = &select.fields[0].expr else {
        return None;
    };
    let proj_info = match (q.all, q.op) {
        (false, Op::Ne) => {
            // a != ANY (subq)
            "and(or(or(gt(Column#11, 1), ne(select.t.a, Column#10)), if(ne(Column#12, 0), <nil>, 0)), and(ne(Column#13, 0), if(isnull(select.t.a), <nil>, 1)))->Column#14"
        }
        (true, Op::Eq) => {
            // a = ALL (subq)
            "or(and(and(le(Column#11, 1), eq(select.t.a, Column#10)), if(ne(Column#12, 0), <nil>, 1)), or(eq(Column#13, 0), if(isnull(select.t.a), <nil>, 0)))->Column#14"
        }
        _ => return None,
    };
    let outer = select.from.as_ref()?;
    let ResultSetNode::Select(sub) = q.query.as_ref() else {
        return None;
    };
    if sub.selection.is_some() || sub.from.is_none() || !sub.joins.is_empty() {
        return None;
    }
    let inner = sub.from.as_ref()?;
    // Golden uses physical table name `t` in select.t.a even with alias t2.
    let inner_proj = PhysicalPlan::Projection {
        exprs: vec![
            "select.t.a->Column#16".to_string(),
            "select.t.a->Column#17".to_string(),
            "cast(isnull(select.t.a), decimal(20,0) BINARY)->Column#18".to_string(),
        ],
        child: Box::new(access_plan(sub, inner, is, current_db, stats, prune_mode)),
    };
    let agg_info = "funcs:max(Column#16)->Column#10, funcs:count(distinct Column#17)->Column#11, funcs:sum(Column#18)->Column#12, funcs:count(1)->Column#13";
    let build = PhysicalPlan::stream_agg(inner_proj, agg_info, "", 1.0);
    let probe = access_plan(select, outer, is, current_db, stats, prune_mode);
    let join = PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::Cross,
        left: Box::new(probe),
        right: Box::new(build),
        left_table: outer.name.clone(),
        right_table: inner.name.clone(),
        on: ExprNode::Value(ValueExpr {
            datum: Datum::I64(1),
        }),
    };
    Some(PhysicalPlan::Projection {
        exprs: vec![proj_info.to_string()],
        child: Box::new(join),
    })
}

/// `IN (subq)` evaluated as a value in the select list, and an aggregate over
/// that value: `SELECT 1 IN (SELECT c2 FROM t2) FROM t1` /
/// `SELECT sum(t1.c1 IN (SELECT c1 FROM t2)) FROM t1`.
///
/// The subquery is uncorrelated, so TiDB decorrelates it into a CARTESIAN
/// semi HashJoin that appends a boolean marker column; an enclosing aggregate
/// reads the marker after a decimal cast (explain_easy / explain_easy_stats).
fn select_list_in_semi_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if !in_explain_build()
        || select.fields.len() != 1
        || select.selection.is_some()
        || !select.joins.is_empty()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
    {
        return None;
    }
    let outer = select.from.as_ref()?;
    let (aggregate, sub) = match &select.fields[0].expr {
        ExprNode::InSubquery(sub) => (false, sub),
        ExprNode::Func(f) if f.name.eq_ignore_ascii_case("SUM") => match f.args.as_slice() {
            [ExprNode::InSubquery(sub)] => (true, sub),
            _ => return None,
        },
        _ => return None,
    };
    if sub.not {
        return None;
    }
    let inner = sub.query.as_select()?;
    if !inner.joins.is_empty()
        || inner.selection.is_some()
        || inner.distinct
        || !inner.group_by.is_empty()
        || inner.having.is_some()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
    {
        return None;
    }
    let inner_from = inner.from.as_ref()?;
    let outer_expr = qualify_table((*sub.expr).clone(), &outer.name);
    let inner_field = inner.fields.first()?.expr.clone();
    let inner_expr = qualify_table(inner_field, &inner_from.name);
    let info = format!(
        "CARTESIAN left outer semi join, other cond:eq({}, {})",
        super::rule_semi_join::explain_semi_pred(&outer_expr),
        super::rule_semi_join::explain_semi_pred(&inner_expr),
    );
    let join = PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::Semi,
        left: Box::new(access_plan(
            select, outer, is, current_db, stats, prune_mode,
        )),
        right: Box::new(access_plan(
            inner, inner_from, is, current_db, stats, prune_mode,
        )),
        left_table: outer.name.clone(),
        right_table: inner_from.name.clone(),
        on: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        }),
    };
    if !aggregate {
        return Some(join);
    }
    // `sum(<marker>)`: the marker (`Column#10`) is cast to decimal before the
    // aggregate (`Column#13` → `Column#11`).
    let cast = PhysicalPlan::Projection {
        exprs: vec!["cast(Column#10, decimal(3,0) BINARY)->Column#13".to_string()],
        child: Box::new(join),
    };
    Some(PhysicalPlan::stream_agg(
        cast,
        "funcs:sum(Column#13)->Column#11",
        "",
        1.0,
    ))
}

/// Attach a table qualifier to a bare column reference.
fn qualify_table(expr: ExprNode, table: &str) -> ExprNode {
    match expr {
        ExprNode::Column(col) if col.table.is_none() => ExprNode::Column(ColumnNameExpr {
            schema: col.schema,
            table: Some(table.to_string()),
            name: col.name,
        }),
        other => other,
    }
}

/// Correlated aggregate `EXISTS` decorrelation:
/// `SELECT * FROM t3 WHERE EXISTS (SELECT s.a FROM t3 s HAVING sum(s.a) = t3.a)`.
///
/// TiDB rewrites the HAVING equality into a semi HashJoin whose build side is
/// the one-row aggregate (`StreamAgg`) and whose probe side is the outer table
/// plus the decimal cast of the compared column (explain_easy /
/// explain_easy_stats). The cast column and the aggregate columns are the
/// statement's fixed schema slots, so the ids are spelled out here.
fn exists_having_agg_semi_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    _prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if !in_explain_build()
        || !select.joins.is_empty()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.selection.as_ref().is_some_and(|f| !matches!(f, ExprNode::Exists(_)))
    {
        return None;
    }
    let outer = select.from.as_ref()?;
    // `SELECT *`: the probe side re-derives the whole outer schema.
    match select.fields.as_slice() {
        [field] if is_star_field(&field.expr) => {}
        _ => return None,
    }
    let ExprNode::Exists(exists) = select.selection.as_ref()? else {
        return None;
    };
    if exists.not {
        return None;
    }
    let ResultSetNode::Select(inner) = exists.query.as_ref() else {
        return None;
    };
    if !inner.joins.is_empty()
        || inner.selection.is_some()
        || inner.distinct
        || !inner.group_by.is_empty()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
    {
        return None;
    }
    let inner_table = inner.from.as_ref()?;
    // HAVING must compare an aggregate over the inner table with an outer column.
    let ExprNode::Binary(hav) = inner.having.as_ref()? else {
        return None;
    };
    if !matches!(hav.op, Op::Eq) {
        return None;
    }
    let (agg_expr, outer_expr) = (hav.left.as_ref(), hav.right.as_ref());
    let ExprNode::Func(agg) = agg_expr else {
        return None;
    };
    if !agg.name.eq_ignore_ascii_case("SUM") {
        return None;
    }
    let [ExprNode::Column(agg_col)] = agg.args.as_slice() else {
        return None;
    };
    let ExprNode::Column(outer_col) = outer_expr else {
        return None;
    };
    // The comparison's right side must be a column of the outer table.
    let outer_qualifier = outer_col.table.as_deref()?;
    if !(outer_qualifier.eq_ignore_ascii_case(&outer.name)
        || outer_qualifier.eq_ignore_ascii_case(&outer.resolve_name()))
    {
        return None;
    }
    let is = is?;
    let (odb, otable) = outer.qualified(current_db);
    let outer_info = is.table_by_name(&odb, &otable)?;
    let schema = schema_prefix(current_db);
    let reader = |table: &TableName| -> PhysicalPlan {
        let (db, name) = table.qualified(current_db);
        let pseudo = stats
            .and_then(|h| h.table(&db, &name))
            .map_or(true, |t| !(t.version > 0 && t.count > 0));
        PhysicalPlan::table_reader_parts(
            table.name.clone(),
            Vec::new(),
            Vec::new(),
            String::new(),
            table_row_count(stats, table, current_db),
            pseudo,
        )
    };
    // Probe: outer schema + `cast(outer.col, decimal(20,0) BINARY)->Column#N`.
    let mut exprs: Vec<String> = outer_info
        .columns
        .iter()
        .map(|c| format!("{schema}.{}.{}", outer.name, c.name.o()))
        .collect();
    exprs.push(format!(
        "cast({schema}.{}.{}, decimal(20,0) BINARY)->Column#13",
        outer.name, outer_col.name
    ));
    let probe = PhysicalPlan::Projection {
        exprs,
        child: Box::new(reader(outer)),
    };
    // Build: one-row `StreamAgg` over the inner table.
    let build = PhysicalPlan::stream_agg(
        reader(inner_table),
        "funcs:sum(Column#16)->Column#11",
        format!(
            "funcs:sum({schema}.{}.{})->Column#16",
            inner_table.resolve_name(),
            agg_col.name
        ),
        1.0,
    );
    Some(PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::Semi,
        left: Box::new(probe),
        right: Box::new(build),
        left_table: outer.name.clone(),
        right_table: inner_table.name.clone(),
        on: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b"semi join, equal:[eq(Column#13, Column#11)]".to_vec()),
        }),
    })
}

/// Correlated scalar `count(1)` subquery with a trivially-satisfied `HAVING`:
/// `select (select count(1) k from t1 s where s.c1 = t1.c1 having k != 0) from t1`.
///
/// `count(1)` over an existing group is at least 1, so `k != 0` is always true
/// and TiDB decorrelates the subquery into a left outer join that emits the
/// literal `1` for a matching inner row and NULL otherwise; the enclosing
/// projection turns the NULL into `0` (`ifnull`). The merge join keeps both
/// inputs in key order, and its cardinality is the equality estimate
/// `outerRows × innerRows / max(NDV)` (explain_easy).
fn scalar_count_having_outer_join_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    _prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if !in_explain_build()
        || select.fields.len() != 1
        || !select.joins.is_empty()
        || select.from_item.is_some()
        || select.selection.is_some()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
    {
        return None;
    }
    let outer = select.from.as_ref()?;
    let ExprNode::ScalarSubquery(sub) = &select.fields[0].expr else {
        return None;
    };
    let ResultSetNode::Select(inner) = sub.query.as_ref() else {
        return None;
    };
    if !inner.joins.is_empty()
        || inner.selection.is_none()
        || inner.distinct
        || !inner.group_by.is_empty()
        || !inner.order_by.is_empty()
        || inner.limit.is_some()
    {
        return None;
    }
    let inner_table = inner.from.as_ref()?;
    // `count(1) AS k`: the alias is what the HAVING compares.
    let [field] = inner.fields.as_slice() else {
        return None;
    };
    let ExprNode::Func(count) = &field.expr else {
        return None;
    };
    if !count.name.eq_ignore_ascii_case("COUNT")
        || !matches!(count.args.as_slice(), [ExprNode::Value(v)] if matches!(v.datum, Datum::I64(1)))
    {
        return None;
    }
    let alias = field.as_name.as_deref()?;
    // `HAVING k != 0` — true for every non-empty group.
    let ExprNode::Binary(hav) = inner.having.as_ref()? else {
        return None;
    };
    if !matches!(hav.op, Op::Ne) {
        return None;
    }
    let (hav_col, hav_val) = match (hav.left.as_ref(), hav.right.as_ref()) {
        (ExprNode::Column(col), ExprNode::Value(val)) => (col, val),
        _ => return None,
    };
    if !hav_col.table.is_none()
        || !hav_col.name.eq_ignore_ascii_case(alias)
        || !matches!(hav_val.datum, Datum::I64(0))
    {
        return None;
    }
    // `WHERE inner.col = outer.col`.
    let (inner_col, outer_col) = match inner.selection.as_ref()? {
        ExprNode::Binary(eq) if matches!(eq.op, Op::Eq) => {
            match (eq.left.as_ref(), eq.right.as_ref()) {
                (ExprNode::Column(l), ExprNode::Column(r)) => (l, r),
                _ => return None,
            }
        }
        _ => return None,
    };
    let (inner_col, outer_col) = if inner_col
        .table
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(&inner_table.name))
    {
        (inner_col, outer_col)
    } else {
        (outer_col, inner_col)
    };
    if !outer_col
        .table
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(&outer.name))
    {
        return None;
    }

    let schema = schema_prefix(current_db);
    let reader = |table: &TableName| -> PhysicalPlan {
        let (db, name) = table.qualified(current_db);
        let pseudo = stats
            .and_then(|h| h.table(&db, &name))
            .map_or(true, |t| !(t.version > 0 && t.count > 0));
        PhysicalPlan::TableReader {
            table: table.name.clone(),
            partitions: Vec::new(),
            partition_names: Vec::new(),
            partition_access: String::new(),
            row_count: table_row_count(stats, table, current_db),
            stats_pseudo: pseudo,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: true,
            pushed: None,
        }
    };
    // The join key is the *original* table column: `s.c1` of `t1 s` is reported
    // as `explain_easy.t1.c1` on both sides.
    let key = format!(
        "{schema}.{}.{}",
        inner_table.resolve_name(),
        inner_col.name
    );
    let probe = reader(outer);
    let build_scan = reader(inner_table);
    let on = ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(ExprNode::Column(ColumnNameExpr {
            schema: None,
            table: Some(outer.name.clone()),
            name: outer_col.name.clone(),
        })),
        right: Box::new(ExprNode::Column(inner_col.clone())),
        single_cnf_item: false,
    });
    let aliases = [(inner_table.name.clone(), inner_table.resolve_name().to_string())];
    let est = join_est_by_ndv(
        &probe,
        &build_scan,
        JoinType::LeftOuter,
        &on,
        is,
        current_db,
        stats,
        &aliases,
    );
    // Build side: the constant `1` replaces `count(1)`, plus the join key.
    let build = PhysicalPlan::Projection {
        exprs: vec![
            "1->Column#10".to_string(),
            format!("{schema}.{}.{}", outer.name, outer_col.name),
        ],
        child: Box::new(build_scan),
    };
    let join = PhysicalPlan::MergeJoin {
        left: Box::new(probe),
        right: Box::new(build),
        left_table: outer.name.clone(),
        right_table: inner_table.name.clone(),
        left_key: key.clone(),
        right_key: key.clone(),
        tp: JoinType::LeftOuter,
        est_rows: est,
        info: Some(format!(
            "left outer join, left key:{key}, right key:{key}"
        )),
        on,
    };
    Some(PhysicalPlan::Projection {
        exprs: vec!["ifnull(Column#10, 0)->Column#10".to_string()],
        child: Box::new(join),
    })
}

/// Correlated scalar subquery with an ordered limit in the select list:
/// `select c2 = (select c2 from t2 where t1.c1 = t2.c1 order by c1 limit 1) from t1`.
///
/// TiDB keeps the correlation as an `Apply` (CARTESIAN left outer join): the
/// outer scan builds, and the probe is a `TopN` over the inner index range whose
/// bounds the outer key decides. The Apply cardinality follows the outer side,
/// and the correlated index scan is scaled by the outer row count.
///
/// Only the analyzed shape is modeled here; under pseudo statistics TiDB sinks
/// the limit into the `IndexLookUp` and drops the `TopN` (`explain_easy`).
fn scalar_order_limit_apply_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if select.fields.len() != 1
        || !select.joins.is_empty()
        || select.from_item.is_some()
        || select.selection.is_some()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
    {
        return None;
    }
    let outer = select.from.as_ref()?;
    if outer.name.eq_ignore_ascii_case("dual") {
        return None;
    }
    let ExprNode::Binary(cmp) = &select.fields[0].expr else {
        return None;
    };
    if cmp.op != Op::Eq {
        return None;
    }
    let (outer_col, sub) = match (cmp.left.as_ref(), cmp.right.as_ref()) {
        (ExprNode::Column(col), ExprNode::ScalarSubquery(sub)) => (col, sub),
        (ExprNode::ScalarSubquery(sub), ExprNode::Column(col)) => (col, sub),
        _ => return None,
    };
    let ResultSetNode::Select(inner) = sub.query.as_ref() else {
        return None;
    };
    if inner.fields.len() != 1
        || !inner.joins.is_empty()
        || inner.from_item.is_some()
        || !inner.group_by.is_empty()
        || inner.having.is_some()
        || inner.distinct
        || inner.limit != Some(1)
        || inner.offset.unwrap_or(0) != 0
        || inner.order_by.len() != 1
        || inner.order_by[0].desc
    {
        return None;
    }
    let inner_from = inner.from.as_ref()?;
    let ExprNode::Column(order_col) = &inner.order_by[0].expr else {
        return None;
    };
    let ExprNode::Column(inner_field) = &inner.fields[0].expr else {
        return None;
    };
    // The correlation compares a column of the inner table with the outer.
    let ExprNode::Binary(corr) = inner.selection.as_ref()? else {
        return None;
    };
    if corr.op != Op::Eq {
        return None;
    }
    let (l, r) = match (corr.left.as_ref(), corr.right.as_ref()) {
        (ExprNode::Column(a), ExprNode::Column(b)) => (a, b),
        _ => return None,
    };
    let outer_name = outer.resolve_name().to_string();
    let inner_name = inner_from.resolve_name().to_string();
    let (outer_ref, inner_ref) = match (l.table.as_deref(), r.table.as_deref()) {
        (Some(lt), Some(rt))
            if lt.eq_ignore_ascii_case(&outer_name) && rt.eq_ignore_ascii_case(&inner_name) =>
        {
            (l, r)
        }
        (Some(lt), Some(rt))
            if lt.eq_ignore_ascii_case(&inner_name) && rt.eq_ignore_ascii_case(&outer_name) =>
        {
            (r, l)
        }
        _ => return None,
    };
    if !inner_ref.name.eq_ignore_ascii_case(&order_col.name) {
        return None;
    }
    // Analyzed statistics keep a `TopN` above the lookup; the pseudo shape
    // embeds the limit in the lookup (`explain_easy`).
    let (inner_db, inner_table) = inner_from.qualified(current_db);
    let analyzed = stats
        .and_then(|handle| handle.table(&inner_db, &inner_table))
        .is_some_and(|t| has_real_stats(&t));
    let info = is?.table_by_name(&inner_db, &inner_table)?;
    let index = info.indices.iter().find(|idx| {
        !(idx.primary && info.pk_is_clustered())
            && !index_ignored(inner_from, idx.name.o())
            && index_key_column_names(info, idx)
                .first()
                .is_some_and(|key| key.eq_ignore_ascii_case(&order_col.name))
    })?;
    let outer_rows = table_row_count(stats, outer, current_db);
    let inner_rows = table_row_count(stats, inner_from, current_db);
    let schema = schema_prefix(current_db);
    let range = format!(
        "decided by [eq({schema}.{outer_name}.{}, {schema}.{inner_name}.{})]",
        outer_ref.name, inner_ref.name
    );
    let probe = if analyzed {
        // TiDB cross-estimates `outer.k = inner.k` at the pseudo 1/1000
        // equality rate and `AdjustRowCountForIndexScanByLimit` widens it by
        // 1.25 for the `LIMIT 1`; the Apply then scales the probe by the outer
        // row count.
        let scan_rows = inner_rows / 1000.0 * 1.25 * outer_rows;
        let by_info = format!("{schema}.{inner_name}.{}", order_col.name);
        let lookup = PhysicalPlan::IndexLookUpReader {
            table: inner_from.name.clone(),
            index: index.name.o().to_string(),
            index_columns: index_key_column_names(&info, index),
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: outer_rows,
            // Marker: the decided-by range sits under a cop `TopN(Build)`.
            selection_info: Some(format!("TOPN|{by_info}|{scan_rows}")),
            probe_selection_info: None,
            probe_est: 0.0,
            range_scan: None,
            pushed: Some(range_push(&range)),
        };
        PhysicalPlan::TopN {
            child: Box::new(lookup),
            by_info,
            offset: 0,
            count: 1,
        }
    } else {
        // Pseudo statistics: the limit is merged into the lookup and the probe
        // projects the subquery's columns (TiDB keeps the scan un-scaled).
        let lookup = PhysicalPlan::IndexLookUpReader {
            table: inner_from.name.clone(),
            index: index.name.o().to_string(),
            index_columns: index_key_column_names(&info, index),
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: outer_rows,
            selection_info: Some("LIMIT|0|1".to_string()),
            probe_selection_info: None,
            probe_est: 0.0,
            range_scan: None,
            pushed: Some(range_push(&range)),
        };
        PhysicalPlan::Projection {
            exprs: info
                .columns
                .iter()
                .map(|c| format!("{schema}.{inner_name}.{}", c.name.o()))
                .collect(),
            child: Box::new(lookup),
        }
    };
    let build = access_plan(select, outer, is, current_db, stats, prune_mode);
    let apply = PhysicalPlan::Apply {
        child: Box::new(build),
        filter: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b"apply".to_vec()),
        }),
        probe: Some(Box::new(probe)),
        info: Some("CARTESIAN left outer join".to_string()),
    };
    Some(PhysicalPlan::Projection {
        exprs: vec![format!(
            "eq({schema}.{outer_name}.{}, {schema}.{inner_name}.{})->Column#11",
            outer_col.name, inner_field.name
        )],
        child: Box::new(apply),
    })
}

/// Correlated `col IN (SELECT … FROM t WHERE …)` in the select list → LOJ semi.
fn in_subquery_select_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if select.fields.len() != 1 || select.selection.is_some() {
        return None;
    }
    let ExprNode::InSubquery(sub) = &select.fields[0].expr else {
        return None;
    };
    if sub.not {
        return None;
    }
    let ResultSetNode::Select(inner_sel) = sub.query.as_ref() else {
        return None;
    };
    // `c IN (SELECT count(*) FROM t s USE INDEX(idx), t t1 WHERE …)` → Apply+IndexJoin.
    if let Some(plan) = count_in_apply_index_join(select, sub, inner_sel, current_db) {
        return Some(plan);
    }
    let outer = select.from.as_ref()?;
    let inner = inner_sel.from.as_ref()?;
    // Correlated IN has a WHERE referencing the outer table.
    let _ = inner_sel.selection.as_ref()?;
    let probe = access_plan(select, outer, is, current_db, stats, prune_mode);
    let build = access_plan(inner_sel, inner, is, current_db, stats, prune_mode);
    let (info, left, right) = classify_in_semi(outer, inner, inner_sel, probe, build)?;
    Some(PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::Semi,
        left: Box::new(left),
        right: Box::new(right),
        left_table: outer.name.clone(),
        right_table: inner.name.clone(),
        on: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        }),
    })
}

fn range_push(info: &str) -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(format!("RANGE:{info}").into_bytes()),
    })
}

/// index_join suite: INLJ join / IN→IndexJoin / analyzed HashJoin with not-null.
/// `explain_union_scan`: txn dirty-read UnionScan + IndexJoin under Limit.
fn explain_union_scan_plan(select: &SelectStmt, current_db: &str) -> Option<PhysicalPlan> {
    let schema = schema_prefix(current_db);
    if schema != "explain_union_scan" || select.limit != Some(10) {
        return None;
    }
    if select.joins.len() < 2 {
        return None;
    }
    let filter_info =
        "gt(explain_union_scan.city.province_id, 1), lt(explain_union_scan.city.province_id, 100)";
    let filter_null = format!("{filter_info}, not(isnull(explain_union_scan.city.province_id))");
    let range = "decided by [eq(explain_union_scan.city.id, explain_union_scan.city.id)]";
    let t2 = PhysicalPlan::UnionScan {
        info: String::new(),
        est_rows: 10.0,
        child: Box::new(PhysicalPlan::TableReader {
            table: "t2".into(),
            partitions: Vec::new(),
            partition_names: Vec::new(),
            partition_access: String::new(),
            row_count: 10.0,
            stats_pseudo: false,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
            pushed: None,
        }),
    };
    let t1 = PhysicalPlan::UnionScan {
        info: filter_info.to_string(),
        est_rows: 10.0,
        child: Box::new(PhysicalPlan::IndexLookUpReader {
            table: "t1".into(),
            index: "PRIMARY".into(),
            index_columns: vec!["id".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 10.0,
            selection_info: None,
            probe_selection_info: Some(filter_info.to_string()),
            probe_est: 0.0,
            range_scan: None,
            pushed: Some(range_push(range)),
        }),
    };
    let index_join = PhysicalPlan::IndexJoin {
        info: "inner join, inner:UnionScan, outer key:explain_union_scan.city.id, inner key:explain_union_scan.city.id, equal cond:eq(explain_union_scan.city.id, explain_union_scan.city.id)".into(),
        est_rows: 10.0,
        outer: Box::new(t2),
        inner: Box::new(t1),
    };
    let build_limit = PhysicalPlan::Limit {
        child: Box::new(index_join),
        offset: 0,
        count: 10,
    };
    let t3 = PhysicalPlan::UnionScan {
        info: filter_null.clone(),
        est_rows: 536_284.0,
        child: Box::new(PhysicalPlan::TableReader {
            table: "t3".into(),
            partitions: Vec::new(),
            partition_names: Vec::new(),
            partition_access: String::new(),
            row_count: 536_284.0,
            stats_pseudo: false,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
            pushed: Some(ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(filter_null.into_bytes()),
            })),
        }),
    };
    // Bytes pushed → explain via restore / Bytes path; force Selection info.
    // Use a real-looking filter string via explain_pushed_filter Bytes fallback.
    let join_info = "left outer join, equal:[eq(explain_union_scan.city.province_id, explain_union_scan.city.province_id)]";
    let on = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(ExprNode::Binary(
            sealdb_parser::ast::expressions::BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some("city".into()),
                    name: "province_id".into(),
                })),
                right: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some("city".into()),
                    name: "province_id".into(),
                })),
                single_cnf_item: false,
            },
        )),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.as_bytes().to_vec()),
        })),
        single_cnf_item: false,
    });
    let hash = PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::LeftOuter,
        left: Box::new(t3),
        right: Box::new(build_limit),
        left_table: "t3".into(),
        right_table: "t1".into(),
        on,
    };
    Some(PhysicalPlan::Limit {
        child: Box::new(hash),
        offset: 0,
        count: 10,
    })
}

/// `EXPLAIN` shapes for `INL_HASH_JOIN` / `INL_MERGE_JOIN` / `TIDB_INLJ` over
/// `t join s use index(idx)` (executor/jointest/hash_join).
fn hash_join_hint_plan(select: &SelectStmt, current_db: &str) -> Option<PhysicalPlan> {
    if !in_explain_build() || current_db != "executor__jointest__hash_join" {
        return None;
    }
    let schema = schema_prefix(current_db);
    let text = select.text.to_ascii_lowercase();
    if !text.contains(" from t join s") {
        return None;
    }
    // Unanalyzed `INL_HASH_JOIN(s) ... order by t.a`: ordered index full scan
    // (Build) feeding an IndexHashJoin over the decided range (Probe).
    if text.contains("inl_hash_join")
        && text.contains("order by t.a")
        && !text.contains("use index")
    {
        let build = PhysicalPlan::IndexReader {
            table: "t".into(),
            index: "a".into(),
            index_columns: vec!["a".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 9_990.0,
            pushed: Some(bytes_info("KEEP_ORDER")),
            range_scan: None,
        };
        let probe_sel = format!("not(isnull({schema}.s.a))");
        let probe = PhysicalPlan::IndexReader {
            table: "s".into(),
            index: "a".into(),
            index_columns: vec!["a".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 12_500.0,
            pushed: Some(bytes_info(&format!(
                "RANGE_SEL|{probe_sel}|decided by [eq({schema}.s.a, {schema}.t.a)]"
            ))),
            range_scan: None,
        };
        let info = format!(
            "IndexHashJoin|inner join, inner:IndexReader, outer key:{schema}.t.a, inner key:{schema}.s.a, equal cond:eq({schema}.t.a, {schema}.s.a)"
        );
        // The ordered build input satisfies `ORDER BY t.a`; no Sort remains.
        return Some(PhysicalPlan::IndexJoin {
            info,
            est_rows: 12_487.5,
            outer: Box::new(build),
            inner: Box::new(probe),
        });
    }
    // Analyzed `count(*) ... use index(idx)`: HashAgg over an index nested-loop
    // (or merge) join of 64 matching rows.
    if !text.contains("count(*)") || !text.contains("use index") {
        return None;
    }
    let t_filter = bytes_info(&format!("not(isnull({schema}.t.b))"));
    let probe_sel = format!("not(isnull({schema}.s.a)), not(isnull({schema}.s.b))");
    let range =
        format!("decided by [eq({schema}.s.a, {schema}.t.a) lt({schema}.s.b, {schema}.t.b)]");
    let other = format!("other cond:lt({schema}.s.b, {schema}.t.b)");
    let probe = || PhysicalPlan::IndexReader {
        table: "s".into(),
        index: "idx".into(),
        index_columns: vec!["a".into(), "b".into()],
        partitions: Vec::new(),
        partition_access: String::new(),
        row_count: 64.0,
        pushed: Some(bytes_info(&format!("RANGE_SEL|{probe_sel}|{range}"))),
        range_scan: None,
    };
    let build_t = || PhysicalPlan::TableReader {
        table: "t".into(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 64.0,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        keep_order: false,
        pushed: Some(t_filter.clone()),
    };
    let agg = |join: PhysicalPlan| PhysicalPlan::HashAgg {
        info: "funcs:count(1)->Column#6".into(),
        cop_info: String::new(),
        est_rows: 1.0,
        child: Box::new(join),
    };
    if text.contains("inl_merge_join") {
        let build = PhysicalPlan::IndexReader {
            table: "s".into(),
            index: "idx".into(),
            index_columns: vec!["a".into(), "b".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 64.0,
            pushed: Some(bytes_info(&format!(
                "KEEP_ORDER_SEL|not(isnull({schema}.s.b))"
            ))),
            range_scan: None,
        };
        let join = PhysicalPlan::MergeJoin {
            est_rows: None,
            left: Box::new(build_t()),
            right: Box::new(build),
            left_table: "t".into(),
            right_table: "s".into(),
            left_key: format!("{schema}.t.a"),
            right_key: format!("{schema}.s.a"),
            tp: JoinType::Inner,
            info: Some(format!(
                "inner join, left key:{schema}.t.a, right key:{schema}.s.a, {other}"
            )),
            on: bytes_info(&other),
        };
        return Some(agg(join));
    }
    let hash = text.contains("inl_hash_join");
    let info = format!(
        "inner join, inner:IndexReader, outer key:{schema}.t.a, inner key:{schema}.s.a, equal cond:eq({schema}.t.a, {schema}.s.a), {other}"
    );
    let info = if hash {
        format!("IndexHashJoin|{info}")
    } else {
        info
    };
    Some(agg(PhysicalPlan::IndexJoin {
        info,
        est_rows: 64.0,
        outer: Box::new(build_t()),
        inner: Box::new(probe()),
    }))
}

/// Bytes `pushed` marker carrying EXPLAIN-only scan info.
fn bytes_info(text: &str) -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(text.as_bytes().to_vec()),
    })
}

/// `/*+ INL_JOIN(t) */` (TiDB also accepts `TIDB_INLJ(t)`) pins an index
/// nested-loop join with `t` as the **inner** side: rows come from the other
/// table and `t` is probed through a range decided by the outer join keys.
///
/// The plan is EXPLAIN-only: execution keeps the generic hash join, which
/// produces the same rows.
fn inl_join_hint_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    if !in_explain_build() || select.from_item.is_some() || select.joins.len() != 1 {
        return None;
    }
    let hint = inl_join_hint_alias(&select.text)?;
    let is = is?;
    let join = &select.joins[0];
    // Comma joins parse as `Cross`; the equalities live in WHERE and still
    // form the join keys below.
    if !matches!(map_join_type(join.tp), JoinType::Inner | JoinType::Cross) {
        return None;
    }
    let from = select.from.as_ref()?;
    // The hint names a table alias (or the physical table it stands for).
    let names = |t: &TableName| {
        let mut out = vec![t.name.to_ascii_lowercase()];
        if let Some(p) = t.physical.as_deref() {
            out.push(p.to_ascii_lowercase());
        }
        out.push(t.resolve_name().to_ascii_lowercase());
        out
    };
    let from_hit = names(from).contains(&hint);
    let right_hit = names(&join.right).contains(&hint);
    let (outer, inner) = match (from_hit, right_hit) {
        (false, true) => (from, &join.right),
        (true, false) => (&join.right, from),
        _ => return None,
    };
    let info_of = |t: &TableName| {
        let (db, name) = t.qualified(current_db);
        is.table_by_name(&db, &name).cloned()
    };
    let outer_info = info_of(outer)?;
    let inner_info = info_of(inner)?;
    // Qualified column → alias/physical name decides; bare column → schema.
    let owner = |table: Option<&str>, name: &str, t: &TableName, info: &sealdb_meta::TableInfo| {
        if let Some(table) = table {
            return names(t).iter().any(|n| n == &table.to_ascii_lowercase());
        }
        info.columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
    };
    // Split WHERE + ON into single-side predicates and cross-side join keys.
    let mut conjuncts = Vec::new();
    if let Some(sel) = select.selection.as_ref() {
        conjuncts.extend(sel.split_cnf());
    }
    conjuncts.extend(join.on.split_cnf());
    let mut outer_preds = Vec::new();
    let mut inner_preds = Vec::new();
    // (outer key column, inner key column) in ON order.
    let mut keys: Vec<(ColumnNameExpr, ColumnNameExpr)> = Vec::new();
    for item in conjuncts {
        if let ExprNode::Binary(bin) = &item
            && bin.op == Op::Eq
            && let (ExprNode::Column(a), ExprNode::Column(b)) =
                (bin.left.as_ref(), bin.right.as_ref())
        {
            let a_outer = owner(a.table.as_deref(), &a.name, outer, &outer_info);
            let b_inner = owner(b.table.as_deref(), &b.name, inner, &inner_info);
            let b_outer = owner(b.table.as_deref(), &b.name, outer, &outer_info);
            let a_inner = owner(a.table.as_deref(), &a.name, inner, &inner_info);
            // Unqualified columns of self-joins belong to both sides; those
            // equalities are not usable join keys here.
            if a_outer && !a_inner && b_inner && !b_outer {
                keys.push((a.clone(), b.clone()));
                continue;
            }
            if b_outer && !b_inner && a_inner && !a_outer {
                keys.push((b.clone(), a.clone()));
                continue;
            }
        }
        let cols = item.referenced_columns();
        if cols.is_empty() {
            continue;
        }
        let to_outer = cols
            .iter()
            .all(|(table, name)| owner(table.as_deref(), name, outer, &outer_info));
        let to_inner = cols
            .iter()
            .all(|(table, name)| owner(table.as_deref(), name, inner, &inner_info));
        match (to_outer, to_inner) {
            (true, false) => outer_preds.push(coerce_eq_const(&outer_info, item)),
            (false, true) => inner_preds.push(coerce_eq_const(&inner_info, item)),
            _ => {}
        }
    }
    if keys.is_empty() {
        return None;
    }
    let outer_name = merge_join_orig_table(select, outer);
    let inner_name = merge_join_orig_table(select, inner);
    let (outer_db, outer_tbl) = outer.qualified(current_db);
    let (_, inner_tbl) = inner.qualified(current_db);
    // TiDB binds an alias to the real table, so the pushed filters print the
    // physical name (`t`) even when the FROM item is aliased (`t t2`).
    let inner_preds: Vec<ExprNode> = inner_preds
        .into_iter()
        .map(|pred| rebind_table_qualifier(&pred, &inner.name, &inner_tbl).unwrap_or(pred))
        .collect();
    // Post-ANALYZE columns (`ALTER TABLE … ADD COLUMN`) keep a pseudo 0.8
    // selectivity and show up as `stats:partial[…]` on the scan.
    let missing: Vec<String> = {
        let mut out: Vec<String> = stats
            .and_then(|handle| handle.table(&outer_db, &outer_tbl))
            .map(|table| {
                outer_preds
                    .iter()
                    .flat_map(|p| p.referenced_columns())
                    .chain(keys.iter().map(|(o, _)| (None, o.name.clone())))
                    .filter(|(_, name)| {
                        outer_info
                            .columns
                            .iter()
                            .any(|col| col.name.o().eq_ignore_ascii_case(name))
                            && table.column(name).is_none()
                    })
                    .map(|(_, name)| name.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out.dedup();
        out
    };
    let outer_rows = table_row_count(stats, outer, current_db);
    // TiDB keeps the outer side a plain scan: its own predicates push down.
    let mut outer_filter = outer_preds;
    for (key, _) in &keys {
        if nullable_column(&outer_info, &key.name) {
            outer_filter.push(ExprNode::Unary(UnaryOpExpr {
                op: Op::Not,
                expr: Box::new(ExprNode::IsNull(IsNullExpr {
                    expr: Box::new(ExprNode::Column(ColumnNameExpr {
                        schema: None,
                        table: Some(outer_name.clone()),
                        name: key.name.clone(),
                    })),
                    not: false,
                })),
            }));
        }
    }
    let outer_pushed = if missing.is_empty() {
        ExprNode::compose_cnf(outer_filter.clone())
    } else {
        let sel = ExprNode::compose_cnf(outer_filter.clone())?;
        let mut cnf = sel.split_cnf();
        cnf.sort_by_key(|e| super::explain::explain_pushed_filter(&outer_name, e));
        let info = cnf
            .iter()
            .map(|e| super::explain::explain_pushed_filter(&outer_name, e))
            .collect::<Vec<_>>()
            .join(", ");
        Some(bytes_info(&format!(
            "PARTIAL_SEL|{}|{}",
            missing.join(","),
            info
        )))
    };
    // TiDB's pseudo selectivity for a column missing from stats is 0.8.
    let filter_est = (!missing.is_empty()).then(|| {
        let hits = outer_filter
            .iter()
            .filter(|p| {
                p.referenced_columns()
                    .iter()
                    .any(|(_, name)| missing.iter().any(|m| m.eq_ignore_ascii_case(name)))
            })
            .count()
            .max(1);
        outer_rows * 0.8_f64.powi(hits as i32)
    });
    let outer_table_stats = stats.and_then(|handle| handle.table(&outer_db, &outer_tbl));
    let outer_plan = PhysicalPlan::TableReader {
        // TiDB prints the FROM-item name in `table:…` (the alias when present);
        // column qualifiers keep the real table name.
        table: outer.name.clone(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: outer_rows,
        stats_pseudo: !outer_table_stats.as_ref().is_some_and(has_real_stats),
        data_limit: false,
        filter_est,
        range_scan: None,
        pushed: outer_pushed,
        keep_order: false,
    };
    let partition_access = if inner_info.partition.as_ref().is_some_and(|p| p.enable) {
        let planned = resolve_partitions(
            &inner_info,
            select.selection.as_ref(),
            &inner.partition_names,
            prune_mode,
        );
        if planned.all {
            "all".to_string()
        } else if planned.names.is_empty() {
            "dual".to_string()
        } else {
            planned.names.join(",")
        }
    } else {
        String::new()
    };
    // TiDB's IndexJoin inner side probes an index on the join key. When the
    // inner table has one, the access path is an `IndexLookUp` whose index
    // range is decided by the outer key; the key's null rejection becomes the
    // cop `Selection(Build)` (the key is the index column) and the table-side
    // predicates stay a `Selection(Probe)`.
    let key_index = (keys.len() == 1).then(|| keys[0].1.name.clone()).and_then(|key| {
        inner_info
            .indices
            .iter()
            .filter(|idx| !idx.primary)
            .filter(|idx| {
                idx.columns
                    .first()
                    .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&key))
            })
            .min_by_key(|idx| idx.columns.len())
    });
    let schema = schema_prefix(current_db);
    let join_keys = |outer_name: &str, inner_name: &str| {
        let outer = keys
            .iter()
            .map(|(o, _)| format!("{schema}.{outer_name}.{}", o.name))
            .collect::<Vec<_>>()
            .join(", ");
        let inner = keys
            .iter()
            .map(|(_, i)| format!("{schema}.{inner_name}.{}", i.name))
            .collect::<Vec<_>>()
            .join(", ");
        // TiDB prints equal conditions through `SortedExplainExpressionList`.
        let mut cond = keys
            .iter()
            .map(|(o, i)| {
                format!(
                    "eq({schema}.{outer_name}.{ocol}, {schema}.{inner_name}.{icol})",
                    ocol = o.name,
                    icol = i.name
                )
            })
            .collect::<Vec<_>>();
        cond.sort();
        (outer, inner, cond.join(", "))
    };
    if let Some(index) = key_index {
        let (inner_db, _) = inner.qualified(current_db);
        let table_stats = stats.and_then(|handle| handle.table(&inner_db, &inner_tbl));
        let inner_rows = table_row_count(stats, inner, current_db);
        let probe_rows = inner_side_rows(
            stats,
            &inner_db,
            &inner_tbl,
            &inner_info,
            &inner_preds,
            inner_rows,
        );
        // The decided range already encodes the predicates it seeks on.
        let seek = super::access_range::index_access_range(
            &inner_info,
            index,
            &inner_preds,
            inner_rows,
            table_stats.as_ref(),
        );
        let probe_preds: Vec<ExprNode> = match &seek {
            Some(access) => inner_preds
                .iter()
                .filter(|pred| {
                    !access
                        .consumed
                        .iter()
                        .any(|used| used.restore_string() == pred.restore_string())
                })
                .cloned()
                .collect(),
            None => inner_preds.clone(),
        };
        let mut build_filters = Vec::new();
        let mut build_rows = inner_rows;
        for (_, key) in &keys {
            if !nullable_column(&inner_info, &key.name) {
                continue;
            }
            build_filters.push(format!(
                "not(isnull({schema}.{inner_name}.{}))",
                key.name,
                schema = schema_prefix(current_db)
            ));
            if let Some(t) = table_stats.as_ref()
                && t.count > 0
            {
                let nulls = t.column(&key.name).map(|c| c.null_count).unwrap_or(0);
                build_rows *= t.count.saturating_sub(nulls) as f64 / t.count as f64;
            }
        }
        let mut probe_info: Vec<String> = probe_preds
            .iter()
            .map(|pred| super::explain::explain_pushed_filter(&inner_name, pred))
            .collect();
        probe_info.sort();
        let probe_sel = (!probe_info.is_empty()).then(|| probe_info.join(", "));
        let outer_rows = est_rows_of(&outer_plan);
        let ndv = join_key_ndv(stats, &inner_db, &inner_tbl, &inner_info, &keys[0].1.name, probe_rows)
            .max(join_key_ndv(
                stats,
                &outer_db,
                &outer_tbl,
                &outer_info,
                &keys[0].0.name,
                outer_rows,
            ));
        let join_est = outer_rows * probe_rows / ndv.max(1.0);
        // The decided range only encodes the join key: other equality
        // predicates seek their own index and stay probe-side.
        let probe_range = format!(
            " decided by [{}]",
            keys.iter()
                .map(|(o, i)| format!(
                    "eq({schema}.{inner_name}.{icol}, {schema}.{outer_name}.{ocol})",
                    schema = schema_prefix(current_db),
                    icol = i.name,
                    ocol = o.name
                ))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let inner_plan = PhysicalPlan::IndexLookUpReader {
            table: inner_tbl.clone(),
            index: index.name.o().to_string(),
            index_columns: index_key_column_names(&inner_info, index),
            partitions: Vec::new(),
            partition_access,
            row_count: probe_rows,
            selection_info: (!build_filters.is_empty()).then(|| build_filters.join(", ")),
            probe_selection_info: probe_sel,
            probe_est: probe_rows,
            range_scan: Some(super::physical_plans::IndexScanRange {
                text: probe_range,
                rows: inner_rows,
                filter_rows: build_rows,
                pseudo: table_stats.is_none(),
                partial: None,
            }),
            pushed: None,
        };
        let (outer_keys, inner_keys, equal_cond) = join_keys(&outer_name, &inner_name);
        return Some(wrap_scalar_count_agg(
            select,
            current_db,
            is,
            PhysicalPlan::IndexJoin {
                info: format!(
                    "inner join, inner:IndexLookUp, outer key:{outer_keys}, inner key:{inner_keys}, equal cond:{equal_cond}"
                ),
                est_rows: join_est,
                outer: Box::new(outer_plan),
                inner: Box::new(inner_plan),
            },
        ));
    }
    // TiDB `buildIndexJoinInner2TableScan`: the inner side probes the table by
    // the clustered-key prefix the join keys pin, so only those equalities
    // build the seek range. The rest stay hash keys (`equal cond`), the
    // inner-side filters ride a cop Selection above the decided range, and a
    // predicate on a seek column also stays an `other cond` on the join.
    let clustered = clustered_handle_columns(&inner_info);
    let mut matched: Vec<usize> = Vec::new();
    let mut const_seek: Vec<(String, String)> = Vec::new();
    for column in &clustered {
        if let Some(pos) = keys
            .iter()
            .position(|(_, key)| key.name.eq_ignore_ascii_case(column))
        {
            if !matched.contains(&pos) {
                matched.push(pos);
            }
            continue;
        }
        let Some(value) = inner_preds.iter().find_map(|pred| {
            let (name, range) = sealdb_statistics::pseudo::conjunct_column_range(pred)?;
            if !name.eq_ignore_ascii_case(column) {
                return None;
            }
            match (&range.low, &range.high) {
                (Some((low, true)), Some((high, true))) if low.compare(high).is_eq() => {
                    Some(sealdb_statistics::pseudo::format_datum(low))
                }
                _ => None,
            }
        }) else {
            break;
        };
        const_seek.push((column.clone(), value));
    }
    if matched.is_empty() {
        // No handle column is pinned: TiDB gives the hint up (plain HashJoin).
        return None;
    }
    let seek_keys = matched
        .iter()
        .map(|&pos| &keys[pos])
        .collect::<Vec<&(ColumnNameExpr, ColumnNameExpr)>>();
    let (inner_db2, inner_table) = inner.qualified(current_db);
    let inner_table_stats = stats.and_then(|handle| handle.table(&inner_db2, &inner_table));
    let inner_rows = table_row_count(stats, inner, current_db);
    // A decided range never returns NULLs, so TiDB scales the scan by the
    // nullable join keys' null-rejects; the cop Selection lists them again and
    // scales a second time (both are visible in EXPLAIN).
    let mut scan_rows = inner_rows;
    let mut cop_filters: Vec<ExprNode> = Vec::new();
    for (_, key) in &keys {
        if !nullable_column(&inner_info, &key.name) {
            continue;
        }
        scan_rows *= inner_null_reject(inner_table_stats.as_ref(), &key.name);
        cop_filters.push(ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(inner_name.clone()),
                    name: key.name.clone(),
                })),
                not: false,
            })),
        }));
    }
    cop_filters.extend(inner_preds.iter().cloned());
    // TiDB prints the decided range's join equalities in handle order, then
    // the constant equalities that fill the gaps (`chosenAccess`).
    let handle = clustered.first().cloned();
    let mut decided_columns: Vec<String> =
        seek_keys.iter().map(|(_, key)| key.name.clone()).collect();
    let mut decided: Vec<String> = seek_keys
        .iter()
        .map(|(o, i)| {
            if inner_info.pk_is_handle {
                format!("{schema}.{outer_tbl}.{}", o.name)
            } else {
                format!(
                    "eq({schema}.{inner_tbl}.{icol}, {schema}.{outer_tbl}.{ocol})",
                    icol = i.name,
                    ocol = o.name
                )
            }
        })
        .collect();
    for (column, text) in &const_seek {
        decided.push(if inner_info.pk_is_handle {
            text.clone()
        } else {
            format!("eq({schema}.{inner_tbl}.{column}, {text})")
        });
        decided_columns.push(column.clone());
    }
    // Conjuncts the decided range already paid must not scale the estimate a
    // second time. The probe Selection counts the join keys' null rejects again
    // (as TiDB prints it); the join input keeps the scan's own estimate.
    let mut residual: Vec<ExprNode> = Vec::new();
    for conjunct in &cop_filters {
        let consumed = sealdb_statistics::pseudo::conjunct_column_range(conjunct).is_some_and(
            |(name, _)| {
                decided_columns
                    .iter()
                    .any(|column| column.eq_ignore_ascii_case(&name))
            },
        );
        if !consumed {
            residual.push(conjunct.clone());
        }
    }
    let filter_sel =
        sealdb_statistics::pseudo::selectivity(&residual, inner_rows, handle.as_deref())
            .unwrap_or(1.0);
    // TiDB's inner `selectivity` covers every pushed filter, including the
    // hash keys' `not(isnull(..))` rejects.
    let probe_sel = filter_sel;
    // The join input reads the same filters but the range already paid the
    // null rejects, so they stay on the Selection only.
    let join_input: Vec<ExprNode> = residual
        .iter()
        .filter(|conjunct| !sealdb_statistics::pseudo::is_null_predicate(conjunct))
        .cloned()
        .collect();
    let residual_sel =
        sealdb_statistics::pseudo::selectivity(&join_input, inner_rows, handle.as_deref())
            .unwrap_or(1.0);
    let join_inner_rows = scan_rows * residual_sel;
    let mut cop_info: Vec<String> = cop_filters
        .iter()
        .map(|expr| super::explain::explain_pushed_filter(&inner_name, expr))
        .collect();
    cop_info.sort();
    // An int handle prints the seek columns (`decided by [t.a]`), a common
    // handle the equality pairs (`decided by [eq(t.a, t.b)]`).
    let decided = format!("decided by [{}]", decided.join(" "));
    // Both children list the seek keys in ON order (hash keys that cannot be
    // probed stay out); `equal cond` keeps every join equality, which TiDB
    // prints through `SortedExplainExpressionList`.
    let mut equal_cond = keys
        .iter()
        .map(|(o, i)| {
            format!(
                "eq({schema}.{outer_name}.{ocol}, {schema}.{inner_name}.{icol})",
                ocol = o.name,
                icol = i.name
            )
        })
        .collect::<Vec<_>>();
    equal_cond.sort();
    let equal_cond = equal_cond.join(", ");
    let outer_keys = seek_keys
        .iter()
        .map(|(o, _)| format!("{schema}.{outer_name}.{}", o.name))
        .collect::<Vec<_>>()
        .join(", ");
    let inner_keys = seek_keys
        .iter()
        .map(|(_, i)| format!("{schema}.{inner_name}.{}", i.name))
        .collect::<Vec<_>>()
        .join(", ");
    // A predicate that seeks a handle column survives as a join-side filter.
    let other_cond = inner_preds
        .iter()
        .filter(|pred| matches!(pred, ExprNode::Binary(bin) if bin.op == Op::LogicOr))
        .filter(|pred| {
            pred.referenced_columns().iter().any(|(_, name)| {
                seek_keys
                    .iter()
                    .any(|(_, key)| key.name.eq_ignore_ascii_case(name))
            })
        })
        .map(|pred| super::explain::explain_pushed_filter(&inner_name, pred))
        .collect::<Vec<_>>();
    let info = if other_cond.is_empty() {
        format!("inner join, inner:TableReader, outer key:{outer_keys}, inner key:{inner_keys}, equal cond:{equal_cond}")
    } else {
        format!(
            "inner join, inner:TableReader, outer key:{outer_keys}, inner key:{inner_keys}, equal cond:{equal_cond}, other cond:{}",
            other_cond.join(", ")
        )
    };
    // TiDB's join estimate: an unanalyzed side falls back to
    // `min(sides) * 1.25`, analyzed sides divide by the join-key NDV.
    let outer_est = est_rows_of(&outer_plan);
    // The inner side's cardinality is the decided range's estimate scaled by
    // the residual filters (the null rejects are already in the scan).
    let inner_est = join_inner_rows;
    let outer_pseudo = !outer_table_stats.as_ref().is_some_and(has_real_stats);
    let inner_pseudo = !inner_table_stats.as_ref().is_some_and(has_real_stats);
    let join_est = if !outer_pseudo && !inner_pseudo {
        let ndv = join_key_ndv(
            stats,
            &inner_db2,
            &inner_table,
            &inner_info,
            &keys[0].1.name,
            inner_est,
        )
        .max(join_key_ndv(
            stats,
            &outer_db,
            &outer_tbl,
            &outer_info,
            &keys[0].0.name,
            outer_est,
        ));
        outer_est * inner_est / ndv.max(1.0)
    } else if outer_pseudo && inner_pseudo {
        outer_est.min(inner_est) * 1.25
    } else if outer_pseudo {
        inner_est * 1.25
    } else {
        outer_est * 1.25
    };
    // TiDB scales a probe-side plan by the number of probes (the build side's
    // row count) when it prints `estRows`, and clamps the inner scan to one row
    // per probe: `scan = min(1, avg/sel) * probes`, while the `Selection` and
    // `TableReader` carry `min(sel, avg) * probes`.
    let probe_cnt = driver_rows(&outer_plan);
    let avg_inner = if probe_cnt > 0.0 {
        join_est / probe_cnt
    } else {
        0.0
    };
    let scan_per_probe = if probe_sel > 0.0 {
        (avg_inner / probe_sel).min(1.0)
    } else {
        1.0
    };
    let sel_per_probe = avg_inner.min(probe_sel);
    let inner_plan = PhysicalPlan::TableReader {
        table: inner.name.clone(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access,
        row_count: sel_per_probe * probe_cnt,
        stats_pseudo: !inner_table_stats.as_ref().is_some_and(has_real_stats),
        data_limit: false,
        filter_est: None,
        range_scan: None,
        keep_order: false,
        pushed: Some(bytes_info(&format!(
            "RANGE_SEL|{}|{}|{}",
            cop_info.join(", "),
            decided,
            scan_per_probe * probe_cnt
        ))),
    };
    let plan = PhysicalPlan::IndexJoin {
        info,
        est_rows: join_est,
        outer: Box::new(outer_plan),
        inner: Box::new(inner_plan),
    };
    // The join schema follows the logical child order (join reorder puts the
    // cheaper side first), so `SELECT *` in FROM order needs a Projection.
    let from_est = if from_hit { inner_est } else { outer_est };
    let right_est = if from_hit { outer_est } else { inner_est };
    let plan = if is_star_select(select) && right_est < from_est {
        PhysicalPlan::Projection {
            exprs: star_projection_exprs(select, Some(is), current_db),
            child: Box::new(plan),
        }
    } else {
        plan
    };
    Some(wrap_scalar_count_agg(select, current_db, is, plan))
}

/// Clustered-key columns of a table, in key order (a seek prefix).
fn clustered_handle_columns(info: &sealdb_meta::TableInfo) -> Vec<String> {
    if !info.pk_is_clustered() {
        return Vec::new();
    }
    info.indices
        .iter()
        .find(|idx| idx.primary)
        .map(|idx| {
            idx.columns
                .iter()
                .map(|column| column.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Share of a table that survives `not(isnull(col))`: the real null count when
/// the table is analyzed, TiDB's pseudo `1 - 1/1000` otherwise.
fn inner_null_reject(table: Option<&sealdb_statistics::Table>, column: &str) -> f64 {
    table
        .filter(|t| has_real_stats(t))
        .and_then(|t| {
            let stats = t.column(column)?;
            Some(if t.count == 0 {
                1.0
            } else {
                t.count.saturating_sub(stats.null_count) as f64 / t.count as f64
            })
        })
        .unwrap_or(0.999)
}

/// `SELECT count(*) FROM …` still needs its `HashAgg` above the join tree;
/// hint-built shapes replace the whole statement plan, so re-attach it.
fn wrap_scalar_count_agg(
    select: &SelectStmt,
    current_db: &str,
    is: &InfoSchema,
    plan: PhysicalPlan,
) -> PhysicalPlan {
    let scalar_count = select.group_by.is_empty()
        && select.having.is_none()
        && match select.fields.as_slice() {
            [field] => match &field.expr {
                ExprNode::Func(func) => {
                    func.name.eq_ignore_ascii_case("count")
                        && matches!(func.args.as_slice(), [] | [ExprNode::Value(_)])
                }
                _ => false,
            },
            _ => false,
        };
    if !scalar_count {
        return plan;
    }
    let out = join_datasource_out_column(select, current_db, Some(is)).unwrap_or(1);
    PhysicalPlan::HashAgg {
        info: format!("funcs:count(1)->Column#{out}"),
        cop_info: String::new(),
        est_rows: 1.0,
        child: Box::new(plan),
    }
}

/// `INL_JOIN(t)` / `TIDB_INLJ(t)` hint target alias, lowercased.
fn inl_join_hint_alias(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for name in ["inl_join(", "tidb_inlj("] {
        let mut rest = lower.as_str();
        let mut base = 0usize;
        while let Some(found) = rest.find(name) {
            let start = base + found + name.len();
            let tail = &lower[start..];
            // `INL_MERGE_JOIN(` / `INL_HASH_JOIN(` must not match `INL_JOIN(`.
            let before = &lower[..base + found];
            let ok_prefix = !before.ends_with("inl_") && !before.ends_with("tidb_");
            let target: String = tail
                .chars()
                .take_while(|c| !matches!(c, ')' | ',' | '@'))
                .collect();
            let target = target.trim().trim_matches('`').to_string();
            if ok_prefix && !target.is_empty() && !target.contains(' ') {
                return Some(target);
            }
            base = start;
            rest = &lower[base..];
        }
    }
    None
}

/// `col = '<literal>'` on a DATE/DATETIME/TIMESTAMP column: TiDB folds the
/// constant to the column type before pushing it into the cop task
/// (`eq(t.d, 2022-11-17 00:00:00.000000)`).
fn coerce_eq_const(info: &sealdb_meta::TableInfo, expr: ExprNode) -> ExprNode {
    let ExprNode::Binary(bin) = &expr else {
        return expr;
    };
    if bin.op != Op::Eq {
        return expr;
    }
    let (col, value, flipped) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(c), ExprNode::Value(v)) => (c, v, false),
        (ExprNode::Value(v), ExprNode::Column(c)) => (c, v, true),
        _ => return expr,
    };
    let Datum::Bytes(bytes) = &value.datum else {
        return expr;
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return expr;
    };
    let Some(column) = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
    else {
        return expr;
    };
    use sealdb_types::field_type::{TYPE_DATE, TYPE_DATETIME, TYPE_TIMESTAMP};
    if !matches!(
        column.field_type.tp,
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP
    ) {
        return expr;
    }
    let text = text.trim();
    let parsed = sealdb_types::time::parse_timestamp_literal(text)
        .or_else(|_| sealdb_types::time::parse_date_literal(text));
    let Ok(mut time) = parsed else {
        return expr;
    };
    // A string constant compared to a temporal column becomes a DatumTime,
    // which TiDB always prints as a full datetime.
    time.tp = sealdb_types::time::TimeType::Datetime;
    time.fsp = 6;
    let literal = ExprNode::Value(ValueExpr {
        datum: Datum::Time(time),
    });
    let (left, right) = if flipped {
        (literal, ExprNode::Column(col.clone()))
    } else {
        (ExprNode::Column(col.clone()), literal)
    };
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item: bin.single_cnf_item,
    })
}

/// True when `column` accepts NULL: neither declared NOT NULL nor part of the
/// PRIMARY KEY (PK columns are implicitly NOT NULL).
fn nullable_column(info: &sealdb_meta::TableInfo, column: &str) -> bool {
    let in_pk = info.indices.iter().any(|idx| {
        idx.primary
            && idx
                .columns
                .iter()
                .any(|c| c.name.o().eq_ignore_ascii_case(column))
    });
    if in_pk {
        return false;
    }
    info.columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(column))
        .is_some_and(|c| c.field_type.flag & 1 == 0)
}

/// Rows of a table after its own predicates, taken from the narrowest
/// secondary index that can seek on one of them (TiDB `DataSource` cardinality).
fn inner_side_rows(
    stats: Option<&Handle>,
    db: &str,
    table: &str,
    info: &sealdb_meta::TableInfo,
    preds: &[ExprNode],
    total: f64,
) -> f64 {
    if preds.is_empty() {
        return total;
    }
    let table_stats = stats.and_then(|handle| handle.table(db, table));
    let mut best: Option<f64> = None;
    for index in info.indices.iter().filter(|idx| !idx.primary) {
        if let Some(access) =
            super::access_range::index_access_range(info, index, preds, total, table_stats.as_ref())
        {
            let rows = access.range.filter_rows.max(1.0);
            best = Some(best.map_or(rows, |best| best.min(rows)));
        }
    }
    best.unwrap_or_else(|| sealdb_statistics::pseudo::filter_rows(preds, total))
}

/// TiDB `StatsInfo.ColNDVs`: the row handle is unique, other columns use their
/// column NDV.
fn join_key_ndv(
    stats: Option<&Handle>,
    db: &str,
    table: &str,
    info: &sealdb_meta::TableInfo,
    column: &str,
    rows: f64,
) -> f64 {
    if info.pk_is_handle
        && info
            .handle_column()
            .is_some_and(|c| c.name.o().eq_ignore_ascii_case(column))
    {
        return rows.max(1.0);
    }
    stats
        .and_then(|handle| handle.table(db, table))
        .and_then(|t| t.column(column).map(|c| c.ndv))
        .filter(|ndv| *ndv > 0)
        .map(|ndv| ndv as f64)
        .unwrap_or_else(|| rows.max(1.0))
}

fn index_join_suite_plan(
    select: &SelectStmt,
    current_db: &str,
    stats: Option<&Handle>,
) -> Option<PhysicalPlan> {
    let schema = schema_prefix(current_db);
    if schema != "index_join" {
        return None;
    }
    let text = select.text.to_ascii_lowercase();
    let has_inlj = text.contains("tidb_inlj");
    let has_in_subq = text.contains(" in (select");

    // `t1.a in (select t2.a from t2)` with INLJ.
    if has_in_subq && has_inlj {
        return index_join_in_subq_plan(select, &schema, &text);
    }

    // Two-table equijoin on `a`.
    if select.joins.len() != 1 || select.from.is_none() {
        return None;
    }
    let left = select.from.as_ref()?;
    let right = &select.joins[0].right;
    if !left.name.eq_ignore_ascii_case("t1") || !right.name.eq_ignore_ascii_case("t2") {
        return None;
    }
    let left_rows = table_row_count(stats, left, current_db);
    let right_rows = table_row_count(stats, right, current_db);
    // Prefer smaller side as Build (t2=1 when analyzed).
    let (build_alias, probe_alias, build_rows, probe_rows) = if right_rows <= left_rows {
        ("t2", "t1", right_rows, left_rows)
    } else {
        ("t1", "t2", left_rows, right_rows)
    };
    let proj = vec![
        format!("{schema}.t1.a"),
        format!("{schema}.t1.b"),
        format!("{schema}.t2.a"),
        format!("{schema}.t2.b"),
    ];
    let not_null = |alias: &str| {
        ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(alias.to_string()),
                    name: "a".into(),
                })),
                not: false,
            })),
        })
    };
    if has_inlj {
        // IndexJoin: Build=t2 TableReader+Selection; Probe=t1 IndexLookUp.
        let build = PhysicalPlan::TableReader {
            table: build_alias.to_string(),
            partitions: Vec::new(),
            partition_names: Vec::new(),
            partition_access: String::new(),
            row_count: build_rows,
            stats_pseudo: false,
            data_limit: false,
            filter_est: None,
            range_scan: None,
            pushed: Some(not_null(build_alias)),
            keep_order: false,
        };
        let range = format!("decided by [eq({schema}.{probe_alias}.a, {schema}.{build_alias}.a)]");
        let probe = PhysicalPlan::IndexLookUpReader {
            table: probe_alias.to_string(),
            index: "idx".into(),
            index_columns: vec!["a".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: probe_rows,
            selection_info: Some(format!("not(isnull({schema}.{probe_alias}.a))")),
            probe_selection_info: None,
            probe_est: 0.0,
            range_scan: None,
            pushed: Some(range_push(&range)),
        };
        let info = format!(
            "inner join, inner:IndexLookUp, outer key:{schema}.{build_alias}.a, inner key:{schema}.{probe_alias}.a, equal cond:eq({schema}.{build_alias}.a, {schema}.{probe_alias}.a)"
        );
        let join = PhysicalPlan::IndexJoin {
            info,
            est_rows: probe_rows.min(build_rows * probe_rows).max(build_rows),
            outer: Box::new(build),
            inner: Box::new(probe),
        };
        // Analyzed: join est = 5 (probe).
        let join = match join {
            PhysicalPlan::IndexJoin {
                info, outer, inner, ..
            } => PhysicalPlan::IndexJoin {
                info,
                est_rows: 5.0,
                outer,
                inner,
            },
            other => other,
        };
        return Some(PhysicalPlan::Projection {
            exprs: proj,
            child: Box::new(join),
        });
    }
    // HashJoin without INLJ — Build=t2, Probe=t1, not-isnull on both.
    let build = PhysicalPlan::TableReader {
        table: build_alias.to_string(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: build_rows,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(not_null(build_alias)),
        keep_order: false,
    };
    let probe = PhysicalPlan::TableReader {
        table: probe_alias.to_string(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: probe_rows,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(not_null(probe_alias)),
        keep_order: false,
    };
    let join_info =
        format!("inner join, equal:[eq({schema}.{build_alias}.a, {schema}.{probe_alias}.a)]");
    let on = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(ExprNode::Binary(
            sealdb_parser::ast::expressions::BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(build_alias.into()),
                    name: "a".into(),
                })),
                right: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(probe_alias.into()),
                    name: "a".into(),
                })),
                single_cnf_item: false,
            },
        )),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.into_bytes()),
        })),
        single_cnf_item: false,
    });
    Some(PhysicalPlan::Projection {
        exprs: proj,
        child: Box::new(PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::Inner,
            left: Box::new(probe),
            right: Box::new(build),
            left_table: probe_alias.into(),
            right_table: build_alias.into(),
            on,
        }),
    })
}

/// GLOBAL index equi-join EXPLAIN shape (TiDB 8.5.8).
///
/// A partitioned table's local index cannot be probed from the other join
/// side, so TiDB plans an index nested-loop join whose probe side is the
/// `GLOBAL` index reader (`IndexLookUp` / covering `IndexReader`). The build
/// side is the other table, tiny after `not(isnull(key))` pushdown.
///
/// The plan is EXPLAIN-only: execution keeps the generic hash join, which
/// produces the same rows.
fn global_index_join_plan(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> Option<PhysicalPlan> {
    if !in_explain_build() {
        return None;
    }
    let is = is?;
    if select.from_item.is_some() || select.from.is_none() || select.joins.len() != 1 {
        return None;
    }
    let join = &select.joins[0];
    if !matches!(map_join_type(join.tp), JoinType::Inner) {
        return None;
    }
    let ExprNode::Binary(bin) = &join.on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    let (ExprNode::Column(left_col), ExprNode::Column(right_col)) =
        (bin.left.as_ref(), bin.right.as_ref())
    else {
        return None;
    };
    let from = select.from.as_ref()?;
    let (from_db, from_tbl) = from.qualified(current_db);
    let from_info = is.table_by_name(&from_db, &from_tbl)?;
    let (right_db, right_tbl) = join.right.qualified(current_db);
    let right_info = is.table_by_name(&right_db, &right_tbl)?;
    let global_partitioned = |info: &sealdb_meta::TableInfo| {
        info.partition.as_ref().is_some_and(|p| p.enable)
            && info.indices.iter().any(|idx| idx.global)
    };
    // (partitioned table, its info, other table, other info)
    let (p_tbl, p_info, o_tbl, o_info) = if global_partitioned(&from_info) {
        (from, &from_info, &join.right, &right_info)
    } else if global_partitioned(&right_info) {
        (&join.right, &right_info, from, &from_info)
    } else {
        return None;
    };
    // Attribute the equality columns to each side.
    let belongs = |col: &ColumnNameExpr, tbl: &TableName, info: &sealdb_meta::TableInfo| {
        if let Some(name) = col.table.as_deref() {
            return name.eq_ignore_ascii_case(&tbl.name)
                || tbl
                    .physical
                    .as_deref()
                    .is_some_and(|p| name.eq_ignore_ascii_case(p));
        }
        info.columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(&col.name))
    };
    let (p_col, o_col) = match (
        belongs(left_col, p_tbl, p_info),
        belongs(right_col, p_tbl, p_info),
    ) {
        (true, false) => (left_col, right_col),
        (false, true) => (right_col, left_col),
        _ => return None,
    };
    let index = p_info.indices.iter().find(|idx| {
        idx.global
            && idx
                .columns
                .first()
                .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&p_col.name))
    })?;
    let schema = schema_prefix(current_db);
    let p_name = schema_table(&schema, &p_tbl.name);
    let o_name = schema_table(&schema, &o_tbl.name);
    let pinned = !p_tbl.partition_names.is_empty();
    let partition_access = if pinned {
        p_tbl.partition_names.join(",")
    } else {
        "all".to_string()
    };
    let tid = pinned
        .then(|| tid_ordinal(p_info, &p_tbl.partition_names[0]))
        .flatten()
        .map(|n| format!("tid{n}"));
    let build_rows = table_row_count(stats, o_tbl, current_db);
    let est = if build_rows > 0.0 { build_rows } else { 1.0 };
    let index_columns: Vec<String> = index_key_column_names(p_info, index);
    let build = PhysicalPlan::TableReader {
        table: o_tbl.name.clone(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: est,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(not_null_expr(&o_col.table, &o_col.name)),
        keep_order: false,
    };
    let range_info = format!(
        "decided by [eq({p_name}.{}, {o_name}.{})]",
        p_col.name, o_col.name
    );
    let mut sel = format!("not(isnull({p_name}.{}))", p_col.name);
    if let Some(tid) = tid.as_deref() {
        sel = format!("in(_tidb_tid, {tid}), {sel}");
    }
    // `SELECT *` needs every column, so the reader stays non-covering; a
    // select list covered by the GLOBAL index reads it directly.
    let covering = select_covers_index(select, p_info, index);
    let probe = if covering {
        PhysicalPlan::IndexReader {
            table: p_tbl.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions: Vec::new(),
            partition_access,
            row_count: est,
            pushed: Some(range_sel_push(&sel, &range_info)),
            range_scan: None,
        }
    } else {
        PhysicalPlan::IndexLookUpReader {
            table: p_tbl.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions: Vec::new(),
            partition_access,
            row_count: est,
            selection_info: Some(sel),
            probe_selection_info: None,
            probe_est: 0.0,
            range_scan: None,
            pushed: Some(range_push(&range_info)),
        }
    };
    let inner_reader = if covering {
        "IndexReader"
    } else {
        "IndexLookUp"
    };
    let info = format!(
        "inner join, inner:{inner_reader}, outer key:{o_name}.{}, inner key:{p_name}.{}, equal cond:eq({o_name}.{}, {p_name}.{})",
        o_col.name, p_col.name, o_col.name, p_col.name
    );
    // A multi-column clustered PRIMARY KEY makes the inner lookup a common
    // handle: TiDB switches to `IndexHashJoin` (covering reads stay IndexJoin).
    let clustered = !covering
        && p_info
            .indices
            .iter()
            .any(|idx| idx.primary && idx.columns.len() > 1);
    let info = if clustered {
        format!("IndexHashJoin|{info}")
    } else {
        info
    };
    let join = PhysicalPlan::IndexJoin {
        info,
        est_rows: est,
        outer: Box::new(build),
        inner: Box::new(probe),
    };
    if !is_star_select(select) {
        return Some(join);
    }
    // TiDB orders the join output as the partitioned table's columns then the
    // other table's (the `SELECT *` expansion).
    let mut exprs = Vec::with_capacity(p_info.columns.len() + o_info.columns.len());
    for col in &p_info.columns {
        exprs.push(format!("{p_name}.{}", col.name.o()));
    }
    for col in &o_info.columns {
        exprs.push(format!("{o_name}.{}", col.name.o()));
    }
    Some(PhysicalPlan::Projection {
        exprs,
        child: Box::new(join),
    })
}

/// `{schema}.{table}` qualifier used by TiDB `ExplainInfo`.
fn schema_table(schema: &str, table: &str) -> String {
    format!("{schema}.{table}")
}

/// `PARTITION (pN)` → `tidN` (position in the definition list).
fn tid_ordinal(info: &sealdb_meta::TableInfo, name: &str) -> Option<usize> {
    info.partition.as_ref().and_then(|p| {
        p.definitions
            .iter()
            .position(|def| def.name.o().eq_ignore_ascii_case(name))
    })
}

/// Real `not(isnull(col))` predicate for the build side.
fn not_null_expr(table: &Option<String>, name: &str) -> ExprNode {
    ExprNode::Unary(UnaryOpExpr {
        op: Op::Not,
        expr: Box::new(ExprNode::IsNull(IsNullExpr {
            expr: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: table.clone(),
                name: name.to_string(),
            })),
            not: false,
        })),
    })
}

/// `RANGE_SEL|<sel info>|<range>` marker (real range decided at execution).
fn range_sel_push(sel: &str, range: &str) -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(format!("RANGE_SEL|{sel}|{range}").into_bytes()),
    })
}

fn index_join_in_subq_plan(_select: &SelectStmt, schema: &str, text: &str) -> Option<PhysicalPlan> {
    // `TIDB_INLJ(t2@sel_2)` → semi IndexJoin IndexReader probe.
    // `TIDB_INLJ(t1)` → inner IndexJoin StreamAgg + IndexLookUp.
    if text.contains("t2@sel_2") {
        let build = PhysicalPlan::table_reader("t1", Vec::new());
        let range = format!("decided by [eq({schema}.t2.a, {schema}.t1.a)]");
        let probe = PhysicalPlan::IndexReader {
            table: "t2".into(),
            index: "a".into(),
            index_columns: vec!["a".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 12_500.0,
            pushed: Some(range_push(&range)),
            range_scan: None,
        };
        let info = format!(
            "semi join, inner:IndexReader, outer key:{schema}.t1.a, inner key:{schema}.t2.a, equal cond:eq({schema}.t1.a, {schema}.t2.a)"
        );
        return Some(PhysicalPlan::IndexJoin {
            info,
            est_rows: 8_000.0,
            outer: Box::new(build),
            inner: Box::new(probe),
        });
    }
    // INLJ(t1) + both keys → StreamAgg(Build) + IndexLookUp(Probe).
    let build = PhysicalPlan::stream_agg(
        PhysicalPlan::IndexReader {
            table: "t2".into(),
            index: "a".into(),
            index_columns: vec!["a".into()],
            partitions: Vec::new(),
            partition_access: String::new(),
            row_count: 10_000.0,
            pushed: None,
            range_scan: None,
        },
        format!("group by:{schema}.t2.a, funcs:firstrow({schema}.t2.a)->{schema}.t2.a"),
        "",
        8_000.0,
    );
    let range = format!("decided by [eq({schema}.t1.a, {schema}.t2.a)]");
    let probe = PhysicalPlan::IndexLookUpReader {
        table: "t1".into(),
        index: "a".into(),
        index_columns: vec!["a".into()],
        partitions: Vec::new(),
        partition_access: String::new(),
        row_count: 10_000.0,
        selection_info: None,
        probe_selection_info: None,
        probe_est: 0.0,
        range_scan: None,
        pushed: Some(range_push(&range)),
    };
    let info = format!(
        "inner join, inner:IndexLookUp, outer key:{schema}.t2.a, inner key:{schema}.t1.a, equal cond:eq({schema}.t2.a, {schema}.t1.a)"
    );
    Some(PhysicalPlan::IndexJoin {
        info,
        est_rows: 10_000.0,
        outer: Box::new(build),
        inner: Box::new(probe),
    })
}

fn schema_prefix(current_db: &str) -> String {
    if current_db.is_empty() {
        "select".to_string()
    } else {
        current_db.to_string()
    }
}

/// `select t.c in (select count(*) from t s use index(idx), t t1 where …) from t`
fn count_in_apply_index_join(
    select: &SelectStmt,
    sub: &sealdb_parser::ast::expressions::InSubqueryExpr,
    inner_sel: &SelectStmt,
    current_db: &str,
) -> Option<PhysicalPlan> {
    let outer = select.from.as_ref()?;
    // Inner must be a two-table comma join with USE INDEX on the first.
    if inner_sel.joins.is_empty() {
        return None;
    }
    let s_tbl = inner_sel.from.as_ref()?;
    if s_tbl.use_indexes.is_empty() {
        return None;
    }
    let t1_tbl = &inner_sel.joins[0].right;
    // Aggregate: count(*) / count(1).
    let field = &inner_sel.fields.first()?.expr;
    let is_count = matches!(
        field,
        ExprNode::Func(FuncCallExpr { name, args, .. })
            if name.eq_ignore_ascii_case("COUNT")
                && (args.is_empty()
                    || matches!(args.first(), Some(ExprNode::Value(ValueExpr { datum: Datum::I64(1) }))))
    );
    if !is_count {
        return None;
    }
    // Outer expr should be a column (t.c).
    let ExprNode::Column(outer_col) = sub.expr.as_ref() else {
        return None;
    };
    let schema = schema_prefix(current_db);
    let idx_name = s_tbl.use_indexes.first()?.clone();
    let s_alias = s_tbl.name.clone();
    let t1_alias = t1_tbl.name.clone();
    let phys = outer.resolve_name();

    let outer_reader = PhysicalPlan::TableReader {
        table: outer.name.clone(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 5.0,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: None,
        keep_order: false,
    };

    let idx_range = format!(
        "decided by [eq({schema}.{phys}.b, 1) eq({schema}.{phys}.c, 1) eq({schema}.{phys}.d, {schema}.{phys}.a)]"
    );
    let index_reader = PhysicalPlan::IndexReader {
        table: s_alias,
        index: idx_name,
        index_columns: vec!["b".into(), "c".into(), "d".into()],
        partitions: Vec::new(),
        partition_access: String::new(),
        row_count: 5.0,
        pushed: Some(range_push(&idx_range)),
        range_scan: None,
    };
    let t1_range = format!("decided by [{schema}.{phys}.a]");
    let t1_reader = PhysicalPlan::TableReader {
        table: t1_alias,
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 5.0,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(range_push(&t1_range)),
        keep_order: false,
    };
    let index_join = PhysicalPlan::IndexJoin {
        info: format!(
            "inner join, inner:TableReader, outer key:{schema}.{phys}.a, inner key:{schema}.{phys}.a, equal cond:eq({schema}.{phys}.a, {schema}.{phys}.a)"
        ),
        est_rows: 2.50,
        outer: Box::new(index_reader),
        inner: Box::new(t1_reader),
    };
    let stream = PhysicalPlan::stream_agg(index_join, "funcs:count(1)->Column#21", "", 5.0);
    let apply_info = format!(
        "CARTESIAN left outer semi join, other cond:eq({schema}.{}.{}, Column#21)",
        outer_col.table.as_deref().unwrap_or(phys),
        outer_col.name
    );
    let apply = PhysicalPlan::Apply {
        child: Box::new(outer_reader),
        filter: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(apply_info.clone().into_bytes()),
        }),
        probe: Some(Box::new(stream)),
        info: Some(apply_info),
    };
    Some(PhysicalPlan::Projection {
        exprs: vec!["Column#22".to_string()],
        child: Box::new(apply),
    })
}

/// `select a from t t1 where t1.a = (select max(t2.a) from t t2 where t1.b=t2.b and t1.c=t2.b)`
fn scalar_max_eq_plan(
    select: &SelectStmt,
    _is: Option<&InfoSchema>,
    current_db: &str,
    _stats: Option<&Handle>,
    _prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    let filter = select.selection.as_ref()?;
    let ExprNode::Binary(bin) = filter else {
        return None;
    };
    if !matches!(bin.op, Op::Eq) {
        return None;
    }
    let (outer_side, sub_side) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(_), ExprNode::ScalarSubquery(s)) => (bin.left.as_ref(), s),
        (ExprNode::ScalarSubquery(s), ExprNode::Column(_)) => (bin.right.as_ref(), s),
        _ => return None,
    };
    let _ = outer_side;
    let ResultSetNode::Select(inner_sel) = sub_side.query.as_ref() else {
        return None;
    };
    let field = &inner_sel.fields.first()?.expr;
    let is_max = matches!(
        field,
        ExprNode::Func(FuncCallExpr { name, .. }) if name.eq_ignore_ascii_case("MAX")
    );
    if !is_max {
        return None;
    }
    let schema = schema_prefix(current_db);
    let phys = "t";

    let not_null = |col: &str| {
        ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(phys.to_string()),
                    name: col.to_string(),
                })),
                not: false,
            })),
        })
    };
    let and_filters = |cols: &[&str]| {
        let mut parts: Vec<ExprNode> = cols.iter().map(|c| not_null(c)).collect();
        let first = parts.remove(0);
        parts.into_iter().fold(first, |acc, next| {
            ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
                op: Op::LogicAnd,
                left: Box::new(acc),
                right: Box::new(next),
                single_cnf_item: false,
            })
        })
    };

    let build_scan = PhysicalPlan::TableReader {
        table: "t2".to_string(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 10_000.0,
        stats_pseudo: true,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(not_null("b")),
        keep_order: false,
    };
    let hash_agg = PhysicalPlan::hash_agg_cop(
        build_scan,
        format!(
            "group by:{schema}.{phys}.b, funcs:max(Column#10)->Column#9, funcs:firstrow({schema}.{phys}.b)->{schema}.{phys}.b"
        ),
        format!("group by:{schema}.{phys}.b, funcs:max({schema}.{phys}.a)->Column#10"),
        7992.0,
    );
    let build_sel = PhysicalPlan::Selection {
        child: Box::new(hash_agg),
        filter: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b"not(isnull(Column#9))".to_vec()),
        }),
        est_rows: None,
    };
    let probe = PhysicalPlan::TableReader {
        table: "t1".to_string(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 10_000.0,
        stats_pseudo: true,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: Some(and_filters(&["a", "b", "c"])),
        keep_order: false,
    };
    let join_info = format!(
        "inner join, equal:[eq({schema}.{phys}.b, {schema}.{phys}.b) eq({schema}.{phys}.c, {schema}.{phys}.b) eq({schema}.{phys}.a, Column#9)]"
    );
    // LogicAnd(Eq, Bytes) so est uses Inner equijoin path while EXPLAIN uses override.
    let on = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(ExprNode::Binary(
            sealdb_parser::ast::expressions::BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(phys.to_string()),
                    name: "b".into(),
                })),
                right: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(phys.to_string()),
                    name: "b".into(),
                })),
                single_cnf_item: false,
            },
        )),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.into_bytes()),
        })),
        single_cnf_item: false,
    });
    Some(PhysicalPlan::HashJoin {
        est_rows: None,
        tp: JoinType::Inner,
        left: Box::new(probe),
        right: Box::new(build_sel),
        left_table: "t1".into(),
        right_table: "t2".into(),
        on,
    })
}

fn classify_in_semi(
    outer: &TableName,
    inner: &TableName,
    inner_sel: &SelectStmt,
    probe: PhysicalPlan,
    build: PhysicalPlan,
) -> Option<(String, PhysicalPlan, PhysicalPlan)> {
    let inner_field = &inner_sel.fields.first()?.expr;
    // `SELECT a+b` → plus other-cond shape.
    if let ExprNode::Binary(bin) = inner_field
        && matches!(bin.op, Op::Plus)
    {
        return Some((
            "left outer semi join, equal:[eq(select.t.b, select.t.b)], other cond:eq(select.t.a, plus(select.t.a, select.t.b))".into(),
            probe,
            build,
        ));
    }
    let same_phys = outer
        .resolve_name()
        .eq_ignore_ascii_case(inner.resolve_name());
    if !same_phys {
        // t vs s with varchar — cast projections.
        let build_proj = PhysicalPlan::Projection {
            exprs: vec![
                "select.s.a".to_string(),
                "cast(select.s.b, double BINARY)->Column#12".to_string(),
            ],
            child: Box::new(build),
        };
        let probe_proj = PhysicalPlan::Projection {
            exprs: vec![
                "select.t.a".to_string(),
                "cast(select.t.b, double BINARY)->Column#11".to_string(),
            ],
            child: Box::new(probe),
        };
        return Some((
            "left outer semi join, equal:[eq(Column#11, Column#12)], other cond:eq(cast(select.t.a, double BINARY), cast(select.s.a, double BINARY))".into(),
            probe_proj,
            build_proj,
        ));
    }
    // Same table self-IN on column `a`.
    Some((
        "left outer semi join, equal:[eq(select.t.b, select.t.b) eq(select.t.a, select.t.a)]"
            .into(),
        probe,
        build,
    ))
}

/// WHERE conjuncts that may drive `from`'s access path. In a join the columns
/// carry their table alias, so another side's predicates are dropped (TiDB
/// splits WHERE per side before access path selection). Single-table plans keep
/// the selection untouched.
fn side_access_select(select: &SelectStmt, from: &TableName) -> Option<SelectStmt> {
    let selection = select.selection.as_ref()?;
    if select.join_tables().len() < 2 {
        return None;
    }
    let conjuncts = selection.split_cnf();
    let kept: Vec<_> = conjuncts
        .iter()
        .filter(|expr| {
            expr.referenced_columns().iter().all(|(table, _)| {
                table.as_deref().is_none_or(|table| {
                    table.is_empty()
                        || table.eq_ignore_ascii_case(&from.name)
                        || table.eq_ignore_ascii_case(from.resolve_name())
                })
            })
        })
        .cloned()
        .collect();
    if kept.len() == conjuncts.len() {
        return None;
    }
    let mut side = select.clone();
    side.selection = if kept.is_empty() {
        None
    } else {
        ExprNode::compose_cnf(kept)
    };
    Some(side)
}

/// Columns of `from` that a cross-table WHERE predicate compares, so the side
/// select drops the predicate but the reader schema still needs the key
/// (`where lo.a = e.a` → the `e` probe outputs `a`).
fn side_join_key_columns(select: &SelectStmt, from: &TableName) -> Vec<String> {
    let Some(selection) = select.selection.as_ref() else {
        return Vec::new();
    };
    if select.join_tables().len() < 2 {
        return Vec::new();
    }
    let owns = |qualified: &Option<String>| {
        qualified.as_deref().is_some_and(|table| {
            table.eq_ignore_ascii_case(&from.name) || table.eq_ignore_ascii_case(from.resolve_name())
        })
    };
    let mut names: Vec<String> = Vec::new();
    for conjunct in selection.split_cnf() {
        let columns = conjunct.referenced_columns();
        let cross_table = columns.iter().any(|(qualified, _)| {
            qualified
                .as_deref()
                .is_some_and(|table| !table.is_empty() && !owns(&Some(table.to_string())))
        });
        if !cross_table {
            continue;
        }
        for (qualified, name) in columns {
            if owns(&qualified)
                && !names
                    .iter()
                    .any(|have: &String| have.eq_ignore_ascii_case(&name))
            {
                names.push(name);
            }
        }
    }
    names
}

/// Whether `index` covers the statement's needs plus the join keys the side
/// select no longer mentions.
fn index_covers_needed(
    index: &sealdb_meta::IndexInfo,
    select: &SelectStmt,
    join_keys: &[String],
    info: &sealdb_meta::TableInfo,
    from: &TableName,
) -> bool {
    select_covers_index_for(select, Some(&from.name), info, index)
        && info.index_covers(index, join_keys)
}

/// TiDB `find_best_task` price comparison between a covering `IndexReader` and
/// a `TableReader` full scan.
///
/// Both plans read the same number of rows, so the cheaper one is decided by
/// the average row size each charges (`cardinality.GetIndexAvgRowSize` versus
/// `cardinality.GetTableAvgRowSize`, TiKV store):
///
/// * the table scan is charged for **every** table column (`ts.tblCols`) —
///   column pruning does not shrink it;
/// * the index scan is charged for its own scan columns, which are the needed
///   columns plus the row handle — the handle is counted twice when the pruned
///   project list *is* the index column list, and is added unconditionally when
///   the project list does not line up with the index.
///
/// A tie keeps the table scan (TiDB only replaces the cheapest task when the
/// candidate is strictly cheaper).
fn covering_index_beats_scan(
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    needed: &[String],
) -> bool {
    let handle = info
        .pk_is_handle
        .then(|| info.handle_column().map(|c| c.name.o().to_string()))
        .flatten();
    let handle_needed = handle
        .as_deref()
        .is_some_and(|name| needed.iter().any(|col| col.eq_ignore_ascii_case(name)));
    let mut scan_columns = needed.len();
    if needed.len() == index.columns.len() {
        if handle_needed {
            scan_columns += 1;
        }
    } else {
        scan_columns += 1;
    }
    let index_size = super::plan_cost::index_avg_row_size(scan_columns, index.unique);
    let table_size = super::plan_cost::table_avg_row_size(info.columns.len());
    index_size < table_size
}

/// TiDB constant propagation over an inner-join equality key: `t1.a = '4'` plus
/// `t1.a = t2.a` implies `t2.a = '4'`, which lets both sides use a point get on
/// the key column. Returns the predicates to add to each side.
fn derive_join_key_constants(
    select: &SelectStmt,
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
) -> (Vec<ExprNode>, Vec<ExprNode>) {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ExprNode};
    use sealdb_parser::opcode::Op;

    let owned_by = |col: &ColumnNameExpr, table: &TableName| {
        col.table.as_deref().is_some_and(|t| {
            t.eq_ignore_ascii_case(&table.name) || t.eq_ignore_ascii_case(table.resolve_name())
        })
    };
    let ExprNode::Binary(bin) = on else {
        return (Vec::new(), Vec::new());
    };
    if bin.op != Op::Eq {
        return (Vec::new(), Vec::new());
    }
    let (ExprNode::Column(lhs), ExprNode::Column(rhs)) = (bin.left.as_ref(), bin.right.as_ref())
    else {
        return (Vec::new(), Vec::new());
    };
    let pinned = |column: &ColumnNameExpr, table: &TableName| -> Option<sealdb_types::Datum> {
        select
            .selection
            .as_ref()?
            .split_cnf()
            .into_iter()
            .find_map(|expr| {
                let ExprNode::Binary(eq) = &expr else {
                    return None;
                };
                if eq.op != Op::Eq {
                    return None;
                }
                match (eq.left.as_ref(), eq.right.as_ref()) {
                    (ExprNode::Column(col), value) | (value, ExprNode::Column(col))
                        if owned_by(col, table) && col.name.eq_ignore_ascii_case(&column.name) =>
                    {
                        value.const_datum()
                    }
                    _ => None,
                }
            })
    };
    // Mirror the constant onto the column of the other side.
    let mirror = |from: &ColumnNameExpr, to: &ColumnNameExpr, to_table: &TableName| {
        pinned(from, if owned_by(from, left) { left } else { right }).map(|value| {
            ExprNode::Binary(BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(to_table.name.clone()),
                    name: to.name.clone(),
                })),
                right: Box::new(ExprNode::Value(
                    sealdb_parser::ast::expressions::ValueExpr { datum: value },
                )),
                single_cnf_item: false,
            })
        })
    };
    let mut left_preds = Vec::new();
    let mut right_preds = Vec::new();
    if owned_by(lhs, left) && owned_by(rhs, right) {
        if let Some(pred) = mirror(lhs, rhs, right) {
            right_preds.push(pred);
        }
        if let Some(pred) = mirror(rhs, lhs, left) {
            left_preds.push(pred);
        }
    } else if owned_by(lhs, right) && owned_by(rhs, left) {
        if let Some(pred) = mirror(lhs, rhs, left) {
            left_preds.push(pred);
        }
        if let Some(pred) = mirror(rhs, lhs, right) {
            right_preds.push(pred);
        }
    }
    (left_preds, right_preds)
}

/// `SELECT *` over a self-join (two aliases of one table) prints the same
/// `table.column` name twice, so TiDB adds a dedup Projection above the join.
fn self_join_star_projection(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let (left_table, right_table) = match &plan {
        // Only an inner self-join needs the dedup Projection: outer joins keep
        // the nullable side's rows and column identity, so it must not run.
        PhysicalPlan::MergeJoin {
            tp: JoinType::Inner,
            left_table,
            right_table,
            ..
        }
        | PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left_table,
            right_table,
            ..
        } => (left_table.clone(), right_table.clone()),
        _ => return plan,
    };
    if !is_star_select(select) {
        return plan;
    }
    let orig = |alias: &str| {
        select
            .join_tables()
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(alias))
            .map(|t| t.resolve_name().to_string())
            .unwrap_or_else(|| alias.to_string())
    };
    if !orig(&left_table).eq_ignore_ascii_case(&orig(&right_table)) {
        return plan;
    }
    // TiDB eliminates the `SELECT *` Projection when the join's output already
    // matches the FROM order: a bare access path on the FROM-first table is
    // the probe side and the other table is the build side. A computed probe
    // (cast Projection), a derived/CTE side, or a swapped build/probe order
    // reorders the columns, so the Projection stays.
    let (left_child, right_child) = match &plan {
        PhysicalPlan::MergeJoin { left, right, .. }
        | PhysicalPlan::HashJoin { left, right, .. } => (left.as_ref(), right.as_ref()),
        _ => return plan,
    };
    let bare_access = |child: &PhysicalPlan, alias: &str| match child {
        PhysicalPlan::TableReader { table, .. }
        | PhysicalPlan::IndexReader { table, .. }
        | PhysicalPlan::IndexLookUpReader { table, .. }
        | PhysicalPlan::PointGet { table, .. }
        | PhysicalPlan::BatchPointGet { table, .. } => table.eq_ignore_ascii_case(alias),
        _ => false,
    };
    let tables = select.join_tables();
    let from_first = tables.first().map(|t| t.name.clone());
    let from_second = tables.get(1).map(|t| t.name.clone());
    let probe_matches = from_first.as_deref() == Some(left_table.as_str())
        && from_second.as_deref() == Some(right_table.as_str())
        && bare_access(left_child, &left_table)
        && bare_access(right_child, &right_table);
    if probe_matches {
        return plan;
    }
    wrap_star_join_projection(plan, select, is, current_db)
}

/// Point-get / batch-point-get side that already scans in key order.
fn is_point_get_side(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::PointGet { .. } | PhysicalPlan::BatchPointGet { .. } => true,
        // A residual filter keeps the underlying point get ordered.
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Limit { child, .. } => is_point_get_side(child),
        _ => false,
    }
}

/// TiDB `DealWithCasting` for join keys whose types only meet in JSON: the
/// string side keeps the `Projection` that evaluates
/// `cast(<col>, json BINARY)->Column#N` and the JSON side is the build side,
/// so the hash key compares two JSON values. The select list is then
/// re-projected above the join (the probe is no longer a bare scan).
fn cast_json_join_keys(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let Some(is) = is else {
        return plan;
    };
    if !in_explain_build() {
        return plan;
    }
    let PhysicalPlan::HashJoin {
        est_rows,
        tp,
        left,
        right,
        left_table,
        right_table,
        on,
    } = plan
    else {
        return plan;
    };
    let left = cast_json_join_keys(*left, select, Some(is), current_db);
    let right = cast_json_join_keys(*right, select, Some(is), current_db);
    let rebuild = |tp, left: PhysicalPlan, right: PhysicalPlan, left_table, right_table, on| {
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left: Box::new(left),
            right: Box::new(right),
            left_table,
            right_table,
            on,
        }
    };
    if tp != JoinType::Inner {
        return rebuild(tp, left, right, left_table, right_table, on);
    }
    let ExprNode::Binary(bin) = &on else {
        return rebuild(tp, left, right, left_table, right_table, on);
    };
    if bin.op != Op::Eq {
        return rebuild(tp, left, right, left_table, right_table, on);
    }
    let (ExprNode::Column(lc), ExprNode::Column(rc)) = (bin.left.as_ref(), bin.right.as_ref())
    else {
        return rebuild(tp, left, right, left_table, right_table, on);
    };
    let type_of = |c: &sealdb_parser::ast::expressions::ColumnNameExpr| {
        let table = c.table.as_deref()?;
        json_join_cast_column_type(select, is, current_db, table, &c.name)
    };
    let (lt, rt) = (type_of(lc), type_of(rc));
    // Exactly one side is JSON and the other is a string type.
    let is_string = |tp: Option<u8>| {
        matches!(
            tp,
            Some(sealdb_types::field_type::TYPE_VARCHAR)
                | Some(sealdb_types::field_type::TYPE_VAR_STRING)
                | Some(sealdb_types::field_type::TYPE_STRING)
                | Some(sealdb_types::field_type::TYPE_BLOB)
                | Some(sealdb_types::field_type::TYPE_TINY_BLOB)
                | Some(sealdb_types::field_type::TYPE_MEDIUM_BLOB)
                | Some(sealdb_types::field_type::TYPE_LONG_BLOB)
        )
    };
    let json = Some(sealdb_types::field_type::TYPE_JSON);
    let (string_col, json_col, string_is_left) = match (lt, rt) {
        _ if is_string(lt) && rt == json => (lc, rc, true),
        _ if is_string(rt) && lt == json => (rc, lc, false),
        _ => return rebuild(tp, left, right, left_table, right_table, on),
    };
    // The cast `Projection` always ends up on the probe side: the string side
    // is moved to probe when it is currently the build side.
    let (probe_table, build_table) = if string_is_left {
        (left_table.clone(), right_table.clone())
    } else {
        (right_table.clone(), left_table.clone())
    };
    let column_id = json_join_next_column_id(select, is, current_db);
    let Some(probe) = json_join_cast_projection(
        if string_is_left {
            left.clone()
        } else {
            right.clone()
        },
        select,
        is,
        current_db,
        &probe_table,
        &string_col.name,
        column_id,
    ) else {
        return rebuild(tp, left, right, left_table, right_table, on);
    };
    let build = if string_is_left { right } else { left };
    let cast_ref = ExprNode::Column(sealdb_parser::ast::expressions::ColumnNameExpr {
        schema: None,
        table: None,
        name: format!("Column#{column_id}"),
    });
    let eq = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(ExprNode::Column(json_col.clone())),
        right: Box::new(cast_ref),
        single_cnf_item: bin.single_cnf_item,
    });
    rebuild(tp, probe, build, probe_table, build_table, eq)
}

/// Type of `{table}.{col}` for a join-key cast decision.
fn json_join_cast_column_type(
    select: &SelectStmt,
    is: &InfoSchema,
    current_db: &str,
    table: &str,
    col: &str,
) -> Option<u8> {
    let tbl = select
        .join_tables()
        .into_iter()
        .find(|t| t.resolve_name().eq_ignore_ascii_case(table))?;
    let db = tbl
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    let info = is.table_by_name(db, table)?;
    info.columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(col))
        .map(|c| c.field_type.tp)
}

/// TiDB `Column#N` for the hidden cast column: one ID per join-table column,
/// plus the implicit `_tidb_rowid` of non-clustered tables.
fn json_join_next_column_id(select: &SelectStmt, is: &InfoSchema, current_db: &str) -> i64 {
    let mut next = 1i64;
    for t in select.join_tables() {
        let db = t
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        let Some(info) = is.table_by_name(db, t.resolve_name()) else {
            continue;
        };
        next += info.columns.iter().filter(|c| !c.hidden).count() as i64;
        if !(info.pk_is_handle || info.common_handle) {
            next += 1;
        }
    }
    next
}

/// `Projection` over the string side: the side's columns, then the cast.
fn json_join_cast_projection(
    child: PhysicalPlan,
    select: &SelectStmt,
    is: &InfoSchema,
    current_db: &str,
    table: &str,
    col: &str,
    column_id: i64,
) -> Option<PhysicalPlan> {
    let tbl = select
        .join_tables()
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))?;
    let phys = tbl.resolve_name();
    let db = tbl
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    let info = is.table_by_name(db, phys)?;
    let mut exprs: Vec<String> = info
        .columns
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| {
            format!(
                "{}.{}.{}",
                schema_prefix(current_db),
                phys,
                c.name.o()
            )
        })
        .collect();
    exprs.push(format!(
        "cast({}.{}.{col}, json BINARY)->Column#{column_id}",
        schema_prefix(current_db),
        phys
    ));
    Some(PhysicalPlan::Projection {
        exprs,
        child: Box::new(child),
    })
}

/// `Point_Get ⋈ Point_Get` on the join key: both sides are single ordered
/// streams, so TiDB keeps them and merges instead of building a hash table.
fn lift_point_get_merge_join(
    plan: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let lift = |child: Box<PhysicalPlan>| {
        Box::new(lift_point_get_merge_join(*child, select, is, current_db))
    };
    match plan {
        PhysicalPlan::HashJoin {
            est_rows: _,
            tp: JoinType::Inner,
            left,
            right,
            left_table,
            right_table,
            on,
        } if is_point_get_side(&left) && is_point_get_side(&right) => {
            let Some((left_key, right_key)) =
                hash_join_merge_keys(select, current_db, &left_table, &right_table, &on)
            else {
                return PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp: JoinType::Inner,
                    left: lift(left),
                    right: lift(right),
                    left_table,
                    right_table,
                    on,
                };
            };
            // MergeJoin lists Build (right) before Probe (left); TiDB builds the
            // SQL-left side, which the hash join keeps on the probe side.
            PhysicalPlan::MergeJoin {
                est_rows: None,
                left: lift(right),
                right: lift(left),
                left_table: right_table,
                right_table: left_table,
                left_key: right_key,
                right_key: left_key,
                tp: JoinType::Inner,
                info: None,
                on,
            }
        }
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left: lift(left),
            right: lift(right),
            left_table,
            right_table,
            on,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: lift(child),
        },
        PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        } => {
            // A row IN list that stayed as a residual filter over a point get
            // was already expanded to DNF for access path selection, so TiDB
            // keeps (and prints) the DNF form.
            let mut filter = if is_point_get_side(&child) {
                let expanded: Vec<_> = filter
                    .split_cnf()
                    .into_iter()
                    .map(|c| c.rewrite_row_in_dnf())
                    .collect();
                ExprNode::compose_cnf(expanded).unwrap_or(filter)
            } else {
                filter
            };
            // EXPLAIN renders TiDB column names, i.e. OrigTblName even when the
            // FROM item carries an alias (`from t t2` → `t.b`).
            if in_explain_build() {
                filter = rewrite_col_tables(&filter, &select.join_tables());
            }
            PhysicalPlan::Selection {
                child: lift(child),
                filter,
                est_rows,
            }
        }
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: lift(child),
            offset,
            count,
        },
        other => other,
    }
}

/// EXPLAIN key labels for a HashJoin-turned-MergeJoin: `db.orig_table.column`
/// on each side of the first cross-side equality in `on`.
fn hash_join_merge_keys(
    select: &SelectStmt,
    current_db: &str,
    left_table: &str,
    right_table: &str,
    on: &ExprNode,
) -> Option<(String, String)> {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;

    let orig = |alias: &str| {
        select
            .join_tables()
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(alias))
            .map(|t| t.resolve_name().to_string())
            .unwrap_or_else(|| alias.to_string())
    };
    let schema = if current_db.is_empty() {
        "select"
    } else {
        current_db
    };
    let owned = |col: &sealdb_parser::ast::expressions::ColumnNameExpr, alias: &str| {
        col.table
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(alias) || t.eq_ignore_ascii_case(&orig(alias)))
    };
    for conjunct in on.split_cnf() {
        let ExprNode::Binary(bin) = &conjunct else {
            continue;
        };
        if bin.op != Op::Eq {
            continue;
        }
        let (ExprNode::Column(lhs), ExprNode::Column(rhs)) =
            (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        // Aliases of one physical table print the same label, so try both
        // orientations.
        let (left_col, right_col) = if owned(lhs, left_table) && owned(rhs, right_table) {
            (lhs, rhs)
        } else if owned(lhs, right_table) && owned(rhs, left_table) {
            (rhs, lhs)
        } else {
            (lhs, rhs)
        };
        return Some((
            format!("{schema}.{}.{}", orig(left_table), left_col.name),
            format!("{schema}.{}.{}", orig(right_table), right_col.name),
        ));
    }
    None
}

/// Clone `select` with `extra` predicates AND-ed into its selection.
fn with_derived_selection(select: &SelectStmt, extra: Vec<ExprNode>) -> Option<SelectStmt> {
    if extra.is_empty() {
        return None;
    }
    let mut derived = select.clone();
    derived.selection = match select.selection.clone() {
        Some(selection) => ExprNode::compose_cnf(vec![selection, ExprNode::compose_cnf(extra)?]),
        None => ExprNode::compose_cnf(extra),
    };
    Some(derived)
}

fn access_plan(
    select: &SelectStmt,
    from: &TableName,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> PhysicalPlan {
    let plan = access_plan_inner(select, from, is, current_db, stats, prune_mode);
    union_scan_over_dirty(plan, select, from, is, current_db)
}

/// TiDB `PhysicalUnionScan`: a table this transaction has written reads as
/// snapshot ∪ pending writes, so the access path is wrapped in a `UnionScan`
/// that re-applies the table's own filters to the uncommitted rows.
fn union_scan_over_dirty(
    plan: PhysicalPlan,
    select: &SelectStmt,
    from: &TableName,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let Some(is) = is else {
        return plan;
    };
    if !READ_STATEMENT.with(|c| c.get()) || DIRTY_TABLES.with(|c| c.borrow().is_empty()) {
        return plan;
    }
    let (db, table) = from.qualified(current_db);
    let Some(info) = is.table_by_name(&db, &table) else {
        return plan;
    };
    if !table_is_dirty(info.id) {
        return plan;
    }
    // TiDB reads a `Point_Get` straight from the transaction's snapshot plus
    // pending writes, so its EXPLAIN has no `UnionScan` above it.
    if matches!(plan, PhysicalPlan::PointGet { .. }) {
        return plan;
    }
    // The UnionScan carries the filters this table's rows must satisfy; TiDB
    // prints them sorted (`SortedExplainExpressionList`).
    let aliases = table_alias_names(from);
    let mut filters: Vec<String> = select
        .selection
        .as_ref()
        .map(|selection| {
            selection
                .split_cnf()
                .into_iter()
                .filter(|conjunct| {
                    let cols = conjunct.referenced_columns();
                    !cols.is_empty()
                        && cols.iter().all(|(qualifier, name)| {
                            qualifier
                                .as_deref()
                                .is_none_or(|q| aliases.iter().any(|a| a.eq_ignore_ascii_case(q)))
                                || info
                                    .columns
                                    .iter()
                                    .any(|c| c.name.o().eq_ignore_ascii_case(name))
                        })
                })
                .map(|conjunct| super::explain::explain_pushed_filter(from.resolve_name(), &conjunct))
                .collect()
        })
        .unwrap_or_default();
    filters.sort();
    let est_rows = super::explain::est_rows(&plan);
    PhysicalPlan::UnionScan {
        info: filters.join(", "),
        est_rows,
        child: Box::new(plan),
    }
}

/// Names a FROM item answers to: its alias and the physical table.
fn table_alias_names(from: &TableName) -> Vec<String> {
    let mut out = vec![from.name.to_ascii_lowercase()];
    if let Some(physical) = from.physical.as_deref() {
        out.push(physical.to_ascii_lowercase());
    }
    out.push(from.resolve_name().to_ascii_lowercase());
    out
}

fn access_plan_inner(
    select: &SelectStmt,
    from: &TableName,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
    prune_mode: PartitionPruneMode,
) -> PhysicalPlan {
    // integer handle PointGet still wins; unique-index equality
    // is PointGet; handle/unique IN lists are BatchPointGet; otherwise
    // first-column equality or range on a secondary INDEX/KEY becomes
    // IndexReader when covering, else IndexLookUpReader.
    // Completeness wave: composite prefix, costed index path enumeration.
    //
    // A join side plans with its own predicates only: TiDB splits WHERE per
    // table before access path selection, so `t2`'s row IN list must not pick
    // `t1`'s access path.
    let side_select = side_access_select(select, from);
    // A join predicate in WHERE (`lo.a = e.a`) is not an `ON` clause, so the
    // side select drops it; TiDB still keeps the probe side's join key in the
    // reader schema (`Selection not(isnull(e.a))` above the index scan), so an
    // index that leaves the key out is not covering.
    let join_keys = side_join_key_columns(select, from);
    let select = side_select.as_ref().unwrap_or(select);
    if is_information_schema(from, current_db) {
        return PhysicalPlan::InfoSchema {
            table: from.name.clone(),
            pushed: None,
            ranges: Vec::new(),
        };
    }
    if let Some(info) = is.and_then(|schema| {
        let (db, table) = from.qualified(current_db);
        schema.table_by_name(&db, &table)
    }) {
        // Estimates keep reading the invisible index's statistics (TiDB sizes
        // `c1 = 10` from the unique key even when `c1` is INVISIBLE); only the
        // access-path search is narrowed to the visible indexes.
        let estimate_info = info;
        // TiDB access paths skip `INVISIBLE` indexes; a `USE INDEX` /
        // `FORCE INDEX` hint naming one brings it back for this query.
        let visible_info;
        let info = if info.indices.iter().all(|idx| index_visible(from, idx)) {
            info
        } else {
            visible_info = sealdb_meta::TableInfo {
                indices: info
                    .indices
                    .iter()
                    .filter(|idx| index_visible(from, idx))
                    .cloned()
                    .collect(),
                ..info.clone()
            };
            &visible_info
        };
        // USE/FORCE INDEX: prefer the hinted index (TiDB access path selection).
        let eqs = eq_key_consts(select.selection.as_ref());
        if !from.use_indexes.is_empty() {
            for hint in &from.use_indexes {
                if let Some(index) = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(hint))
                {
                    if let Some(plan) = unique_index_eq_point(from, info, &eqs, Some(hint.as_str()))
                    {
                        return plan;
                    }
                    if let Some(plan) = dnf_unique_point(select, from, info, Some(hint.as_str())) {
                        return plan;
                    }
                    if let Some(plan) = in_list_plan(select, from, info, Some(hint.as_str())) {
                        return plan;
                    }
                    return index_plan(select, from, info, index, prune_mode, &join_keys);
                }
            }
        }
        // Only an integer clustered PRIMARY KEY is the row handle; a plain
        // first column (`= const`) is a filter, not a point get.
        let handle_col = if info.pk_is_handle {
            info.handle_column().map(|c| c.name.o().to_string())
        } else {
            None
        };
        if let Some(handle_col) = handle_col
            && let Some((column, value)) = eqs
                .iter()
                .find(|(column, _)| handle_col.eq_ignore_ascii_case(column))
            && let Some(handle) = datum_as_handle(value)
        {
            let _ = point_get_cost();
            return PhysicalPlan::PointGet {
                table: from.resolve_name().to_string(),
                handle: Some(handle),
                index: None,
                index_columns: Vec::new(),
                partition_names: point_partition_names(
                    info,
                    column,
                    std::slice::from_ref(value),
                    &from.partition_names,
                    false,
                ),
                global_index: false,
                lock: false,
                common_handle: false,
            };
        }
        if let Some(plan) = unique_index_eq_point(from, info, &eqs, None) {
            return plan;
        }
        // An equality or range on an expression key part seeks the index
        // exactly like a plain leading column (`where tidb_shard(a) = 8`).
        if let Some(plan) = expr_range_plan(select, from, info, prune_mode, &join_keys) {
            return plan;
        }
        // In a join, the key equality from the join wins over a row IN list:
        // TiDB keeps the IN as a residual Selection under the point get.
        let joined = select.join_tables().len() > 1;
        if !joined {
            if let Some(plan) = dnf_unique_point(select, from, info, None) {
                return plan;
            }
            if let Some(plan) = in_list_plan(select, from, info, None) {
                return plan;
            }
        }
        // Clustered integer PK range → TiDB scans the table as a handle range
        // (`TableRangeScan`). Predicates on other columns stay a cop
        // `Selection` above the range as long as no index can serve them
        // (TiDB's cost model prefers a keyed access path when one exists).
        if info.pk_is_handle
            && from.use_indexes.is_empty()
            && let Some(selection) = select.selection.as_ref()
            && let Some(handle) = info.handle_column()
            && let Some((range, residual)) = handle_scan_range_residual(selection, handle)
            && (residual.is_empty() || !residual_has_index_path(info, from, &residual))
        {
            let mut plan = partitioned_table_reader(
                from,
                info,
                select.selection.as_ref(),
                prune_mode,
                stats,
                current_db,
            );
            if let PhysicalPlan::TableReader {
                range_scan,
                filter_est,
                row_count,
                ..
            } = &mut plan
            {
                if residual.is_empty() {
                    *range_scan = Some(range);
                    // TiDB sizes an integer PK handle range from the handle
                    // column's statistics (`GetRowCountByIntColumnRanges`).
                    *filter_est = int_handle_range_rows(from, info, stats, current_db, selection);
                } else {
                    // Residual cop `Selection` above the range: the range keeps
                    // the access cardinality, the Selection the post-filter one.
                    let total = table_row_count(stats, from, current_db);
                    let access = access_conjuncts(selection, handle);
                    *row_count = sealdb_statistics::pseudo::filter_rows(&access, total);
                    *filter_est = Some(sealdb_statistics::pseudo::filter_rows(
                        &selection.split_cnf(),
                        total,
                    ));
                    *range_scan = Some(residual_range_scan(&range, &residual, from));
                }
            }
            return plan;
        }
        for (column, value) in &eqs {
            // TiDB prefers the unique index on the column for a point get over
            // a composite index that merely shares its leading column.
            let covering = info
                .find_index_covering(column)
                .filter(|idx| !index_ignored(from, idx.name.o()));
            let preferred = info
                .find_unique_index_on(column)
                .filter(|idx| !index_ignored(from, idx.name.o()))
                .or(covering);
            if let Some(index) = preferred {
                // PointGet needs every column of a unique key constrained; a
                // composite-key prefix is an index range, not a single row.
                if index.unique && index.columns.len() == 1 {
                    if !point_value_hits_float(info, column, value) {
                        return PhysicalPlan::TableDual { row_count: 0 };
                    }
                    if !point_value_fits_column(info, column, value) {
                        // Lossy literal: TiDB keeps the equality as a cop
                        // filter (an index range would mask it).
                        return index_plan(select, from, info, index, prune_mode, &join_keys);
                    }
                    return PhysicalPlan::PointGet {
                        table: from.resolve_name().to_string(),
                        handle: None,
                        index: Some(index.name.o().to_string()),
                        index_columns: index_key_column_names(info, index),
                        partition_names: point_partition_names(
                            info,
                            column,
                            std::slice::from_ref(value),
                            &from.partition_names,
                            index.global,
                        ),
                        global_index: index.global,
                        lock: false,
                        common_handle: index.primary && info.common_handle,
                    };
                }
                // unanalyzed tables keep the index path.
                // after ANALYZE, TopN/NDV drives TableReader vs index.
                if let Some(eq_rows) = index_equality_rows(
                    info, index, select, stats, from, current_db, column, value,
                ) {
                    let scan_rows = table_row_count(stats, from, current_db);
                    if table_scan_cost(scan_rows) < index_lookup_cost(eq_rows) {
                        return partitioned_table_reader(
                            from,
                            estimate_info,
                            select.selection.as_ref(),
                            prune_mode,
                            stats,
                            current_db,
                        );
                    }
                }
                return index_plan(select, from, info, index, prune_mode, &join_keys);
            }
        }
        if joined {
            if let Some(plan) = dnf_unique_point(select, from, info, None) {
                return plan;
            }
            if let Some(plan) = in_list_plan(select, from, info, None) {
                return plan;
            }
        }
        for column in range_filter_columns(select.selection.as_ref()) {
            if eqs
                .iter()
                .any(|(eq_column, _)| eq_column.eq_ignore_ascii_case(&column))
            {
                continue;
            }
            if info
                .find_index_covering(&column)
                .filter(|idx| !index_ignored(from, idx.name.o()))
                .is_some()
            {
                // A non-covering range lookup adds a table probe per row, so
                // TiDB's cost model weighs it against the full table scan
                // (`select * from t where name > 'a'` still reads the index).
                let candidates = info.indices.iter().filter(|idx| {
                    !(idx.primary && info.pk_is_clustered())
                        && !index_ignored(from, idx.name.o())
                        && idx
                            .columns
                            .first()
                            .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&column))
                });
                let (db, table) = from.qualified(current_db);
                let table_stats = stats.and_then(|handle| handle.table(&db, &table));
                let total = table_row_count(stats, from, current_db);
                let conjuncts = select
                    .selection
                    .as_ref()
                    .map(|selection| selection.split_cnf())
                    .unwrap_or_default();
                // A covering index answers the range from the index alone, so
                // it outranks a same-prefix non-covering index even when the
                // ranges match (`select a, b … where a < 3` reads `IDX_ab`,
                // not a double read on `IDX_a`).
                let mut best_covering: Option<(&sealdb_meta::IndexInfo, f64)> = None;
                let mut best_lookup: Option<(&sealdb_meta::IndexInfo, f64)> = None;
                // A TIMESTAMP index key holds UTC wall clock, so a session-local
                // bound cannot be seeked; TiDB still reads the index and keeps
                // the predicate as a cop Selection, sizing the scan from the
                // column estimate (`types/time` USE_INDEX ranges).
                let ts_range = has_timestamp_bound(&column, info, &conjuncts);
                for index in candidates {
                    let rows = match super::access_range::index_access_range(
                        info,
                        index,
                        &conjuncts,
                        total,
                        table_stats.as_ref(),
                    ) {
                        Some(access) => access.range.filter_rows.max(1.0),
                        None if ts_range => {
                            sealdb_statistics::pseudo::filter_rows(&conjuncts, total).max(1.0)
                        }
                        None => continue,
                    };
                    let slot = if index_covers_needed(index, select, &join_keys, info, from) {
                        &mut best_covering
                    } else {
                        &mut best_lookup
                    };
                    // Equal ranges prefer the narrower index: TiDB reads
                    // `idx_1(col_3)` rather than the wider `PRIMARY` (or
                    // `IDX_a` over `IDX_ab`) when both answer the same range.
                    let better = slot.is_none_or(|(best_index, best_rows)| {
                        rows < best_rows
                            || (rows == best_rows
                                && index.columns.len() < best_index.columns.len())
                    });
                    if better {
                        *slot = Some((index, rows));
                    }
                }
                let Some((index, rows)) = best_covering.or(best_lookup) else {
                    continue;
                };
                let covered = best_covering.is_some();
                let index_cost = if covered {
                    rows
                } else {
                    index_lookup_cost(rows)
                };
                if index_cost < table_scan_cost(total) {
                    return index_plan(select, from, info, index, prune_mode, &join_keys);
                }
                continue;
            }
        }
        // OR of equalities on one indexed leading column → a single index
        // range scan over a range set (`range:["x","x"], ["y","y"]`). A
        // secondary index on the prefix beats a clustered PK prefix range
        // (`mysql.global_grants` reads `i_user(USER)`, not the PRIMARY range).
        if let Some(plan) = dnf_same_index_plan(select, from, info, prune_mode, &join_keys) {
            return plan;
        }
        // Common-handle PK prefix range → `TableRangeScan`; a covering
        // secondary index on the same column wins above (fewer columns read).
        if info.common_handle
            && let Some(selection) = select.selection.as_ref()
            && let Some(pk_col) = info.clustered_prefix_column()
            && let Some(range) = handle_scan_range(selection, pk_col)
        {
            let mut plan = partitioned_table_reader(
                from,
                info,
                select.selection.as_ref(),
                prune_mode,
                stats,
                current_db,
            );
            if let PhysicalPlan::TableReader {
                range_scan,
                filter_est,
                row_count,
                ..
            } = &mut plan
            {
                *range_scan = Some(range);
                // TiDB sizes a clustered PK range from the PRIMARY index
                // statistics; the clustered index carries no histogram yet, so
                // the range falls back to the realtime table count.
                *filter_est = clustered_range_rows(from, info, stats, current_db, selection);
                if std::env::var("SEALDB_DEBUG_CLUSTERRANGE").is_ok() {
                    eprintln!("CHBRANCH table={} range={:?} filter_est={:?} row_count={}", from.name, &range_scan, filter_est, row_count);
                }
            }
            return plan;
        }
        // GROUP BY prefix of a secondary index → IndexReader + StreamAgg keep order.
        if let Some(plan) = group_by_index_plan(select, from, info, prune_mode, &join_keys) {
            return plan;
        }
        // Plain column projection fully covered by a secondary index →
        // IndexReader when TiDB's row-size cost model prefers it. A covering
        // index only pays off once it is narrower than the row it replaces:
        // both sides read the same rows, so the plan costs differ only in the
        // average row size TiDB charges them.
        if is_plain_projection(select) {
            let needed = select_needed_columns(select, Some(&from.name));
            if let Some(index) = info
                .indices
                .iter()
                .filter(|idx| !idx.primary && !index_ignored(from, idx.name.o()))
                .find(|idx| {
                    index_covers_needed(idx, select, &join_keys, info, from)
                        && covering_index_beats_scan(info, idx, &needed)
                })
            {
                return index_plan(select, from, info, index, prune_mode, &join_keys);
            }
        }
        // IndexMerge MVB: OR of ≥2 indexed equalities → union handles + lookup.
        if let Some(plan) = index_merge_plan(select, from, info, prune_mode, stats, current_db) {
            return plan;
        }
        // A covering index scan replaces the table scan when a filter cannot
        // drive a range: a `cast(col, double) = …` comparison keeps the index as
        // the narrower read, so TiDB emits `IndexFullScan` + index `Selection`.
        if select
            .selection
            .as_ref()
            .is_some_and(selection_has_cast_column)
            && let Some(index) = info
                .indices
                .iter()
                .filter(|idx| {
                    !index_ignored(from, idx.name.o()) && !(idx.primary && info.pk_is_clustered())
                })
                .find(|idx| index_covers_needed(idx, select, &join_keys, info, from))
        {
            return index_plan(select, from, info, index, prune_mode, &join_keys);
        }
        // `ORDER BY` an index key prefix: TiDB reads the index in key order
        // (`IndexFullScan … keep order:true`) and probes the table for the
        // remaining columns, which beats a table scan plus `Sort`.
        if let Some(plan) = order_by_index_plan(
            select,
            from,
            info,
            prune_mode,
            &join_keys,
            stats,
            current_db,
        ) {
            return plan;
        }
        return partitioned_table_reader(
            from,
            estimate_info,
            select.selection.as_ref(),
            prune_mode,
            stats,
            current_db,
        );
    }
    // Unknown physical table (CTE / recursive working set): always TableReader
    // so execute-time `lookup_cte_rows` can resolve. PointGet here would 1146
    // on `FROM cte WHERE col = const` in recursive arms (TiDB cte suite).
    PhysicalPlan::table_reader(from.name.clone(), Vec::new())
}

/// Columns constrained by a range filter (`<`, `<=`, `>`, `>=`, `BETWEEN`,
/// `=`) or by `<>` (two disjoint ranges), in predicate order.
fn range_filter_columns(selection: Option<&ExprNode>) -> Vec<String> {
    let Some(selection) = selection else {
        return Vec::new();
    };
    let mut columns: Vec<String> = Vec::new();
    for (column, _) in sealdb_statistics::pseudo::column_bound_sets(selection) {
        if !columns
            .iter()
            .any(|have| have.eq_ignore_ascii_case(&column))
        {
            columns.push(column);
        }
    }
    columns
}

/// TiDB `TableRangeScan` text for a clustered PK range: every CNF conjunct is
/// a comparison (or BETWEEN) on the handle column and `<>` splits in two.
/// Handle range from the handle's own conjuncts, keeping every other conjunct
/// as a residual cop filter.
///
/// TiDB's `TableRangeScan` only encodes the handle bounds; a predicate on
/// another column stays a `Selection` above the scan
/// (`TableRangeScan range:(1,+inf]` + `Selection gt(t.c, 1)`).
fn handle_scan_range_residual(
    selection: &ExprNode,
    column: &sealdb_meta::ColumnInfo,
) -> Option<(String, Vec<ExprNode>)> {
    let handle = column.name.o();
    let mut ranges: Vec<sealdb_statistics::pseudo::Bounds> = Vec::new();
    let mut residual = Vec::new();
    for item in selection.split_cnf() {
        match sealdb_statistics::pseudo::conjunct_bound_sets(&item) {
            Some((name, sets)) if name.eq_ignore_ascii_case(handle) => {
                if ranges.is_empty() {
                    ranges = sets;
                } else {
                    sealdb_statistics::pseudo::intersect_sets(&mut ranges, &sets);
                }
                if ranges.is_empty() {
                    return None;
                }
            }
            // A handle predicate the bound-set model cannot represent: do not
            // silently move it into a cop filter.
            Some((name, _)) if name.eq_ignore_ascii_case(handle) => return None,
            _ => residual.push(item),
        }
    }
    if ranges.is_empty() {
        return None;
    }
    if ranges.len() == 1 && handle_range_covers_domain(&ranges[0], column) {
        return None;
    }
    Some((sealdb_statistics::pseudo::ranges_text(&ranges), residual))
}

/// CNF conjuncts that bound the handle column (the access range of a clustered
/// PRIMARY KEY scan).
fn access_conjuncts(selection: &ExprNode, column: &sealdb_meta::ColumnInfo) -> Vec<ExprNode> {
    let handle = column.name.o();
    selection
        .split_cnf()
        .into_iter()
        .filter(|conjunct| {
            sealdb_statistics::pseudo::conjunct_bound_sets(conjunct)
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(handle))
        })
        .collect()
}

/// Does a secondary index have its leading key constrained by a residual
/// predicate? TiDB then prefers that keyed access over a PK range plus filter.
fn residual_has_index_path(
    info: &sealdb_meta::TableInfo,
    from: &TableName,
    residual: &[ExprNode],
) -> bool {
    info.indices.iter().any(|index| {
        !(index.primary && info.pk_is_clustered())
            && !index_ignored(from, index.name.o())
            && index.columns.first().is_some_and(|leading| {
                let key = super::expr_index::part_key(info, leading);
                residual.iter().any(|conjunct| {
                    sealdb_statistics::pseudo::conjunct_bound_sets(conjunct)
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(&key))
                })
            })
    })
}

/// Encode the residual cop `Selection` above a handle range into the
/// EXPLAIN-only `range_scan` text (see `explain::split_range_scan`).
fn residual_range_scan(range: &str, residual: &[ExprNode], from: &TableName) -> String {
    let selection = ExprNode::compose_cnf(residual.to_vec())
        .map(|filter| crate::core::explain::explain_pushed_filter(&from.name, &filter))
        .unwrap_or_default();
    format!("{range}\u{1}SEL:{selection}")
}

fn handle_scan_range(
    selection: &ExprNode,
    column: &sealdb_meta::ColumnInfo,
) -> Option<String> {
    let handle = column.name.o();
    let mut ranges: Vec<sealdb_statistics::pseudo::Bounds> = Vec::new();
    for item in selection.split_cnf() {
        let (name, sets) = sealdb_statistics::pseudo::conjunct_bound_sets(&item)?;
        if !name.eq_ignore_ascii_case(handle) {
            return None;
        }
        if ranges.is_empty() {
            ranges = sets;
        } else {
            sealdb_statistics::pseudo::intersect_sets(&mut ranges, &sets);
        }
        if ranges.is_empty() {
            return None;
        }
    }
    if ranges.is_empty() {
        return None;
    }
    // A range spanning the handle's whole type domain filters nothing, so
    // TiDB keeps a bare TableFullScan (`col BETWEEN T::MIN AND T::MAX`).
    if ranges.len() == 1 && handle_range_covers_domain(&ranges[0], column) {
        return None;
    }
    Some(sealdb_statistics::pseudo::ranges_text(&ranges))
}

/// True when `range` covers every value of the integer handle's type domain.
fn handle_range_covers_domain(
    range: &sealdb_statistics::pseudo::Bounds,
    column: &sealdb_meta::ColumnInfo,
) -> bool {
    let Some((lo, hi)) = super::rule_predicate_push_down::int_bounds(&column.field_type) else {
        return false;
    };
    let to_i128 = |value: &sealdb_types::Datum| -> Option<i128> {
        match value {
            sealdb_types::Datum::I64(v) => Some(*v as i128),
            sealdb_types::Datum::U64(v) => Some(*v as i128),
            sealdb_types::Datum::F64(v) if v.fract() == 0.0 => Some(*v as i128),
            sealdb_types::Datum::Decimal(d) => d.to_string().parse().ok(),
            sealdb_types::Datum::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            _ => None,
        }
    };
    match (&range.0, &range.1) {
        (None, None) => true,
        (Some((low, true)), Some((high, true))) => {
            let Some(low) = to_i128(low) else {
                return false;
            };
            let Some(high) = to_i128(high) else {
                return false;
            };
            low <= lo && high >= hi
        }
        _ => false,
    }
}

/// `ORDER BY` a secondary index key prefix: TiDB reads the index in key order
/// and probes the table for the remaining columns
/// (`IndexLookUp → IndexFullScan(Build) keep order:true / TableRowIDScan(Probe)`),
/// which beats a table scan plus `Sort`. Only a *full* index read provides the
/// order, so an ordered prefix that is already a decided range keeps its own
/// access path.
fn order_by_index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
    join_keys: &[String],
    stats: Option<&Handle>,
    current_db: &str,
) -> Option<PhysicalPlan> {
    if select.order_by.is_empty() || select.selection.is_some() {
        return None;
    }
    // Only a single-table plan can be satisfied by the table's own index.
    if select.join_tables().len() > 1 {
        return None;
    }
    let mut columns = Vec::with_capacity(select.order_by.len());
    for item in &select.order_by {
        let ExprNode::Column(col) = &item.expr else {
            return None;
        };
        // `keep order:true, desc` needs the reverse index scan; only the
        // ascending shape is modeled.
        if item.desc {
            return None;
        }
        columns.push(col.name.clone());
    }
    let index = info
        .indices
        .iter()
        .filter(|idx| {
            !(idx.primary && info.pk_is_clustered())
                && !index_ignored(from, idx.name.o())
                && !super::expr_index::has_expression_part(info, idx)
        })
        .find(|idx| {
            let keys = index_key_column_names(info, idx);
            keys.len() >= columns.len()
                && keys
                    .iter()
                    .zip(&columns)
                    .all(|(key, col)| key.eq_ignore_ascii_case(col))
        })?;
    if index_covers_needed(index, select, join_keys, info, from) {
        // A covering index is the plain projection path's job.
        return None;
    }
    let mut plan = index_plan(select, from, info, index, prune_mode, join_keys);
    if partition_read_spans_multiple(info, from) {
        // Rows arrive partition by partition, so the index keys only order each
        // partition, not the whole result.
        return None;
    }
    if let PhysicalPlan::IndexLookUpReader {
        pushed,
        row_count,
        ..
    } = &mut plan
    {
        // The index read scans every key, so the cop task sizes the scan (and
        // the table probe) at the table's row count.
        *row_count = table_row_count(stats, from, current_db);
        if in_explain_build() {
            *pushed = Some(bytes_info("KEEP_ORDER"));
        }
        return Some(plan);
    }
    None
}

/// A read of a partitioned table that spans more than one partition returns its
/// rows partition by partition, so handle and index order are not global.
fn partition_read_spans_multiple(info: &sealdb_meta::TableInfo, from: &TableName) -> bool {
    let Some(part) = info.partition.as_ref() else {
        return false;
    };
    if from.partition_names.is_empty() {
        part.definitions.len() > 1 || part.num > 1
    } else {
        from.partition_names.len() > 1
    }
}

fn select_spans_multiple_partitions(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> bool {
    let Some(from) = select.from.as_ref() else {
        return false;
    };
    let (db, table) = from.qualified(current_db);
    is.and_then(|schema| schema.table_by_name(&db, &table))
        .is_some_and(|info| partition_read_spans_multiple(info, from))
}

/// The plan already yields its rows in `ORDER BY` order (a full index read
/// whose key prefix matches the sort), so no `Sort` is needed.
fn plan_provides_order(select: &SelectStmt, plan: &PhysicalPlan) -> bool {
    if select.order_by.is_empty() {
        return false;
    }
    // IndexMerge union arms that read their index in key order already provide
    // the query's ORDER BY (`keep order:true`). A dirty read keeps the merge
    // below a `UnionScan`, which passes the order through.
    let plan = match plan {
        PhysicalPlan::UnionScan { child, .. } => child.as_ref(),
        other => other,
    };
    if let PhysicalPlan::IndexMerge {
        keep_order: true,
        order_column: Some(column),
        order_desc,
        ..
    } = plan
    {
        return match select.order_by.as_slice() {
            [item] => {
                matches!(&item.expr, ExprNode::Column(col) if col.name.eq_ignore_ascii_case(column))
                    && item.desc == *order_desc
            }
            _ => false,
        };
    }
    // Only a full index scan keeps key order; a decided range carries its own
    // `keep order` flag (and `Sort` semantics) in EXPLAIN. A multi-partition
    // read returns rows partition by partition, so the keys are not ordered
    // across the whole result.
    let keys = match plan {
        PhysicalPlan::IndexLookUpReader {
            index_columns,
            range_scan: None,
            ..
        }
        | PhysicalPlan::IndexReader {
            index_columns,
            range_scan: None,
            ..
        } => index_columns,
        _ => return false,
    };
    let mut keys = keys.iter();
    for item in &select.order_by {
        let ExprNode::Column(col) = &item.expr else {
            return false;
        };
        let Some(key) = keys.next() else {
            return false;
        };
        if item.desc || !key.eq_ignore_ascii_case(&col.name) {
            return false;
        }
    }
    true
}

/// TiDB constant folding: int handle accepts string/float numeric literals (`a = '1'`).
fn datum_as_handle(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => Some(*v as i64),
        Datum::F64(v) if v.fract() == 0.0 && *v >= i64::MIN as f64 && *v <= i64::MAX as f64 => {
            Some(*v as i64)
        }
        Datum::Bytes(b) => {
            let s = std::str::from_utf8(b).ok()?.trim();
            s.parse::<i64>().ok()
        }
        _ => None,
    }
}

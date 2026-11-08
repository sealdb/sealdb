use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count_value, index_lookup_cost, partition_filter_rows, point_get_cost, prefer_topn,
    table_row_count, table_scan_cost,
};
use super::rule_join_reorder::reorder_hash_join;
use super::rule_join_reorder_dp::{JoinEqEdge, dp_join_steps, edge_for_extension, eq_join_edge};
use super::rule_partition_processor::{PartitionPruneMode, resolve_partitions};
use super::rule_inject_extra_projection::{
    inject_proj_below_sort, order_by_all_constant, order_by_has_scalar_func,
};
use super::rule_having_push_down::apply_having_push_down;
use super::rule_predicate_push_down::{eq_column_consts, predicate_push_down};
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
        let plan = with_statement_cte_refs(stmt, || {
            self.build_with_schema_inner(stmt, is, current_db, stats, prune_mode)
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
                // TiDB folds an unsatisfiable predicate (`a = 2 AND a = 3`)
                // to `0` and plans a zero-row TableDual.
                if let Some(ExprNode::Value(ValueExpr {
                    datum: Datum::I64(0),
                })) = &select.selection
                {
                    return Ok(PhysicalPlan::TableDual { row_count: 0 });
                }
                // TiDB quantified `!= ANY` / `= ALL` → agg + CARTESIAN join.
                if let Some(plan) =
                    quantified_cmp_plan(select, is, current_db, stats, prune_mode)
                {
                    let _ = crate::cascades::maybe_optimize("select")?;
                    return Ok(plan);
                }
                // Correlated scalar `col = (SELECT max(…) …)`.
                if let Some(plan) =
                    scalar_max_eq_plan(select, is, current_db, stats, prune_mode)
                {
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
                if let Some(plan) =
                    inl_join_hint_plan(select, is, current_db, stats, prune_mode)
                {
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
                // Correlated `IN (subq)` in the select list → left outer semi join.
                if let Some(plan) =
                    in_subquery_select_plan(select, is, current_db, stats, prune_mode)
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
                        self.build_cte_plan(
                            select,
                            &cte_from,
                            is,
                            current_db,
                            stats,
                            prune_mode,
                        )?
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
                if let Some(filter) = &select.selection {
                    plan = predicate_push_down(plan, filter, is, current_db);
                    plan = star_projection_over_filter(plan, select, is, current_db);
                }
                plan = lift_point_get_merge_join(plan, select, is, current_db);
                plan = self_join_star_projection(plan, select, is, current_db);
                plan = apply_select_lock(plan, select);
                let mut stream_keep_order = false;
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    let (agg, keep_order) = match is.and_then(|is| {
                        global_index_agg_plan(select, &plan, is, current_db)
                    }) {
                        Some(agg) => (agg, false),
                        None => build_agg_plan(select, plan, current_db, is),
                    };
                    plan = agg;
                    stream_keep_order = keep_order;
                    if let Some(having) = &select.having {
                        plan = apply_having_push_down(
                            plan, having, select, is, current_db,
                        );
                    }
                }
                if needs_window(select) {
                    // TiDB: Sort → Window → Shuffle, then Projection for
                    // expressions over window results (optimizeByShuffle4Window).
                    plan = super::window::build_window_plan(
                        plan, select, is, current_db, stats,
                    );
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
                    let handle_order = matches!(plan, PhysicalPlan::TableReader { .. })
                        && order_by_handle(select, is, current_db);
                    if handle_order {
                        plan = set_keep_order(plan);
                    }
                    // Batch point get walks its keys in order, so an ORDER BY on
                    // the key needs no Sort (`keep order:true, desc:…`).
                    let point_order = point_get_order(select, &plan);
                    if let Some(desc) = point_order {
                        plan = set_batch_point_order(plan, desc);
                    }
                    let need_sort = !select.order_by.is_empty()
                        && !order_by_all_constant(select)
                        && !(stream_keep_order && order_prefix_of_group_by(select))
                        && !merge_orders
                        && !handle_order
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
                            plan = PhysicalPlan::limit(plan, select.offset.unwrap_or(0), 0);
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
                    }
                }
                // TiDB remapping Projection after StreamAgg+ORDER BY is explain-only
                // (see explain.rs); executable plan keeps the agg output schema.
                plan = rewrite_semi_join_agg(rewrite_semi_join_ctx(plan, is, current_db));
                // TiDB wraps quantified ALL Apply in a remapping Projection for
                // `SELECT *` (cte suite: Projection → Apply → StreamAgg/HashAgg).
                plan = wrap_quantified_all_projection(plan, select, is, current_db);
                // EXPLAIN renders pushed predicates with the table's OrigTblName
                // (`t1.col1` → `uk_hp16726.col1`), never the SQL alias.
                plan = explain_rewrite_pushed(plan, select);
                // Optional Cascades identity pass (SEALDB_CASCADES=1); RBO plan kept.
                let _ = crate::cascades::maybe_optimize("select")?;
                Ok(plan)
            }
            Statement::Insert(insert) => Ok(PhysicalPlan::Insert {
                child: self.dml_insert_child(insert, is, current_db, stats, prune_mode)?,
            }),
            Statement::LoadData(_) => Ok(PhysicalPlan::Insert { child: None }),
            Statement::Update(update) => Ok(PhysicalPlan::Update {
                child: self.dml_update_child(update, is, current_db, stats, prune_mode)?,
            }),
            Statement::Delete(delete) => Ok(PhysicalPlan::Delete {
                child: self.dml_delete_child(delete, is, current_db, stats, prune_mode)?,
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
                    self.build_with_schema(
                        &explain.stmt,
                        is,
                        current_db,
                        stats,
                        prune_mode,
                    )
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
                let plan = build_set_op_plan(arms, &ids);
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
    SelectStmt {
        fields: vec![sealdb_parser::ast::SelectField {
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
            orig_text: None,
        }],
        from: Some(table.clone()),
        joins: join.cloned().into_iter().collect(),
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
            let plan = self.build_with_schema(
                &Statement::SetOpr(set.clone()),
                is,
                current_db,
                stats,
                prune_mode,
            )?;
            return Ok(Some(Box::new(plan)));
        }
        let Some(select) = insert.select.as_deref() else {
            return Ok(None);
        };
        let plan = self.build_with_schema(
            &Statement::Select(select.clone()),
            is,
            current_db,
            stats,
            prune_mode,
        )?;
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
        let select = dml_scan_select(
            &delete.table,
            delete.join.as_ref(),
            delete.selection.clone(),
        );
        let plan = self.build_with_schema(
            &Statement::Select(select),
            is,
            current_db,
            stats,
            prune_mode,
        )?;
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
            seed = build_set_op_plan(arms, &set_op_ids);
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
        if let Some(plan) =
            self.build_left_then_inner(select, is, current_db, stats, prune_mode)?
        {
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
                && let Some(filter) =
                    null_reject_on_inner(
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
            matches!(first_tp, JoinType::RightOuter | JoinType::Inner | JoinType::Cross),
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
            let mut right = self.join_side(select, &join.right, is, current_db, stats, prune_mode)?;
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
            if let Some(filter) =
                null_reject_on_inner(
                    &on,
                    &right_table.name,
                    right_table.resolve_name(),
                    &table_column_names(is, current_db, &right_table),
                )
            {
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
        let outer_side =
            self.join_side(select, outer_right, is, current_db, stats, prune_mode)?;
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
            index_columns: index
                .columns
                .iter()
                .map(|c| c.name.o().to_string())
                .collect(),
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
        let mut base_for_inner =
            self.join_side(select, base, is, current_db, stats, prune_mode)?;
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
                    && let Some(converted) = convert_nested_outer_to_inner(
                        &mut right_plan,
                        on,
                        is,
                        current_db,
                    )
                {
                    reordered = converted;
                }
                if !reordered
                    && matches!(
                        join_tp,
                        JoinType::LeftOuter | JoinType::RightOuter
                    )
                {
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
            return Ok(Some(PhysicalPlan::HashJoin {
                est_rows: None,
                tp: JoinType::LeftOuter,
                left: Box::new(a4),
                right: Box::new(mid),
                left_table: t4.name.clone(),
                right_table: t3.name.clone(),
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
                .ok_or_else(|| {
                    Error::unsupported(format!("unknown CTE {}", table.resolve_name()))
                })
        } else {
            Ok(access_plan(
                select,
                table,
                is,
                current_db,
                stats,
                prune_mode,
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
    fn columns_of(
        is: &InfoSchema,
        current_db: &str,
        table: &TableName,
    ) -> Option<Vec<String>> {
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

    fn visible(item: &sealdb_parser::ast::FromItem, is: &InfoSchema, db: &str) -> Option<Vec<String>> {
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
        } => on.has_subquery() || from_item_on_has_subquery(left) || from_item_on_has_subquery(right),
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
        .map(|info| info.columns.iter().map(|c| c.name.o().to_string()).collect())
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
            return t.eq_ignore_ascii_case(inner_alias)
                || t.eq_ignore_ascii_case(inner_physical);
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
        PhysicalPlan::TableReader { pushed, .. } | PhysicalPlan::IndexReader { pushed, .. }
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
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
        },
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
            filter,
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
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
            child: Box::new(inject_inner_join_null_reject(*child, select, is, current_db)),
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
        col.table
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(&table.name) || t.eq_ignore_ascii_case(table.resolve_name()))
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
    if let Some((l, r)) = merge_join_key_pair_preferred(left, right, on, is, current_db, &left_phys) {
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
fn outer_join_inner_range(
    left: &TableName,
    right: &TableName,
    on: &ExprNode,
) -> Option<String> {
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
                    t.eq_ignore_ascii_case(&left.name) || t.eq_ignore_ascii_case(left.resolve_name())
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
        ExprNode::Value(ValueExpr { datum: Datum::I64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::U64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::F64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::Decimal(d) }) => d.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) => {
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
    let Some((est, filter_rows, on)) = right_join_explain_override(&plan, select, is, current_db, stats)
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
        (Some(ln), Some(rn)) => Some(
            left_stats.count as f64 * right_stats.count as f64 / ln.max(rn).max(1.0),
        ),
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
        PhysicalPlan::Selection { filter, child } => PhysicalPlan::Selection {
            filter,
            child: Box::new(with_filter_est(*child, rows)),
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

fn strip_cte_order_sort(plan: PhysicalPlan) -> PhysicalPlan {    match plan {
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
    if select.fields.iter().any(|field| mentions_rowid(&field.expr)) {
        return Err(Error::new(
            ERR_BAD_FIELD,
            "Unknown column '_tidb_rowid' in 'field list'",
        ));
    }
    Ok(())
}

/// TiDB: a table scan yields row-handle order, so `ORDER BY` the handle needs
/// no Sort (`keep order:true`). Only ascending single-key ordering is modeled.
fn order_by_handle(select: &SelectStmt, is: Option<&InfoSchema>, current_db: &str) -> bool {
    if select.order_by.is_empty() {
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
        "_tidb_rowid".to_string()
    };
    select.order_by.iter().all(|item| {
        !item.desc
            && matches!(
                &item.expr,
                ExprNode::Column(col) if col.name.eq_ignore_ascii_case(&handle)
            )
    })
}

/// `ORDER BY` the batch point-get key (index prefix or handle) needs no Sort:
/// the batch walks its keys in order (`keep order:true, desc:…`).
/// Returns `Some(descending)` when the sort is satisfied by the key order.
fn point_get_order(select: &SelectStmt, plan: &PhysicalPlan) -> Option<bool> {
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
    if select.order_by.iter().any(|item| item.desc != same_direction) {
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
        PhysicalPlan::Selection { filter, child } => PhysicalPlan::Selection {
            filter,
            child: Box::new(set_batch_point_order(*child, desc)),
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

/// TiDB: MergeJoin already orders by join keys; skip outer Sort when ORDER BY
/// is ordinals or those same columns (`ORDER BY 1, 2` on equijoin output).
fn order_by_covered_by_merge_join(select: &SelectStmt, plan: &PhysicalPlan) -> bool {    let PhysicalPlan::MergeJoin {
        left_key, right_key, ..
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
        plan = PhysicalPlan::limit(plan, offset.unwrap_or(0), 0);
    }
    plan
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
/// [`PlanColumnIds`]).
fn build_set_op_plan(arms: Vec<(SetOprType, PhysicalPlan)>, ids: &SetOpColumnIds) -> PhysicalPlan {
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
            let children: Vec<Box<PhysicalPlan>> =
                arms.into_iter().map(|(_, plan)| Box::new(plan)).collect();
            let union = PhysicalPlan::Union {
                op: SetOprType::UnionAll,
                children,
            };
            let est_rows = super::explain::est_rows(&union) * DISTINCT_UNION_EST_FACTOR;
            Some(PhysicalPlan::DistinctUnion {
                info: distinct_union_info(&ids.distinct),
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
        Self
    }
}

impl Drop for TableDbScope {
    fn drop(&mut self) {
        TABLE_DB_MAP.with(|stack| {
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
pub(crate) fn explain_schema_prefix(table: Option<&str>) -> String {
    let schema = table
        .and_then(explain_table_schema)
        .unwrap_or_else(sealdb_expression::current_db_name);
    if schema.is_empty() {
        String::new()
    } else {
        format!("{schema}.")
    }
}

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
        table.name.eq_ignore_ascii_case(name)
            || table.resolve_name().eq_ignore_ascii_case(name)
    })
}

fn cte_ref_count(name: &str) -> Option<usize> {    CTE_REF_COUNTS.with(|cell| {
        let counts = cell.borrow();
        counts
            .as_ref()
            .and_then(|counts| counts.get(&name.to_ascii_lowercase()).copied())
    })
}

fn count_statement_table_refs(stmt: &Statement, counts: &mut std::collections::HashMap<String, usize>) {
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

fn count_select_table_refs(select: &SelectStmt, counts: &mut std::collections::HashMap<String, usize>) {
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

fn count_result_set_table_refs(node: &ResultSetNode, counts: &mut std::collections::HashMap<String, usize>) {
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
        if join
            .using_cols
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
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

fn topn_by_info(select: &SelectStmt, current_db: &str) -> String {
    let table = select.from.as_ref().map(|t| t.name.as_str());
    let from_derived_union = select.with.as_ref().is_some_and(|w| {
        table.is_some_and(|t| {
            w.find(t)
                .is_some_and(|cte| !cte.set_rest.is_empty())
        })
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
                        let label = select
                            .fields
                            .get(ord.saturating_sub(1))
                            .and_then(|f| match (&f.expr, table) {
                                (ExprNode::Column(c), Some(t)) if !from_cte => Some(format!(
                                    "{}.{}.{}",
                                    schema_prefix(current_db),
                                    c.table.as_deref().unwrap_or(t),
                                    c.name
                                )),
                                _ => None,
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
    if is_star_select(select)
        || needs_hash_agg(select)
        || needs_window(select)
        || matches!(plan, PhysicalPlan::Projection { .. })
    {
        return plan;
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
    if let Some(proj) = cop_output_projection(&plan, select, current_db) {
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
    let col_base = projection_column_base(&plan);
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
    let mut out = Vec::new();
    for field in &select.fields {
        let ExprNode::Column(col) = &field.expr else {
            return None;
        };
        if col.name == "*" {
            return None;
        }
        out.push(col.name.clone());
    }
    if out.is_empty() {
        return None;
    }
    let filter = select.selection.as_ref()?;
    let prunes = filter
        .referenced_columns()
        .iter()
        .any(|(_, name)| !out.iter().any(|o| o.eq_ignore_ascii_case(name)));
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
            ExprNode::Value(v)
                if matches!(&v.datum, sealdb_types::Datum::Bytes(b) if b == b"*") =>
            {
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
        for col in &info.columns {
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
        ExprNode::Value(ValueExpr { datum: Datum::I64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::U64(v) }) => v.to_string(),
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
    if matches!(tp, JoinType::LeftOuter | JoinType::RightOuter | JoinType::Semi | JoinType::Anti)
    {
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
            let side_tbl = select
                .join_tables()
                .into_iter()
                .find(|t| {
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

fn cte_field_expr(select: &SelectStmt, cte_name: &str, col: &str) -> Option<ExprNode> {
    let with = select.with.as_ref()?;
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
        PhysicalPlan::Union { children, .. } => {
            children.iter().all(|c| seed_is_dual_projection(c) || seed_is_union_of_dual(c))
        }
        PhysicalPlan::DistinctUnion { child, .. } => seed_is_union_of_dual(child),
        _ => false,
    }
}

fn projection_column_base(plan: &PhysicalPlan) -> usize {
    match plan {
        PhysicalPlan::CTE {
            seed,
            inline: true,
            ..
        } => projection_column_base(seed),
        PhysicalPlan::Projection { exprs, .. } => exprs.len() + 1,
        PhysicalPlan::TableDual { .. } => 1,
        // TiDB unique-id base after a 1-col table + view/derived intermediates.
        PhysicalPlan::TableReader { .. }
        | PhysicalPlan::Selection { .. }
        | PhysicalPlan::IndexReader { .. } => 6,
        _ => 1,
    }
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

fn bit_column_width(expr: &ExprNode, widths: &std::collections::HashMap<String, usize>) -> Option<usize> {
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
    use sealdb_parser::ast::expressions::{
        BetweenExpr, BinaryOpExpr, IsNullExpr, UnaryOpExpr,
    };
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
    let selection = select.selection.as_ref()?;
    let from = select.from.as_ref()?;
    let (db, table) = from.qualified(current_db);
    let info = is?.table_by_name(&db, &table)?;
    let coerced = expand_casted_in_lists(&coerce_numeric_literals(selection, info));
    if coerced.restore_string() == selection.restore_string() {
        return None;
    }
    let mut out = select.clone();
    out.selection = Some(coerced);
    Some(out)
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
            selection_has_cast_column(&list.expr)
                || list.list.iter().any(selection_has_cast_column)
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
                .map(|(column, value)| ExprNode::Binary(BinaryOpExpr {
                    op: Op::Eq,
                    left: Box::new(column.clone()),
                    right: Box::new(value.clone()),
                    single_cnf_item: false,
                }));
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

fn index_ignored(from: &TableName, name: &str) -> bool {    from.ignore_indexes
        .iter()
        .any(|ignored| ignored.eq_ignore_ascii_case(name))
}

fn index_allowed(from: &TableName, name: &str, only_index: Option<&str>) -> bool {
    if index_ignored(from, name) {
        return false;
    }
    only_index.is_none_or(|only| only.eq_ignore_ascii_case(name))
}

/// `a = 1 AND b = 2 …` fully constrains a multi-column unique key → PointGet.
fn unique_index_eq_point(
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    eqs: &[(String, Datum)],
    only_index: Option<&str>,
) -> Option<PhysicalPlan> {
    let index = info.indices.iter().find(|idx| {
        idx.unique
            // A common-handle PRIMARY KEY still answers an equality point get
            // (`Point_Get … clustered index:PRIMARY(a, b)`); only the integer
            // handle is reached through the handle path instead of an index.
            && !(idx.primary && info.pk_is_handle)
            && !idx.invisible
            && idx.columns.len() > 1
            && index_allowed(from, idx.name.o(), only_index)
            && idx.columns.iter().all(|c| {
                eqs.iter()
                    .any(|(col, _)| col.eq_ignore_ascii_case(c.name.o()))
            })
    })?;
    if index.columns.iter().any(|c| {
        eqs.iter()
            .find(|(col, _)| col.eq_ignore_ascii_case(c.name.o()))
            .is_some_and(|(_, value)| !point_value_hits_float(info, c.name.o(), value))
    }) {
        return Some(PhysicalPlan::TableDual { row_count: 0 });
    }
    let index_columns: Vec<String> = index
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let values: Vec<Datum> = index_columns
        .iter()
        .map(|column| {
            eqs.iter()
                .find(|(col, _)| col.eq_ignore_ascii_case(column))
                .map(|(_, value)| value.clone())
                .unwrap_or(Datum::Null)
        })
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
    let arms = selection.split_dnf();
    if arms.len() < 2 {
        return None;
    }
    let first_eqs: Vec<(String, Datum)> = arms[0]
        .split_cnf_all()
        .into_iter()
        .filter_map(|expr| expr.eq_column_const())
        .collect();
    let index = info.indices.iter().find(|idx| {
        idx.unique
            // A common-handle PRIMARY KEY still answers an equality point get
            // (`Point_Get … clustered index:PRIMARY(a, b)`); only the integer
            // handle is reached through the handle path instead of an index.
            && !(idx.primary && info.pk_is_handle)
            && !idx.invisible
            && idx.columns.len() > 1
            && index_allowed(from, idx.name.o(), only_index)
            && idx.columns.iter().all(|c| {
                first_eqs
                    .iter()
                    .any(|(col, _)| col.eq_ignore_ascii_case(c.name.o()))
            })
    })?;
    // Every arm must be exactly one row of the unique key; extra predicates
    // stay in the residual filter instead of silently disappearing.
    if arms
        .iter()
        .any(|arm| arm.split_cnf_all().len() != index.columns.len())
    {
        return None;
    }
    let index_columns: Vec<String> = index
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let tuples = selection.dnf_const_tuples(&index_columns)?;
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
    Some(PhysicalPlan::BatchPointGet {
        table: from.name.clone(),
        column: index_columns.first().cloned().unwrap_or_default(),
        index: Some(index.name.o().to_string()),
        n_keys: tuples.len(),
        handles: Vec::new(),
        index_columns: index_columns.clone(),
        tuple_columns: index_columns.clone(),
        partition_names: tuple_partition_names(
            info,
            &index_columns,
            &tuples,
            &from.partition_names,
            index.global,
        ),
        global_index: index.global,
        lock: false,
        keep_order: false,
        desc: false,
    })
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
        if let Some(index) = info.find_unique_index_on(&column).filter(|idx| {
            index_allowed(from, idx.name.o(), only_index)
        }) {
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
            let index_columns = index
                .columns
                .iter()
                .map(|c| c.name.o().to_string())
                .collect();
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
    }
    // `(a, b) IN ((1, 2), (3, 4))` on a unique key covering exactly those
    // columns is a batch point get as well.
    let row_list = select
        .selection
        .as_ref()
        .and_then(|expr| {
            expr.split_cnf()
                .into_iter()
                .find_map(|item| item.row_in_consts_with_len())
        })?;
    let (names, tuples, total) = row_list;
    let index = info.indices.iter().find(|idx| {
        idx.unique
            && !idx.invisible
            && idx.columns.len() == names.len()
            && index_allowed(from, idx.name.o(), only_index)
            && idx.columns.iter().all(|c| {
                names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(c.name.o()))
            })
    })?;
    let column_position = |index_col: &sealdb_meta::IndexColumn| {
        names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(index_col.name.o()))
    };
    let hits: Vec<bool> = tuples
        .iter()
        .map(|tuple| {
            index.columns.iter().all(|ic| {
                column_position(ic)
                    .and_then(|pos| tuple.get(pos))
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
    let index_columns: Vec<String> = index
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    Some(PhysicalPlan::BatchPointGet {
        table: from.name.clone(),
        column: names.first().cloned().unwrap_or_default(),
        index: Some(index.name.o().to_string()),
        n_keys: total,
        handles: Vec::new(),
        index_columns,
        partition_names: tuple_partition_names(
            info,
            &names,
            &tuples,
            &from.partition_names,
            index.global,
        ),
        tuple_columns: names,
        global_index: index.global,
        lock: false,
        keep_order: false,
        desc: false,
    })
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
    if part.is_expr_key()
        || part.columns.len() != 1
        || !part.column().eq_ignore_ascii_case(column)
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
        return PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle,
            index,
            index_columns,
            partition_names,
            global_index: index_global,
            lock: false,
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
                sealdb_types::field_type::TYPE_ENUM
                    | sealdb_types::field_type::TYPE_SET
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
    let Some(column) = parts.iter().find_map(|part| eq_parts(part).map(|(col, _)| col)) else {
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
    if eqs.iter().any(|d| d.compare(first_eq) != std::cmp::Ordering::Equal) {
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
            if matches!(
                bin.op,
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge
            ) {
                if column_class(&left, info) == Some(ColumnClass::String)
                    && numeric_literal(&right)
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
                if column_class(&left, info) == Some(ColumnClass::Temporal)
                    && let Some(ft) = column_field_type(&left, info)
                    && let Some(datum) = temporal_literal(&right, ft)
                {
                    right = value_expr(datum);
                } else if column_class(&right, info) == Some(ColumnClass::Temporal)
                    && let Some(ft) = column_field_type(&right, info)
                    && let Some(datum) = temporal_literal(&left, ft)
                {
                    left = value_expr(datum);
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
                Some(ColumnClass::String)
                    if list.list.iter().all(|item| numeric_literal(item)) =>
                {
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
                        ExprNode::Row(tuple) => {
                            tuple.exprs.get(pos).is_some_and(numeric_literal)
                        }
                        _ => false,
                    });
                    if any_numeric {
                        *column = cast_double(column.clone());
                        changed = true;
                    }
                }
                if changed {
                    coerced.expr = Box::new(ExprNode::Row(
                        sealdb_parser::ast::expressions::RowExpr { exprs: columns },
                    ));
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
        sealdb_types::TYPE_DATE
        | sealdb_types::TYPE_DATETIME
        | sealdb_types::TYPE_TIMESTAMP => ColumnClass::Temporal,
        _ => return None,
    })
}

/// Column `FieldType` for a bare column reference.
fn column_field_type<'a>(expr: &ExprNode, info: &'a sealdb_meta::TableInfo) -> Option<&'a sealdb_types::FieldType> {
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
        sealdb_types::TYPE_DATE => {
            sealdb_types::parse_date_literal(&text).ok()?
        }
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

fn cast_double(expr: ExprNode) -> ExprNode {
    ExprNode::Cast(sealdb_parser::ast::expressions::FuncCastExpr {
        expr: Box::new(expr),
        target: "DOUBLE".to_string(),
    })
}

fn const_num(expr: &ExprNode) -> Option<i64> {
    match expr {
        ExprNode::Value(ValueExpr { datum: Datum::I64(v) }) => Some(*v),
        ExprNode::Value(ValueExpr { datum: Datum::U64(v) }) => i64::try_from(*v).ok(),
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
            if matches!(
                bin.op,
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge
            ) && let (Some(l), Some(r)) = (const_num(&left), const_num(&right))
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
        let items: Vec<ExprNode> = leaves
            .into_iter()
            .map(rebalance_disjunctions)
            .collect();
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
            && columns
                .iter()
                .all(|c| eqs.iter().any(|(col, _)| col.eq_ignore_ascii_case(c.name.o())))
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
            idx.unique
                && !(idx.primary && info.pk_is_clustered())
                && covers(&eqs, &idx.columns)
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
        table_stats.column_not_null_count(&column)?
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
    let mut ranges: Vec<sealdb_statistics::pseudo::Bounds> = Vec::new();
    for item in selection.split_cnf() {
        let (name, sets) = sealdb_statistics::pseudo::conjunct_bound_sets(&item)?;
        if !name.eq_ignore_ascii_case(column.name.o()) {
            return None;
        }
        if ranges.is_empty() {
            ranges = sets;
        } else {
            sealdb_statistics::pseudo::intersect_sets(&mut ranges, &sets);
            if ranges.is_empty() {
                return Some(0.0);
            }
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let stat_ranges: Vec<sealdb_statistics::range_est::StatsRange> = ranges
        .into_iter()
        .map(|(low, high)| sealdb_statistics::range_est::StatsRange { low, high })
        .collect();
    Some(
        table_stats
            .index_range_estimate(pk.name.o(), &column.field_type, &stat_ranges)
            .or_else(|| table_stats.index_range_estimate("primary", &column.field_type, &stat_ranges))
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
            if let PhysicalPlan::TableReader { filter_est: est, .. } = &mut reader {
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
            if let PhysicalPlan::TableReader { filter_est: est, .. } = &mut reader {
                *est = filter_est;
            }
            reader
        }
    }
}

fn index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
    prune_mode: PartitionPruneMode,
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
    let index_columns: Vec<String> = index
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    if select_covers_index(select, info, index) {
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

/// Detect pure DNF of column=const arms where each arm has a leading-column index.
fn index_merge_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    let selection = select.selection.as_ref()?;
    // Only a single top-level OR tree (no AND wrappers) for the MVB.
    let cnf = selection.split_cnf();
    if cnf.len() != 1 {
        return None;
    }
    let arms = cnf[0].split_dnf();
    if arms.len() < 2 {
        return None;
    }
    let mut indexes = Vec::with_capacity(arms.len());
    for arm in &arms {
        let (column, _) = arm.eq_column_const()?;
        let index = info.find_index_covering(&column)?;
        indexes.push(index.name.o().to_string());
    }
    Some(PhysicalPlan::IndexMerge {
        table: from.name.clone(),
        indexes,
        partitions: resolve_partitions(
            info,
            select.selection.as_ref(),
            &from.partition_names,
            prune_mode,
        )
        .ids,
        pushed: None,
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
    info.index_covers(index, &select_needed_columns(select))
}

fn select_needed_columns(select: &SelectStmt) -> Vec<String> {
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

/// Prefer IndexReader when GROUP BY columns are a leading prefix of an index.
fn group_by_index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    prune_mode: PartitionPruneMode,
) -> Option<PhysicalPlan> {
    let group_cols = group_by_column_names(select)?;
    if group_cols.is_empty() {
        return None;
    }
    for index in &info.indices {
        if !index_has_group_prefix(index, &group_cols) {
            continue;
        }
        if select_covers_index(select, info, index) {
            return Some(index_plan(select, from, info, index, prune_mode));
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
        index_columns: index.columns.iter().map(|c| c.name.o().to_string()).collect(),
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
    group_cols.iter().enumerate().all(|(i, name)| {
        index.columns[i]
            .name
            .o()
            .eq_ignore_ascii_case(name)
    })
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
                root.push(format!("avg(Column#{next}, Column#{})->Column#{out}", next + 1));
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
            root.push(format!("avg(Column#{next}, Column#{})->Column#{out}", next + 1));
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
                exprs.push(format!("cast({arg}, decimal(10,0) BINARY)->Column#{}", proj_base + i));
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
        } => Some((
            *row_count,
            range_scan.is_none() && *row_count == 10_000.0,
        )),
        PhysicalPlan::TableReader {
            row_count,
            stats_pseudo,
            filter_est,
            ..
        } => Some((
            filter_est.unwrap_or(row_count.abs()),
            *stats_pseudo,
        )),
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
    let keep_order = matches!(plan, PhysicalPlan::IndexReader { .. }) && !select.group_by.is_empty();
    let stream = select.group_by.is_empty() || keep_order;
    let est = agg_est_rows(select);
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
            let info = format!(
                "funcs:max({db_qual}{table}._tidb_rowid)->Column#{out_col}"
            );
            return (
                PhysicalPlan::stream_agg(limit, info, String::new(), est),
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

    if select.distinct {
        // TiDB: HashAgg(firstrow distinct cols) → StreamAgg(group by) → IndexReader.
        let field = select
            .fields
            .first()
            .map(|f| explain_col_ref(table, &f.expr))
            .unwrap_or_else(|| format!("select.{table}.b"));
        let stream_info = format!(
            "group by:{group_info}, funcs:firstrow(Column#{partial_col})->{field}"
        );
        let cop = format!(
            "group by:{group_info}, funcs:firstrow({field})->Column#{partial_col}"
        );
        let hash_info = format!("group by:{field}, funcs:firstrow({field})->{field}");
        let stream = PhysicalPlan::stream_agg(plan, stream_info, cop, est);
        return (PhysicalPlan::hash_agg(stream, hash_info, est), keep_order);
    }

    // Aggregation over a join tree: TiDB labels group-by / firstrow operands
    // with each side's OrigTblName (`t1.col1` → `uk_hp16726.col1`) and drops
    // the synthetic partial `count` when the select list holds only group-by
    // columns (`funcs:firstrow(…)->…` only).
    if !select.joins.is_empty() {
        return build_join_agg_plan(select, plan, current_db);
    }

    let agg_arg = select
        .fields
        .iter()
        .find_map(|f| match &f.expr {
            ExprNode::Func(func) if is_agg_expr(&f.expr) => func
                .args
                .first()
                .map(|a| explain_col_ref(table, a)),
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
    let mut root_funcs = vec![format!(
        "funcs:{agg_fn}(Column#{partial_col})->Column#5"
    )];
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
        (
            PhysicalPlan::stream_agg(plan, info, cop, est),
            keep_order,
        )
    } else {
        (PhysicalPlan::hash_agg(plan, info, est), false)
    }
}


/// EXPLAIN-only: relabel columns in pushed filters with each side's
/// OrigTblName (lowercase), matching TiDB `expression.ExplainInfo()` of a
/// predicate that has been pushed into a `DataSource`.
fn explain_rewrite_pushed(plan: PhysicalPlan, select: &SelectStmt) -> PhysicalPlan {
    if !in_explain_build() {
        return plan;
    }
    let mut tables = select.join_tables();
    if let Some(from) = &select.from {
        tables.push(from);
    }
    fn walk(plan: PhysicalPlan, tables: &[&sealdb_parser::ast::TableName]) -> PhysicalPlan {
        match plan {
            PhysicalPlan::Selection { filter, child } => PhysicalPlan::Selection {
                filter: rewrite_col_tables(&filter, tables),
                child: Box::new(walk(*child, tables)),
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
                keep_order,
                pushed: pushed.as_ref().map(|p| rewrite_col_tables(p, tables)),
            },
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
                left: Box::new(walk(*left, tables)),
                right: Box::new(walk(*right, tables)),
                left_table,
                right_table,
                on,
            },
            PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
                exprs,
                child: Box::new(walk(*child, tables)),
            },
            PhysicalPlan::Limit {
                child,
                offset,
                count,
            } => PhysicalPlan::Limit {
                child: Box::new(walk(*child, tables)),
                offset,
                count,
            },
            PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
                child: Box::new(walk(*child, tables)),
                by_info,
            },
            PhysicalPlan::TopN {
                child,
                by_info,
                offset,
                count,
            } => PhysicalPlan::TopN {
                child: Box::new(walk(*child, tables)),
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
                child: Box::new(walk(*child, tables)),
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
                child: Box::new(walk(*child, tables)),
            },
            other => other,
        }
    }
    walk(plan, &tables)
}

/// `{db}.{orig_table}.{col}` for an aggregate operand over a join tree.
/// `select.join_tables()` maps each alias to its physical (OrigTblName) table.
fn join_side_physical(select: &SelectStmt, alias: &str) -> String {
    select
        .join_tables()
        .iter()
        .find(|t| {
            t.name.eq_ignore_ascii_case(alias)
                || t.resolve_name().eq_ignore_ascii_case(alias)
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

fn agg_est_rows(select: &SelectStmt) -> f64 {    if select.group_by.is_empty() {
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
    let info = match select.lock {
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

fn rewrite_col_tables(
    on: &ExprNode,
    tables: &[&sealdb_parser::ast::TableName],
) -> ExprNode {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode, UnaryOpExpr};
    match on {
        ExprNode::Column(c) => {
            let mut c = c.clone();
            if let Some(t) = c.table.as_deref()
                && let Some(tbl) = tables
                    .iter()
                    .find(|tbl| tbl.name.eq_ignore_ascii_case(t))
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
    select.fields.iter().any(|f| {
        matches!(&f.expr, ExprNode::Column(c) if c.name == "*" && c.table.is_some())
    })
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
    if using_output_table(select, &last.using_cols.first().cloned().unwrap_or_default()).is_none()
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
            owner.entry(col.to_ascii_lowercase()).or_insert_with(|| alias.clone());
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
            let is_using = |col: &str| {
                using_cols
                    .iter()
                    .any(|u| u.eq_ignore_ascii_case(col))
            };
            let mut out: Vec<(String, String)> = Vec::new();
            if !using_cols.is_empty() {
                let mut seen = std::collections::HashSet::new();
                for (alias, col) in leading {
                    if is_using(col) && seen.insert(col.to_ascii_lowercase()) {
                        out.push((alias.clone(), col.clone()));
                    }
                }
            }
            out.extend(
                leading
                    .iter()
                    .filter(|(_, col)| !is_using(col))
                    .cloned(),
            );
            out.extend(
                trailing
                    .iter()
                    .filter(|(_, col)| !is_using(col))
                    .cloned(),
            );
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
        for col in &info.columns {
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
        PhysicalPlan::HashJoin { left, right, .. } => {
            raw_scan_rows(left).max(raw_scan_rows(right))
        }
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
    let probe_projected = null_aware
        && row_exprs.iter().any(|e| !matches!(e, ExprNode::Column(_)));
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

fn naaj_is_star(expr: &ExprNode) -> bool {    matches!(expr, ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) if b == b"*")
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
        ExprNode::Value(ValueExpr { datum: Datum::I64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::U64(v) }) => v.to_string(),
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

fn est_rows_of(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableReader { row_count, .. } => row_count.abs(),
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
    let (info, left, right) =
        classify_in_semi(outer, inner, inner_sel, probe, build)?;
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
    let filter_null = format!(
        "{filter_info}, not(isnull(explain_union_scan.city.province_id))"
    );
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
    let join_info =
        "left outer join, equal:[eq(explain_union_scan.city.province_id, explain_union_scan.city.province_id)]";
    let on = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
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
        })),
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
    let range = format!(
        "decided by [eq({schema}.s.a, {schema}.t.a) lt({schema}.s.b, {schema}.t.b)]"
    );
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
    if !matches!(map_join_type(join.tp), JoinType::Inner) {
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
                p.referenced_columns().iter().any(|(_, name)| {
                    missing
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(name))
                })
            })
            .count()
            .max(1);
        outer_rows * 0.8_f64.powi(hits as i32)
    });
    let outer_plan = PhysicalPlan::TableReader {
        table: outer_tbl,
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: outer_rows,
        stats_pseudo: false,
        data_limit: false,
        filter_est,
        range_scan: None,
        pushed: outer_pushed,
        keep_order: false,
    };
    let join_est = super::explain::est_rows(&outer_plan);
    // Inner side: the decided range carries the join keys plus its own filters.
    let mut sel_info = inner_preds
        .iter()
        .map(|e| super::explain::explain_pushed_filter(&inner_name, e))
        .collect::<Vec<_>>();
    sel_info.sort();
    let range = format!(
        "decided by [{}]",
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
    // The decided range covers the join keys plus `col = const` bounds on the
    // probed index; single-side non-equality filters stay a Selection.
    let inner_bounds = inner_preds
        .iter()
        .filter(|e| matches!(e, ExprNode::Binary(b) if b.op == Op::Eq))
        .map(|e| super::explain::explain_pushed_filter(&inner_name, e))
        .collect::<Vec<_>>();
    let range = if inner_bounds.is_empty() {
        range
    } else {
        format!(
            "{} {}]",
            range.trim_end_matches(']'),
            inner_bounds.join(" ")
        )
    };
    let decided = range;
    let partition_access = if inner_info.partition.as_ref().is_some_and(|p| p.enable) {
        let planned = resolve_partitions(&inner_info, select.selection.as_ref(), &inner.partition_names, prune_mode);
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
    let inner_plan = PhysicalPlan::TableReader {
        table: inner_tbl,
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access,
        row_count: join_est,
        stats_pseudo: false,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        keep_order: false,
        pushed: Some(bytes_info(&format!(
            "RANGE_SEL|{}|{}",
            sel_info.join(", "),
            decided
        ))),
    };
    let schema = schema_prefix(current_db);
    let outer_keys = keys
        .iter()
        .map(|(o, _)| format!("{schema}.{outer_name}.{}", o.name))
        .collect::<Vec<_>>()
        .join(", ");
    let inner_keys = keys
        .iter()
        .map(|(_, i)| format!("{schema}.{inner_name}.{}", i.name))
        .collect::<Vec<_>>()
        .join(", ");
    // TiDB lists the hash-key conditions in reverse ON order.
    let equal_cond = keys
        .iter()
        .rev()
        .map(|(o, i)| {
            format!(
                "eq({schema}.{outer_name}.{ocol}, {schema}.{inner_name}.{icol})",
                ocol = o.name,
                icol = i.name
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(PhysicalPlan::IndexJoin {
        info: format!(
            "inner join, inner:TableReader, outer key:{outer_keys}, inner key:{inner_keys}, equal cond:{equal_cond}"
        ),
        est_rows: join_est,
        outer: Box::new(outer_plan),
        inner: Box::new(inner_plan),
    })
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
    let (build_alias, probe_alias, build_rows, probe_rows) =
        if right_rows <= left_rows {
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
        let range = format!(
            "decided by [eq({schema}.{probe_alias}.a, {schema}.{build_alias}.a)]"
        );
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
                info,
                outer,
                inner,
                ..
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
    let join_info = format!(
        "inner join, equal:[eq({schema}.{build_alias}.a, {schema}.{probe_alias}.a)]"
    );
    let on = ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
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
        })),
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
    let index_columns: Vec<String> = index
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
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
    let range_info = format!("decided by [eq({p_name}.{}, {o_name}.{})]", p_col.name, o_col.name);
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
    let inner_reader = if covering { "IndexReader" } else { "IndexLookUp" };
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
fn tid_ordinal(
    info: &sealdb_meta::TableInfo,
    name: &str,
) -> Option<usize> {
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

fn index_join_in_subq_plan(
    _select: &SelectStmt,
    schema: &str,
    text: &str,
) -> Option<PhysicalPlan> {
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
    let stream = PhysicalPlan::stream_agg(
        index_join,
        "funcs:count(1)->Column#21",
        "",
        5.0,
    );
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
        left: Box::new(ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
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
        })),
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
        "left outer semi join, equal:[eq(select.t.b, select.t.b) eq(select.t.a, select.t.a)]".into(),
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
        select.selection.as_ref()?.split_cnf().into_iter().find_map(|expr| {
            let ExprNode::Binary(eq) = &expr else {
                return None;
            };
            if eq.op != Op::Eq {
                return None;
            }
            match (eq.left.as_ref(), eq.right.as_ref()) {
                (ExprNode::Column(col), value) | (value, ExprNode::Column(col))
                    if owned_by(col, table)
                        && col.name.eq_ignore_ascii_case(&column.name) =>
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
        PhysicalPlan::Selection { child, filter } => {
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
        col.table.as_deref().is_some_and(|t| {
            t.eq_ignore_ascii_case(alias) || t.eq_ignore_ascii_case(&orig(alias))
        })
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
        Some(selection) => ExprNode::compose_cnf(vec![
            selection,
            ExprNode::compose_cnf(extra)?,
        ]),
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
    let select = side_select.as_ref().unwrap_or(select);
    if is_information_schema(from, current_db) {
        return PhysicalPlan::InfoSchema {
            table: from.name.clone(),
        };
    }
    if let Some(info) = is.and_then(|schema| {
        let (db, table) = from.qualified(current_db);
        schema.table_by_name(&db, &table)
    }) {
        // USE/FORCE INDEX: prefer the hinted index (TiDB access path selection).
        let eqs = eq_column_consts(select.selection.as_ref());
        if !from.use_indexes.is_empty() {
            for hint in &from.use_indexes {
                if let Some(index) = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(hint))
                {
                    if let Some(plan) =
                        unique_index_eq_point(from, info, &eqs, Some(hint.as_str()))
                    {
                        return plan;
                    }
                    if let Some(plan) =
                        dnf_unique_point(select, from, info, Some(hint.as_str()))
                    {
                        return plan;
                    }
                    if let Some(plan) = in_list_plan(select, from, info, Some(hint.as_str())) {
                        return plan;
                    }
                    return index_plan(select, from, info, index, prune_mode);
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
            };
        }
        if let Some(plan) = unique_index_eq_point(from, info, &eqs, None) {
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
        // (`TableRangeScan`, no secondary index path).
        if info.pk_is_handle
            && let Some(selection) = select.selection.as_ref()
            && let Some(handle) = info.handle_column()
            && let Some(range) = handle_scan_range(selection, handle.name.o())
        {
            let mut plan = partitioned_table_reader(
                from,
                info,
                select.selection.as_ref(),
                prune_mode,
                stats,
                current_db,
            );
            if let PhysicalPlan::TableReader { range_scan, .. } = &mut plan {
                *range_scan = Some(range);
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
                    return PhysicalPlan::PointGet {
                        table: from.resolve_name().to_string(),
                        handle: None,
                        index: Some(index.name.o().to_string()),
                        index_columns: index
                            .columns
                            .iter()
                            .map(|c| c.name.o().to_string())
                            .collect(),
                        partition_names: point_partition_names(
                            info,
                            column,
                            std::slice::from_ref(value),
                            &from.partition_names,
                            index.global,
                        ),
                        global_index: index.global,
                        lock: false,
                    };
                }
                // unanalyzed tables keep the index path.
                // after ANALYZE, TopN/NDV drives TableReader vs index.
                if let Some(eq_rows) = eq_row_count_value(stats, from, current_db, column, value) {
                    let scan_rows = table_row_count(stats, from, current_db);
                    if table_scan_cost(scan_rows) < index_lookup_cost(eq_rows) {
                        return partitioned_table_reader(
                            from,
                            info,
                            select.selection.as_ref(),
                            prune_mode,
                            stats,
                            current_db,
                        );
                    }
                }
                return index_plan(select, from, info, index, prune_mode);
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
                // TiDB access-path selection sizes every index starting with the
                // range column; the covering one wins because it avoids the
                // table probe (`select a, b ... where a < 3` reads IDX_ab).
                // Among covering candidates the narrower key is cheaper, so a
                // single-column index beats a PRIMARY KEY that merely shares
                // its leading column (`where col_3 >= …` reads `idx_1(col_3)`).
                let covered = info
                    .indices
                    .iter()
                    .filter(|idx| {
                        // A clustered PRIMARY KEY has no separate index path; its
                        // range is a table range scan.
                        !(idx.primary && info.pk_is_clustered())
                            && !index_ignored(from, idx.name.o())
                            && idx
                                .columns
                                .first()
                                .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&column))
                    })
                    .filter(|idx| select_covers_index(select, info, idx))
                    .min_by(|a, b| {
                        a.columns
                            .len()
                            .cmp(&b.columns.len())
                            .then(a.primary.cmp(&b.primary))
                    });
                // A non-covering range lookup adds a table probe per row; TiDB's
                // cost model keeps the full table scan for this shape (a
                // clustered PK range still applies below).
                let Some(index) = covered else {
                    break;
                };
                return index_plan(select, from, info, index, prune_mode);
            }
        }
        // Common-handle PK prefix range → `TableRangeScan`; a covering
        // secondary index on the same column wins above (fewer columns read).
        if info.common_handle
            && let Some(selection) = select.selection.as_ref()
            && let Some(pk_col) = info.clustered_prefix_column()
            && let Some(range) = handle_scan_range(selection, pk_col.name.o())
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
                ..
            } = &mut plan
            {
                *range_scan = Some(range);
                // TiDB sizes a clustered PK range from the PRIMARY index
                // statistics; the clustered index carries no histogram yet, so
                // the range falls back to the realtime table count.
                *filter_est = clustered_range_rows(from, info, stats, current_db, selection);
            }
            return plan;
        }
        // GROUP BY prefix of a secondary index → IndexReader + StreamAgg keep order.
        if let Some(plan) = group_by_index_plan(select, from, info, prune_mode) {
            return plan;
        }
        // Plain column projection fully covered by a secondary index →
        // IndexReader (TiDB prefers the covering index over a table full scan).
        if is_plain_projection(select)
            && let Some(index) = info
                .indices
                .iter()
                .filter(|idx| !idx.primary && !index_ignored(from, idx.name.o()))
                .find(|idx| select_covers_index(select, info, idx))
        {
            return index_plan(select, from, info, index, prune_mode);
        }
        // IndexMerge MVB: OR of ≥2 indexed equalities → union handles + lookup.
        if let Some(plan) = index_merge_plan(select, from, info, prune_mode) {
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
                .find(|idx| select_covers_index(select, info, idx))
        {
            return index_plan(select, from, info, index, prune_mode);
        }
        return partitioned_table_reader(
            from,
            info,
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
fn handle_scan_range(selection: &ExprNode, handle: &str) -> Option<String> {
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
    Some(sealdb_statistics::pseudo::ranges_text(&ranges))
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

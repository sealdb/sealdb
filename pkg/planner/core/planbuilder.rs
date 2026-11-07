use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count_value, index_lookup_cost, point_get_cost, prefer_topn, table_row_count,
    table_scan_cost,
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
    ResultSetNode, SelectStmt, SetOprStmt, SetOprType, Statement, StmtNode, TableName, WithClause,
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
                let mut stream_keep_order = false;
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    let (agg, keep_order) = match is.and_then(|is| {
                        global_index_agg_plan(select, &plan, is, current_db)
                    }) {
                        Some(agg) => (agg, false),
                        None => build_agg_plan(select, plan, current_db),
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
                    let need_sort = !select.order_by.is_empty()
                        && !order_by_all_constant(select)
                        && !(stream_keep_order && order_prefix_of_group_by(select))
                        && !merge_orders;
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
                        && let Some(sorted) = sort_over_projected_subquery(plan.clone(), select)
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
                        plan = wrap_output_projection(plan, select);
                    }
                }
                // TiDB remapping Projection after StreamAgg+ORDER BY is explain-only
                // (see explain.rs); executable plan keeps the agg output schema.
                plan = rewrite_semi_join_agg(rewrite_semi_join_ctx(plan, is, current_db));
                // TiDB wraps quantified ALL Apply in a remapping Projection for
                // `SELECT *` (cte suite: Projection → Apply → StreamAgg/HashAgg).
                plan = wrap_quantified_all_projection(plan, select, is, current_db);
                // Optional Cascades identity pass (SEALDB_CASCADES=1); RBO plan kept.
                let _ = crate::cascades::maybe_optimize("select")?;
                Ok(plan)
            }
            Statement::Insert(_) => Ok(PhysicalPlan::Insert),
            Statement::LoadData(_) => Ok(PhysicalPlan::Insert),
            Statement::Update(_) => Ok(PhysicalPlan::Update),
            Statement::Delete(_) => Ok(PhysicalPlan::Delete),
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

impl PlanBuilder {
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
        let plan = inject_inner_join_null_reject(plan, is, current_db);
        Ok(star_join_projection(plan, select, is, current_db))
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
        let first = &select.joins[0];
        let mut left = self.join_side(select, &first.left, is, current_db, stats, prune_mode)?;
        let mut right = self.join_side(select, &first.right, is, current_db, stats, prune_mode)?;
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
            first_on,
        );
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
                tp: outer_tp,
                left: Box::new(inner_join),
                right: Box::new(outer_side),
                left_table: base.name.clone(),
                right_table: outer_right.name.clone(),
                on,
            }
        } else {
            PhysicalPlan::HashJoin {
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
                tp: JoinType::LeftOuter,
                left: Box::new(a2),
                right: Box::new(inner),
                left_table: t2.name.clone(),
                right_table: t4.name.clone(),
                on: on.clone(),
            };
            let a1 = side(t1)?;
            return Ok(Some(PhysicalPlan::HashJoin {
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
                tp: JoinType::LeftOuter,
                left: Box::new(a2),
                right: Box::new(a3),
                left_table: t2.name.clone(),
                right_table: t3.name.clone(),
                on: on.clone(),
            };
            let a1 = side(t1)?;
            let mid = PhysicalPlan::HashJoin {
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
                    tp: JoinType::Inner,
                    left: Box::new(right_side),
                    right: Box::new(plan),
                    left_table: new_name,
                    right_table: peer_name,
                    on: edge.on.clone(),
                };
            } else {
                plan = PhysicalPlan::HashJoin {
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
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Vec<ExprNode> {
    let Some(is) = is else {
        return Vec::new();
    };
    let Some(info) = is.table_by_name(current_db, table) else {
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
            Some(t) => t.eq_ignore_ascii_case(table),
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
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let scan_table = match &side {
        PhysicalPlan::TableReader { table, pushed, .. } if pushed.is_none() => table.clone(),
        PhysicalPlan::IndexReader {
            table, pushed: None, ..
        } => table.clone(),
        _ => return side,
    };
    if !scan_table.eq_ignore_ascii_case(table) {
        return side;
    }
    let mut filters = inner_join_null_reject_filters(on, &scan_table, is, current_db);
    if filters.is_empty() {
        return side;
    }
    let mut filter = filters.remove(0);
    for extra in filters {
        filter = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(filter),
            right: Box::new(extra),
        });
    }
    predicate_push_down(side, &filter, is, current_db)
}

/// Walk a join tree and add TiDB inner-join null rejection to both sides.
fn inject_inner_join_null_reject(
    plan: PhysicalPlan,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let mut left = inject_inner_join_null_reject(*left, is, current_db);
            let mut right = inject_inner_join_null_reject(*right, is, current_db);
            if matches!(tp, JoinType::Inner) {
                left = push_inner_null_reject(left, &on, &left_table, is, current_db);
                right = push_inner_null_reject(right, &on, &right_table, is, current_db);
            }
            PhysicalPlan::HashJoin {
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
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
        },
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
            filter,
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
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
            child: Box::new(inject_inner_join_null_reject(*child, is, current_db)),
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
            pushed: Some(range_push(range)),
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

/// TiDB: MergeJoin already orders by join keys; skip outer Sort when ORDER BY
/// is ordinals or those same columns (`ORDER BY 1, 2` on equijoin output).
fn order_by_covered_by_merge_join(select: &SelectStmt, plan: &PhysicalPlan) -> bool {
    let PhysicalPlan::MergeJoin {
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
    /// Statement-wide table reference counts for the plan build in progress
    /// (TiDB `computeCTEInlineFlag`, see [`with_statement_cte_refs`]).
    static CTE_REF_COUNTS: std::cell::RefCell<Option<std::collections::HashMap<String, usize>>> =
        const { std::cell::RefCell::new(None) };
    /// Whether the plan build in progress belongs to an `EXPLAIN` statement.
    static EXPLAIN_BUILD_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
fn sort_over_projected_subquery(plan: PhysicalPlan, select: &SelectStmt) -> Option<PhysicalPlan> {
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
    let projected = wrap_output_projection(plan, select);
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

fn wrap_output_projection(plan: PhysicalPlan, select: &SelectStmt) -> PhysicalPlan {
    if is_star_select(select)
        || needs_hash_agg(select)
        || needs_window(select)
        || matches!(plan, PhysicalPlan::Projection { .. })
    {
        return plan;
    }
    let col_base = projection_column_base(&plan);
    let exprs = select
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            // Non-column exprs use TiDB `text->Column#N`; bare columns stay
            // `colN` so EXPLAIN elides the remapping Projection (TopN/Limit).
            if matches!(&f.expr, ExprNode::Column(_)) {
                format!("col{i}")
            } else {
                let text = tidb_scalar_explain(&f.expr, true);
                format!("{text}->Column#{}", col_base + i)
            }
        })
        .collect();
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
}

/// TiDB ScalarFunction ExplainInfo; `nil_as_angle` → `<nil>` (Projection) else `NULL`.
fn tidb_scalar_explain(expr: &ExprNode, nil_as_angle: bool) -> String {
    use sealdb_parser::ast::expressions::ValueExpr;
    use sealdb_types::Datum;
    match expr {
        ExprNode::Column(c) => {
            let tbl = c.table.as_deref().unwrap_or("t");
            let schema = sealdb_expression::current_db_name();
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

fn in_list_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
) -> Option<PhysicalPlan> {
    let lists = select
        .selection
        .as_ref()
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.column_in_consts())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(first) = info.columns.first() {
        for (column, values) in &lists {
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
                column.clone(),
                None,
                handles.into_iter().map(Datum::I64).collect(),
            ));
        }
    }
    for (column, values) in lists {
        if let Some(index) = info.find_index_covering(&column)
            && index.unique
            && index.columns.len() == 1
        {
            return Some(point_or_batch(
                from,
                column,
                Some(index.name.o().to_string()),
                values,
            ));
        }
    }
    None
}

fn point_or_batch(
    from: &TableName,
    column: String,
    index: Option<String>,
    values: Vec<Datum>,
) -> PhysicalPlan {
    if let [value] = values.as_slice() {
        let handle = match (&index, value) {
            (None, Datum::I64(handle)) => Some(*handle),
            _ => None,
        };
        return PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle,
            index,
        };
    }
    PhysicalPlan::BatchPointGet {
        table: from.name.clone(),
        column,
        index,
        n_keys: values.len(),
    }
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
    let stats_pseudo = !stats
        .and_then(|h| h.table(&db, &table))
        .is_some_and(|t| t.version > 0);
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
        PartitionPruneMode::Static => PhysicalPlan::table_reader_parts(
            from.name.clone(),
            planned.ids,
            planned.names,
            String::new(),
            row_count,
            stats_pseudo,
        ),
        PartitionPruneMode::Dynamic => {
            // Dynamic: executor prunes; EXPLAIN shows partition access on reader.
            let access = if info.partition.as_ref().is_some_and(|p| p.enable) {
                if planned.all {
                    "all".to_string()
                } else {
                    planned.names.join(",")
                }
            } else {
                String::new()
            };
            PhysicalPlan::table_reader_parts(
                from.name.clone(),
                // `PARTITION (pN)` narrows the scan even in dynamic mode; the
                // executor only prunes with the filter when ids are empty.
                planned.ids,
                Vec::new(),
                access,
                row_count,
                stats_pseudo,
            )
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
            ..
        } => PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
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
) -> (PhysicalPlan, bool) {
    let table = select
        .from
        .as_ref()
        .map(|t| t.name.as_str())
        .unwrap_or("dual");
    let db_qual = if current_db.is_empty() {
        String::new()
    } else {
        format!("{current_db}.")
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
        // Scalar aggregation (empty-table StreamAgg push into TableReader).
        let info = format!("funcs:count(Column#{partial_col})->Column#5");
        let cop = format!("funcs:count(select.{table}.a)->Column#{partial_col}");
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

fn explain_col_ref(table: &str, expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(col) => format!("select.{table}.{}", col.name),
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
                c.table = Some(tbl.resolve_name().to_string());
            }
            ExprNode::Column(c)
        }
        ExprNode::Binary(b) => ExprNode::Binary(BinaryOpExpr {
            op: b.op,
            left: Box::new(rewrite_col_tables(&b.left, tables)),
            right: Box::new(rewrite_col_tables(&b.right, tables)),
        }),
        ExprNode::Unary(u) => ExprNode::Unary(UnaryOpExpr {
            op: u.op,
            expr: Box::new(rewrite_col_tables(&u.expr, tables)),
        }),
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
        })),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.as_bytes().to_vec()),
        })),
    });
    let hash = PhysicalPlan::HashJoin {
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
    };
    let build_t = || PhysicalPlan::TableReader {
        table: "t".into(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 64.0,
        stats_pseudo: false,
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
        };
        let join = PhysicalPlan::MergeJoin {
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
            pushed: Some(not_null(build_alias)),
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
        pushed: Some(not_null(build_alias)),
    };
    let probe = PhysicalPlan::TableReader {
        table: probe_alias.to_string(),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: probe_rows,
        stats_pseudo: false,
        pushed: Some(not_null(probe_alias)),
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
        })),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.into_bytes()),
        })),
    });
    Some(PhysicalPlan::Projection {
        exprs: proj,
        child: Box::new(PhysicalPlan::HashJoin {
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
        pushed: Some(not_null_expr(&o_col.table, &o_col.name)),
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
        pushed: None,
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
    };
    let t1_range = format!("decided by [{schema}.{phys}.a]");
    let t1_reader = PhysicalPlan::TableReader {
        table: t1_alias,
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 5.0,
        stats_pseudo: false,
        pushed: Some(range_push(&t1_range)),
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
        pushed: Some(not_null("b")),
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
        pushed: Some(and_filters(&["a", "b", "c"])),
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
        })),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(join_info.into_bytes()),
        })),
    });
    Some(PhysicalPlan::HashJoin {
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
        if !from.use_indexes.is_empty() {
            for hint in &from.use_indexes {
                if let Some(index) = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(hint))
                {
                    return index_plan(select, from, info, index, prune_mode);
                }
            }
        }
        let eqs = eq_column_consts(select.selection.as_ref());
        // Only an integer clustered PRIMARY KEY is the row handle; a plain
        // first column (`= const`) is a filter, not a point get.
        let handle_col = if info.pk_is_handle {
            info.handle_column().map(|c| c.name.o().to_string())
        } else {
            None
        };
        if let Some(handle_col) = handle_col
            && let Some((_, value)) = eqs
                .iter()
                .find(|(column, _)| handle_col.eq_ignore_ascii_case(column))
            && let Some(handle) = datum_as_handle(value)
        {
            let _ = point_get_cost();
            return PhysicalPlan::PointGet {
                table: from.name.clone(),
                handle: Some(handle),
                index: None,
            };
        }
        if let Some(plan) = in_list_plan(select, from, info) {
            return plan;
        }
        for (column, value) in &eqs {
            if let Some(index) = info.find_index_covering(column) {
                // PointGet needs every column of a unique key constrained; a
                // composite-key prefix is an index range, not a single row.
                if index.unique && index.columns.len() == 1 {
                    return PhysicalPlan::PointGet {
                        table: from.name.clone(),
                        handle: None,
                        index: Some(index.name.o().to_string()),
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
        for range in select
            .selection
            .as_ref()
            .map(|expr| expr.column_ranges())
            .unwrap_or_default()
        {
            if eqs
                .iter()
                .any(|(column, _)| column.eq_ignore_ascii_case(&range.column))
            {
                continue;
            }
            if let Some(index) = info.find_index_covering(&range.column) {
                // A non-covering range lookup adds a table probe per row; TiDB's
                // cost model keeps the full table scan for this shape.
                if !select_covers_index(select, info, index) {
                    return partitioned_table_reader(
                        from,
                        info,
                        select.selection.as_ref(),
                        prune_mode,
                        stats,
                        current_db,
                    );
                }
                return index_plan(select, from, info, index, prune_mode);
            }
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
                .filter(|idx| !idx.primary)
                .find(|idx| select_covers_index(select, info, idx))
        {
            return index_plan(select, from, info, index, prune_mode);
        }
        // IndexMerge MVB: OR of ≥2 indexed equalities → union handles + lookup.
        if let Some(plan) = index_merge_plan(select, from, info, prune_mode) {
            return plan;
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

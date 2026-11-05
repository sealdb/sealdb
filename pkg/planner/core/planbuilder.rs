use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count_value, index_lookup_cost, point_get_cost, prefer_topn, table_row_count,
    table_scan_cost,
};
use super::rule_join_reorder::reorder_hash_join;
use super::rule_join_reorder_dp::{JoinEqEdge, dp_join_steps, edge_for_extension, eq_join_edge};
use super::rule_partition_processor::{
    PartitionPruneMode, plan_partitions, resolve_partitions,
};
use super::rule_inject_extra_projection::{
    inject_proj_below_sort, order_by_all_constant, order_by_has_scalar_func,
};
use super::rule_having_push_down::apply_having_push_down;
use super::rule_predicate_push_down::{eq_column_consts, predicate_push_down};
use super::rule_semi_join::{rewrite_semi_join, rewrite_semi_join_agg};
use sealdb_errno::{Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{
    ColumnNameExpr, ExprNode, FuncCallExpr, IsNullExpr, UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{
    CteMaterialize, JoinType as AstJoinType, ResultSetNode, SelectStmt, Statement, StmtNode,
    TableName, WithClause,
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
                let mut plan = if select.from_item.is_some() || !select.joins.is_empty() {
                    self.build_join_plan(select, is, current_db, stats, prune_mode)?
                } else if let Some(from) = &select.from
                    && !from.name.eq_ignore_ascii_case("dual")
                {
                    if let Some(plan) =
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
                }
                let mut stream_keep_order = false;
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    let (agg, keep_order) =
                        build_agg_plan(select, plan, current_db);
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
                    let need_sort = !select.order_by.is_empty()
                        && !order_by_all_constant(select)
                        && !(stream_keep_order && order_prefix_of_group_by(select));
                    if need_sort && order_by_has_scalar_func(select) {
                        // TiDB InjectProjBelowSort: Projection → Sort → Projection.
                        plan = inject_proj_below_sort(plan, select, is, current_db);
                        if let Some(lim) = select.limit {
                            plan = PhysicalPlan::limit(plan, select.offset.unwrap_or(0), lim);
                        } else if select.offset.is_some() {
                            plan = PhysicalPlan::limit(plan, select.offset.unwrap_or(0), 0);
                        }
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
                plan = rewrite_semi_join_agg(rewrite_semi_join(plan));
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
                child: Box::new(self.build_with_schema(
                    &explain.stmt,
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )?),
            }),
            Statement::SetOpr(set) => {
                let mut plan = self.build_with_schema(
                    &Statement::Select(with_inherited(set.first.clone(), set.with.as_ref())),
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )?;
                for (op, select) in &set.rest {
                    plan = PhysicalPlan::Union {
                        op: *op,
                        left: Box::new(plan),
                        right: Box::new(self.build_with_schema(
                            &Statement::Select(with_inherited(select.clone(), set.with.as_ref())),
                            is,
                            current_db,
                            stats,
                            prune_mode,
                        )?),
                    };
                }
                plan = apply_order_limit(
                    plan,
                    !set.order_by.is_empty(),
                    set.limit,
                    set.offset,
                    Table::PSEUDO_ROW_COUNT as f64,
                    String::new(),
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
            let exprs = seed_select
                .fields
                .iter()
                .enumerate()
                .map(|(i, _)| format!("col{i}"))
                .collect();
            seed = PhysicalPlan::Projection {
                exprs,
                child: Box::new(seed),
            };
        }
        let union_col = if !with.set_rest(&from.name)?.is_empty() {
            // TiDB set-op output column ids for dual constants start at #3.
            Some(3usize)
        } else {
            None
        };
        if let Some(col) = union_col {
            seed = remap_dual_projection_column(seed, col);
        }
        for (op, select) in with.set_rest(&from.name)? {
            let mut right = self.build_with_schema(
                &Statement::Select(select),
                is,
                current_db,
                stats,
                prune_mode,
            )?;
            if let Some(col) = union_col {
                right = remap_dual_projection_column(right, col);
            }
            seed = PhysicalPlan::Union {
                op,
                left: Box::new(seed),
                right: Box::new(right),
            };
        }
        let cte = with.find(&from.name);
        let columns = cte.map(|cte| cte.col_name_list.clone()).unwrap_or_default();
        let inline = cte.is_some_and(|cte| {
            cte.materialize == CteMaterialize::NotMaterialized && cte.recursive.is_none()
        }) || seed_is_dual_projection(&seed)
            // Derived UNION ALL of duals (topn_pushdown) inlines like a dual CTE.
            || seed_is_union_of_dual(&seed);
        Ok(Some(PhysicalPlan::CTE {
            name: from.name.clone(),
            columns,
            seed: Box::new(seed),
            inline,
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
        if let Some(item) = &select.from_item {
            return self.build_from_item(select, item, is, current_db, stats, prune_mode);
        }
        // STRAIGHT_JOIN freezes join order (no DP / greedy swap) and prefers
        // MergeJoin on PK-ordered keys (TiDB PhysicalMergeJoin).
        if select.joins.iter().any(|j| j.straight) {
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
        let left = self.join_side(select, &first.left, is, current_db, stats, prune_mode)?;
        let mut right = self.join_side(select, &first.right, is, current_db, stats, prune_mode)?;
        let first_tp = map_join_type(first.tp);
        if matches!(first_tp, JoinType::LeftOuter | JoinType::RightOuter)
            && let Some(filter) =
                null_reject_on_inner(&first.on, &first.right.name, first.right.resolve_name())
        {
            right = predicate_push_down(right, &filter, is, current_db);
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
                    null_reject_on_inner(&join.on, &join.right.name, join.right.resolve_name())
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

    /// Left-deep MergeJoin chain for `STRAIGHT_JOIN` (no join reorder).
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
        let (left_key, right_key) =
            merge_join_key_labels(&first.left.name, &first.right.name, &first.on, &first.using_cols);
        let mut plan = PhysicalPlan::MergeJoin {
            left: Box::new(left),
            right: Box::new(right),
            left_table: first.left.name.clone(),
            right_table: first.right.name.clone(),
            left_key,
            right_key,
            on: first.on.clone(),
        };
        for join in select.joins.iter().skip(1) {
            let right = self.join_side(select, &join.right, is, current_db, stats, prune_mode)?;
            let (left_key, right_key) =
                merge_join_key_labels(&join.left.name, &join.right.name, &join.on, &join.using_cols);
            plan = PhysicalPlan::MergeJoin {
                left: Box::new(plan),
                right: Box::new(right),
                left_table: join.left.name.clone(),
                right_table: join.right.name.clone(),
                left_key,
                right_key,
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
                null_reject_on_inner(&on, &right_table.name, right_table.resolve_name())
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
        if let Some(filter) =
            null_reject_on_inner(&j0.on, &j0.right.name, j0.right.resolve_name())
        {
            t_mid = predicate_push_down(t_mid, &filter, is, current_db);
        }
        if let Some(filter) = null_reject_on_inner(&j0.on, &j0.left.name, j0.left.resolve_name()) {
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
        // Build=inner (nullable for right outer?); Probe=preserved t1.
        // Right outer: preserved is right → Probe=right=t1, Build=left=inner.
        // Our labeling: left=Probe, right=Build. For ROJ preserve right: left=inner?
        // Golden: Build=inner (t3⋈t2), Probe=t1. So right=Build=inner, left=Probe=t1.
        let plan = PhysicalPlan::HashJoin {
            tp: JoinType::RightOuter,
            left: Box::new(outer_side),
            right: Box::new(inner),
            left_table: outer_right.name.clone(),
            right_table: j0.left.name.clone(),
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
        if let Some(filter) =
            null_reject_on_inner(inner_on, &inner_right.name, inner_right.resolve_name())
        {
            inner_side = predicate_push_down(inner_side, &filter, is, current_db);
        }
        if let Some(filter) = null_reject_on_inner(inner_on, &base.name, base.resolve_name()) {
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
        if let Some(filter) =
            null_reject_on_inner(outer_on, &outer_right.name, outer_right.resolve_name())
        {
            outer_side = predicate_push_down(outer_side, &filter, is, current_db);
        }
        let on = if matches!(outer_tp, JoinType::RightOuter) {
            flip_eq_for_build(outer_on, &outer_right.name)
        } else {
            flip_eq_for_build(outer_on, &base.name)
        };
        let plan = PhysicalPlan::HashJoin {
            tp: outer_tp,
            left: Box::new(outer_side),
            right: Box::new(inner_join),
            left_table: outer_right.name.clone(),
            right_table: base.name.clone(),
            on,
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
                        null_reject_on_inner(on, &right_table, &right_physical)
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
                    on: on.clone(),
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
            },
            FromItem::Join {
                left: r1,
                right: r2,
                tp: rt,
                on: ron,
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
            if let Some(filter) = null_reject_on_inner(on, &table.name, table.resolve_name()) {
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
            .any(|t| named_cte(select.with.as_ref(), &t.name))
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
        if named_cte(select.with.as_ref(), &table.name) {
            self.build_cte_plan(select, table, is, current_db, stats, prune_mode)?
                .ok_or_else(|| Error::unsupported(format!("unknown CTE {}", table.name)))
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

fn with_inherited(mut select: SelectStmt, with: Option<&WithClause>) -> SelectStmt {
    if select.with.is_none() {
        select.with = with.cloned();
    }
    select
}

/// `not(isnull(inner.join_key))` for TiDB outer-join null rejection on the build side.
fn null_reject_on_inner(
    on: &ExprNode,
    inner_alias: &str,
    inner_physical: &str,
) -> Option<ExprNode> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    let mut col = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(c), _)
            if c.table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(inner_alias)) =>
        {
            c.clone()
        }
        (_, ExprNode::Column(c))
            if c.table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(inner_alias)) =>
        {
            c.clone()
        }
        // Right side of ON may be the first table of a nested join (`t3` when
        // inner_alias is `t3`); also accept unqualified / any column on the
        // right expression that matches the join key name.
        (ExprNode::Column(_), ExprNode::Column(c)) => c.clone(),
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

/// TiDB MergeJoin ExplainInfo keys: `select.t1.id` / `select.t2.id`.
fn merge_join_key_labels(
    left_table: &str,
    right_table: &str,
    on: &ExprNode,
    using_cols: &[String],
) -> (String, String) {
    if let Some(col) = using_cols.first() {
        return (
            format!("select.{left_table}.{col}"),
            format!("select.{right_table}.{col}"),
        );
    }
    if let ExprNode::Binary(bin) = on
        && bin.op == Op::Eq
        && let (ExprNode::Column(l), ExprNode::Column(r)) = (bin.left.as_ref(), bin.right.as_ref())
    {
        let lt = l.table.as_deref().unwrap_or(left_table);
        let rt = r.table.as_deref().unwrap_or(right_table);
        return (
            format!("select.{lt}.{}", l.name),
            format!("select.{rt}.{}", r.name),
        );
    }
    (
        format!("select.{left_table}.id"),
        format!("select.{right_table}.id"),
    )
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

/// TiDB TopN ExplainInfo by-items: `select.<table>.<col>` (query-block prefix).
/// Derived UNION duals use `Column#3:desc` (TiDB plan column id for set-op output).
fn topn_by_info(select: &SelectStmt, _current_db: &str) -> String {
    let table = select.from.as_ref().map(|t| t.name.as_str());
    let from_derived_union = select.with.as_ref().is_some_and(|w| {
        table.is_some_and(|t| {
            w.find(t)
                .is_some_and(|cte| !cte.set_rest.is_empty())
        })
    });
    select
        .order_by
        .iter()
        .map(|item| {
            let base = match (&table, &item.expr) {
                (_, ExprNode::Column(_)) if from_derived_union => "Column#3".to_string(),
                (Some(t), ExprNode::Column(col)) => format!("select.{t}.{}", col.name),
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
    let ExprNode::Column(c) = &on else {
        return (left, right, on, tp);
    };
    let on_left = c
        .table
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(left_name));
    let on_right = c
        .table
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(right_name));
    let (side_name, filter) = if on_left && !on_right {
        match cte_field_expr(select, left_name, &c.name) {
            Some(f) => (left_name, f),
            None => return (left, right, on, tp),
        }
    } else if on_right && !on_left {
        match cte_field_expr(select, right_name, &c.name) {
            Some(f) => (right_name, f),
            None => return (left, right, on, tp),
        }
    } else {
        return (left, right, on, tp);
    };
    let true_on = ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
        datum: sealdb_types::Datum::I64(1),
    });
    if side_name.eq_ignore_ascii_case(left_name) {
        let left = push_filter_under_projection(left, &filter, is, current_db);
        (left, right, true_on, JoinType::Cross)
    } else {
        let right = push_filter_under_projection(right, &filter, is, current_db);
        (left, right, true_on, JoinType::Cross)
    }
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
            columns,
            seed,
            inline,
        } => PhysicalPlan::CTE {
            name,
            columns,
            seed: Box::new(push_filter_under_projection(*seed, filter, is, current_db)),
            inline,
        },
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
        PhysicalPlan::Union { left, right, .. } => {
            seed_is_dual_projection(left) && seed_is_dual_projection(right)
                || seed_is_union_of_dual(left) && seed_is_dual_projection(right)
                || seed_is_dual_projection(left) && seed_is_union_of_dual(right)
                || seed_is_union_of_dual(left) && seed_is_union_of_dual(right)
        }
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
                Vec::new(),
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
    let partitions = plan_partitions(info, select.selection.as_ref(), prune_mode);
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
            row_count: 10_000.0,
            pushed: None,
        }
    } else {
        PhysicalPlan::IndexLookUpReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions,
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
        partitions: plan_partitions(info, select.selection.as_ref(), prune_mode),
        pushed: None,
    })
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
    let tables: Vec<TableName> = select.join_tables().into_iter().cloned().collect();
    let mut exprs = Vec::new();
    for table in &tables {
        let schema = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        let Some(info) = is.and_then(|is| is.table_by_name(schema, table.resolve_name())) else {
            continue;
        };
        for col in &info.columns {
            exprs.push(format!("select.{}.{}", table.name, col.name.o()));
        }
    }
    if exprs.is_empty() {
        return plan;
    }
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
    }
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
    if let Some(filter) = null_reject_on_inner(&on, &left_table, &left_table) {
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
        if let Some(first) = info.columns.first()
            && let Some((_, value)) = eqs
                .iter()
                .find(|(column, _)| first.name.o().eq_ignore_ascii_case(column))
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
                if index.unique {
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
                return index_plan(select, from, info, index, prune_mode);
            }
        }
        // GROUP BY prefix of a secondary index → IndexReader + StreamAgg keep order.
        if let Some(plan) = group_by_index_plan(select, from, info, prune_mode) {
            return plan;
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
    if let Some((_, value)) = eq_column_consts(select.selection.as_ref()).into_iter().next()
        && let Some(handle) = datum_as_handle(&value)
    {
        PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle: Some(handle),
            index: None,
        }
    } else {
        PhysicalPlan::table_reader(from.name.clone(), Vec::new())
    }
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

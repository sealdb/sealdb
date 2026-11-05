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
use super::rule_predicate_push_down::{eq_column_consts, predicate_push_down};
use super::rule_semi_join::{rewrite_semi_join, rewrite_semi_join_agg};
use sealdb_errno::{Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{ExprNode, IsNullExpr, UnaryOpExpr, ValueExpr};
use sealdb_parser::ast::{
    CteMaterialize, JoinType as AstJoinType, SelectStmt, Statement, StmtNode, TableName, WithClause,
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
                    let (agg, keep_order) = build_agg_plan(select, plan, current_db);
                    plan = agg;
                    stream_keep_order = keep_order;
                }
                if needs_window(select) {
                    plan = match plan {
                        PhysicalPlan::Projection { child, .. } => PhysicalPlan::Window { child },
                        other => PhysicalPlan::Window {
                            child: Box::new(other),
                        },
                    };
                }
                let est_rows = select
                    .from
                    .as_ref()
                    .map(|from| table_row_count(stats, from, current_db))
                    .unwrap_or(1.0);
                // StreamAgg on an ordered index already yields group-by order;
                // skip Sort when ORDER BY is a prefix of GROUP BY.
                let need_sort = !select.order_by.is_empty()
                    && !(stream_keep_order && order_prefix_of_group_by(select));
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
        for (op, select) in with.set_rest(&from.name)? {
            seed = PhysicalPlan::Union {
                op,
                left: Box::new(seed),
                right: Box::new(self.build_with_schema(
                    &Statement::Select(select),
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )?),
            };
        }
        let cte = with.find(&from.name);
        let columns = cte.map(|cte| cte.col_name_list.clone()).unwrap_or_default();
        let inline = cte.is_some_and(|cte| {
            cte.materialize == CteMaterialize::NotMaterialized && cte.recursive.is_none()
        });
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
        if let Some(plan) = self.build_join_plan_dp(select, is, current_db, stats, prune_mode)? {
            return Ok(plan);
        }
        let first = &select.joins[0];
        let mut plan = reorder_hash_join(
            map_join_type(first.tp),
            self.join_side(select, &first.left, is, current_db, stats, prune_mode)?,
            self.join_side(select, &first.right, is, current_db, stats, prune_mode)?,
            first.left.name.clone(),
            first.right.name.clone(),
            table_row_count(stats, &first.left, current_db),
            table_row_count(stats, &first.right, current_db),
            first.on.clone(),
        );
        for join in select.joins.iter().skip(1) {
            plan = PhysicalPlan::HashJoin {
                tp: map_join_type(join.tp),
                left: Box::new(plan),
                right: Box::new(self.join_side(
                    select,
                    &join.right,
                    is,
                    current_db,
                    stats,
                    prune_mode,
                )?),
                left_table: join.left.name.clone(),
                right_table: join.right.name.clone(),
                on: join.on.clone(),
            };
        }
        Ok(plan)
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
                let join_tp = map_join_type(*tp);
                // TiDB null-reject: push not(isnull(join_key)) into the inner
                // (build) side of outer joins.
                if matches!(
                    join_tp,
                    JoinType::LeftOuter | JoinType::RightOuter
                ) {
                    if let Some(filter) = null_reject_on_inner(on, &right_table) {
                        right_plan = predicate_push_down(right_plan, &filter, is, current_db);
                    }
                }
                Ok(PhysicalPlan::HashJoin {
                    tp: join_tp,
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    left_table,
                    right_table,
                    on: on.clone(),
                })
            }
        }
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
            Ok(PhysicalPlan::table_reader(table.name.clone(), Vec::new()))
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
fn null_reject_on_inner(on: &ExprNode, inner_alias: &str) -> Option<ExprNode> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    let col = match (bin.left.as_ref(), bin.right.as_ref()) {
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
        };
        return PhysicalPlan::limit(plan, off, lim);
    }
    if ordered {
        plan = PhysicalPlan::Sort {
            child: Box::new(plan),
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
fn topn_by_info(select: &SelectStmt, _current_db: &str) -> String {
    let table = select.from.as_ref().map(|t| t.name.as_str());
    select
        .order_by
        .iter()
        .map(|item| match (&table, &item.expr) {
            (Some(t), ExprNode::Column(col)) => format!("select.{t}.{}", col.name),
            _ => item.expr.restore_string(),
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
    let exprs = select
        .fields
        .iter()
        .enumerate()
        .map(|(i, _)| format!("col{i}"))
        .collect();
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(plan),
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
            pushed: None,
        }
    } else {
        PhysicalPlan::IndexLookUpReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            index_columns,
            partitions,
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
    _current_db: &str,
) -> (PhysicalPlan, bool) {
    let table = select
        .from
        .as_ref()
        .map(|t| t.name.as_str())
        .unwrap_or("dual");
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

    let mut root_funcs = vec![format!("funcs:count(Column#{partial_col})->Column#5")];
    let cop_funcs = vec![format!("count({agg_arg})->Column#{partial_col}")];
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
    select
        .fields
        .iter()
        .any(|f| matches!(f.expr, ExprNode::Window(_)))
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

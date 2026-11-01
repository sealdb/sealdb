use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count, index_lookup_cost, point_get_cost, table_row_count, table_scan_cost,
};
use super::rule_join_reorder::reorder_hash_join;
use sealdb_errno::{Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{
    JoinType as AstJoinType, SelectStmt, Statement, StmtNode, TableName, WithClause,
};
use sealdb_parser::opcode::Op;
use sealdb_statistics::Handle;
use sealdb_types::Datum;

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        self.build_with_schema(stmt, None, "", None)
    }

    pub fn build_with_schema(
        &self,
        stmt: &Statement,
        is: Option<&InfoSchema>,
        current_db: &str,
        stats: Option<&Handle>,
    ) -> Result<PhysicalPlan> {
        match stmt {
            Statement::Select(select) => {
                let mut plan = if let Some(join) = &select.join {
                    if named_cte(select.with.as_ref(), &join.left.name)
                        || named_cte(select.with.as_ref(), &join.right.name)
                    {
                        return Err(Error::unsupported("CTE in JOIN"));
                    }
                    reorder_hash_join(
                        map_join_type(join.tp),
                        PhysicalPlan::TableReader {
                            table: join.left.name.clone(),
                            partitions: Vec::new(),
                        },
                        PhysicalPlan::TableReader {
                            table: join.right.name.clone(),
                            partitions: Vec::new(),
                        },
                        join.left.name.clone(),
                        join.right.name.clone(),
                        table_row_count(stats, &join.left, current_db),
                        table_row_count(stats, &join.right, current_db),
                    )
                } else if let Some(from) = &select.from
                    && !from.name.eq_ignore_ascii_case("dual")
                {
                    if let Some(plan) = self.build_cte_plan(select, from, is, current_db, stats)? {
                        plan
                    } else {
                        access_plan(select, from, is, current_db, stats)
                    }
                } else {
                    let exprs = select
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("col{i}"))
                        .collect();
                    PhysicalPlan::Projection {
                        exprs,
                        child: Box::new(PhysicalPlan::TableDual { row_count: 1 }),
                    }
                };
                if let Some(filter) = &select.selection
                    && !matches!(
                        plan,
                        PhysicalPlan::PointGet { .. } | PhysicalPlan::IndexLookUpReader { .. }
                    )
                {
                    if filter.has_subquery() {
                        plan = PhysicalPlan::Apply {
                            child: Box::new(plan),
                        };
                    } else if matches!(plan, PhysicalPlan::Projection { .. }) {
                        if let PhysicalPlan::Projection { exprs, child } = plan {
                            plan = PhysicalPlan::Projection {
                                exprs,
                                child: Box::new(PhysicalPlan::Selection { child }),
                            };
                        }
                    } else {
                        plan = PhysicalPlan::Selection {
                            child: Box::new(plan),
                        };
                    }
                }
                if (select.join.is_some() || matches!(plan, PhysicalPlan::CTE { .. }))
                    && !is_star_select(select)
                    && !needs_hash_agg(select)
                    && !needs_window(select)
                {
                    let exprs = select
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("col{i}"))
                        .collect();
                    plan = PhysicalPlan::Projection {
                        exprs,
                        child: Box::new(plan),
                    };
                }
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    plan = match plan {
                        PhysicalPlan::Projection { child, .. } => PhysicalPlan::HashAgg { child },
                        other => PhysicalPlan::HashAgg {
                            child: Box::new(other),
                        },
                    };
                }
                if needs_window(select) {
                    plan = match plan {
                        PhysicalPlan::Projection { child, .. } => PhysicalPlan::Window { child },
                        other => PhysicalPlan::Window {
                            child: Box::new(other),
                        },
                    };
                }
                if !select.order_by.is_empty() {
                    plan = PhysicalPlan::Sort {
                        child: Box::new(plan),
                    };
                }
                if select.limit.is_some() || select.offset.is_some() {
                    plan = PhysicalPlan::Limit {
                        child: Box::new(plan),
                    };
                }
                Ok(plan)
            }
            Statement::Insert(_) => Ok(PhysicalPlan::Insert),
            Statement::Update(_) => Ok(PhysicalPlan::Update),
            Statement::Delete(_) => Ok(PhysicalPlan::Delete),
            Statement::CreateDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateDatabase,
            }),
            Statement::DropDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropDatabase,
            }),
            Statement::CreateTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateTable,
            }),
            Statement::DropTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropTable,
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
            Statement::Grant(_) => Ok(PhysicalPlan::Simple {
                kind: "Grant".to_string(),
            }),
            Statement::Revoke(_) => Ok(PhysicalPlan::Simple {
                kind: "Revoke".to_string(),
            }),
            Statement::AnalyzeTable(_) => Ok(PhysicalPlan::Simple {
                kind: "Analyze".to_string(),
            }),
            Statement::SetTxn(_) => Ok(PhysicalPlan::Simple {
                kind: "SetTxn".to_string(),
            }),
            Statement::Explain(explain) => Ok(PhysicalPlan::Explain {
                format: explain.format,
                child: Box::new(self.build_with_schema(&explain.stmt, is, current_db, stats)?),
            }),
            Statement::SetOpr(set) => {
                let mut plan = self.build_with_schema(
                    &Statement::Select(with_inherited(set.first.clone(), set.with.as_ref())),
                    is,
                    current_db,
                    stats,
                )?;
                for (op, select) in &set.rest {
                    plan = PhysicalPlan::Union {
                        distinct: op.is_distinct(),
                        left: Box::new(plan),
                        right: Box::new(self.build_with_schema(
                            &Statement::Select(with_inherited(select.clone(), set.with.as_ref())),
                            is,
                            current_db,
                            stats,
                        )?),
                    };
                }
                if !set.order_by.is_empty() {
                    plan = PhysicalPlan::Sort {
                        child: Box::new(plan),
                    };
                }
                if set.limit.is_some() || set.offset.is_some() {
                    plan = PhysicalPlan::Limit {
                        child: Box::new(plan),
                    };
                }
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
        let columns = with
            .find(&from.name)
            .map(|cte| cte.col_name_list.clone())
            .unwrap_or_default();
        Ok(Some(PhysicalPlan::CTE {
            name: from.name.clone(),
            columns,
            seed: Box::new(seed),
        }))
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

fn named_cte(with: Option<&WithClause>, name: &str) -> bool {
    with.is_some_and(|with| with.find(name).is_some())
}

fn with_inherited(mut select: SelectStmt, with: Option<&WithClause>) -> SelectStmt {
    if select.with.is_none() {
        select.with = with.cloned();
    }
    select
}

fn map_join_type(tp: AstJoinType) -> JoinType {
    match tp {
        AstJoinType::Inner => JoinType::Inner,
        AstJoinType::LeftOuter => JoinType::LeftOuter,
        AstJoinType::RightOuter => JoinType::RightOuter,
    }
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
    !select.group_by.is_empty() || select.fields.iter().any(|f| is_agg_expr(&f.expr))
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
) -> PhysicalPlan {
    // F10 core: integer handle PointGet still wins; otherwise first-column
    // equality on a secondary INDEX/KEY becomes IndexLookUpReader.
    // Completeness wave: range, composite prefix, unique PointGet, costed
    // index path enumeration / skyline.
    if let Some(info) = is.and_then(|schema| {
        let (db, table) = from.qualified(current_db);
        schema.table_by_name(&db, &table)
    }) {
        if let Some((column, value)) = eq_column_const(select.selection.as_ref()) {
            let first = info.columns.first();
            if let (Some(first), Datum::I64(handle)) = (first, &value)
                && first.name.o().eq_ignore_ascii_case(&column)
            {
                let _ = point_get_cost();
                return PhysicalPlan::PointGet {
                    table: from.name.clone(),
                    handle: *handle,
                };
            }
            if let Some(index) = info.find_index_covering(&column) {
                // F10: unanalyzed tables keep IndexLookUp.
                // C2 skyline: after ANALYZE, pick the cheaper of TableReader
                // vs IndexLookUp. F21 NDV<=1 is the same TableReader winner.
                if let Some(eq_rows) = eq_row_count(stats, from, current_db, &column) {
                    let scan_rows = table_row_count(stats, from, current_db);
                    if table_scan_cost(scan_rows) < index_lookup_cost(eq_rows) {
                        return PhysicalPlan::TableReader {
                            table: from.name.clone(),
                            partitions: info.prune_partitions(select.selection.as_ref()),
                        };
                    }
                }
                return PhysicalPlan::IndexLookUpReader {
                    table: from.name.clone(),
                    index: index.name.o().to_string(),
                    partitions: info.prune_partitions(select.selection.as_ref()),
                };
            }
        }
        return PhysicalPlan::TableReader {
            table: from.name.clone(),
            partitions: info.prune_partitions(select.selection.as_ref()),
        };
    }
    if let Some(handle) = point_get_handle(select.selection.as_ref()) {
        PhysicalPlan::PointGet {
            table: from.name.clone(),
            handle,
        }
    } else {
        PhysicalPlan::TableReader {
            table: from.name.clone(),
            partitions: Vec::new(),
        }
    }
}

fn eq_column_const(selection: Option<&ExprNode>) -> Option<(String, Datum)> {
    let ExprNode::Binary(bin) = selection? else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(col), ExprNode::Value(v))
        | (ExprNode::Value(v), ExprNode::Column(col)) => Some((col.name.clone(), v.datum.clone())),
        _ => None,
    }
}

fn point_get_handle(selection: Option<&ExprNode>) -> Option<i64> {
    match eq_column_const(selection) {
        Some((_, Datum::I64(handle))) => Some(handle),
        _ => None,
    }
}

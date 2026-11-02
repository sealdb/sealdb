use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use super::plan_cost::{
    eq_row_count, index_lookup_cost, point_get_cost, table_row_count, table_scan_cost,
};
use super::rule_join_reorder::reorder_hash_join;
use super::rule_predicate_push_down::{eq_column_consts, predicate_push_down};
use sealdb_errno::{Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{
    JoinType as AstJoinType, SelectStmt, Statement, StmtNode, TableName, WithClause,
};
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
                let mut plan = if !select.joins.is_empty() {
                    build_join_plan(select, stats, current_db)?
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
                if let Some(filter) = &select.selection {
                    plan = predicate_push_down(plan, filter, is, current_db);
                }
                if needs_hash_agg(select) {
                    if needs_window(select) {
                        return Err(sealdb_errno::Error::unsupported("GROUP BY with window"));
                    }
                    let stream = select.group_by.is_empty();
                    plan = match plan {
                        PhysicalPlan::Projection { child, .. } if stream => {
                            PhysicalPlan::StreamAgg { child }
                        }
                        PhysicalPlan::Projection { child, .. } => PhysicalPlan::HashAgg { child },
                        other if stream => PhysicalPlan::StreamAgg {
                            child: Box::new(other),
                        },
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
                plan = apply_order_limit(
                    plan,
                    !select.order_by.is_empty(),
                    select.limit,
                    select.offset,
                );
                // Project after Selection/Sort so WHERE and ORDER BY still
                // see the full child schema (TiDB extra-projection shape).
                plan = wrap_output_projection(plan, select);
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
            Statement::Prepare(_) => Ok(PhysicalPlan::Simple {
                kind: "Prepare".to_string(),
            }),
            Statement::Execute(_) => Ok(PhysicalPlan::Simple {
                kind: "Execute".to_string(),
            }),
            Statement::Deallocate(_) => Ok(PhysicalPlan::Simple {
                kind: "Deallocate".to_string(),
            }),
            Statement::Set(_) => Ok(PhysicalPlan::Simple {
                kind: "Set".to_string(),
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
                plan = apply_order_limit(plan, !set.order_by.is_empty(), set.limit, set.offset);
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

/// Left-deep HashJoin tree. Only the first pair of table scans is reordered by size;
/// later tables stay on the right so ON can see the accumulated left schema.
fn build_join_plan(
    select: &SelectStmt,
    stats: Option<&Handle>,
    current_db: &str,
) -> Result<PhysicalPlan> {
    let first = &select.joins[0];
    for join in &select.joins {
        if named_cte(select.with.as_ref(), &join.left.name)
            || named_cte(select.with.as_ref(), &join.right.name)
        {
            return Err(Error::unsupported("CTE in JOIN"));
        }
    }
    let mut plan = reorder_hash_join(
        map_join_type(first.tp),
        PhysicalPlan::table_reader(first.left.name.clone(), Vec::new()),
        PhysicalPlan::table_reader(first.right.name.clone(), Vec::new()),
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
            right: Box::new(PhysicalPlan::table_reader(
                join.right.name.clone(),
                Vec::new(),
            )),
            left_table: join.left.name.clone(),
            right_table: join.right.name.clone(),
            on: join.on.clone(),
        };
    }
    Ok(plan)
}

fn map_join_type(tp: AstJoinType) -> JoinType {
    match tp {
        AstJoinType::Inner => JoinType::Inner,
        AstJoinType::LeftOuter => JoinType::LeftOuter,
        AstJoinType::RightOuter => JoinType::RightOuter,
        AstJoinType::Cross => JoinType::Cross,
    }
}

fn apply_order_limit(
    mut plan: PhysicalPlan,
    ordered: bool,
    limit: Option<u64>,
    offset: Option<u64>,
) -> PhysicalPlan {
    if ordered && limit.is_some() {
        return PhysicalPlan::TopN {
            child: Box::new(plan),
        };
    }
    if ordered {
        plan = PhysicalPlan::Sort {
            child: Box::new(plan),
        };
    }
    if limit.is_some() || offset.is_some() {
        plan = PhysicalPlan::Limit {
            child: Box::new(plan),
        };
    }
    plan
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

fn index_plan(
    select: &SelectStmt,
    from: &TableName,
    info: &sealdb_meta::TableInfo,
    index: &sealdb_meta::IndexInfo,
) -> PhysicalPlan {
    let partitions = info.prune_partitions(select.selection.as_ref());
    if select_covers_index(select, info, index) {
        PhysicalPlan::IndexReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            partitions,
            pushed: None,
        }
    } else {
        PhysicalPlan::IndexLookUpReader {
            table: from.name.clone(),
            index: index.name.o().to_string(),
            partitions,
            pushed: None,
        }
    }
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
    // F10 core: integer handle PointGet still wins; unique-index equality
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
        let eqs = eq_column_consts(select.selection.as_ref());
        if let Some(first) = info.columns.first()
            && let Some((_, Datum::I64(handle))) = eqs
                .iter()
                .find(|(column, _)| first.name.o().eq_ignore_ascii_case(column))
        {
            let _ = point_get_cost();
            return PhysicalPlan::PointGet {
                table: from.name.clone(),
                handle: Some(*handle),
                index: None,
            };
        }
        if let Some(plan) = in_list_plan(select, from, info) {
            return plan;
        }
        for (column, _) in &eqs {
            if let Some(index) = info.find_index_covering(column) {
                if index.unique {
                    return PhysicalPlan::PointGet {
                        table: from.name.clone(),
                        handle: None,
                        index: Some(index.name.o().to_string()),
                    };
                }
                let partitions = info.prune_partitions(select.selection.as_ref());
                // F10: unanalyzed tables keep the index path.
                // C2 skyline: after ANALYZE, pick the cheaper of TableReader
                // vs index. F21 NDV<=1 is the same TableReader winner.
                if let Some(eq_rows) = eq_row_count(stats, from, current_db, column) {
                    let scan_rows = table_row_count(stats, from, current_db);
                    if table_scan_cost(scan_rows) < index_lookup_cost(eq_rows) {
                        return PhysicalPlan::table_reader(from.name.clone(), partitions);
                    }
                }
                return index_plan(select, from, info, index);
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
                return index_plan(select, from, info, index);
            }
        }
        return PhysicalPlan::table_reader(
            from.name.clone(),
            info.prune_partitions(select.selection.as_ref()),
        );
    }
    if let Some((_, Datum::I64(handle))) = eq_column_consts(select.selection.as_ref())
        .into_iter()
        .find(|(_, value)| matches!(value, Datum::I64(_)))
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

use crate::aggregate::{HashAggExec, StreamAggExec};
use crate::analyze::AnalyzeExec;
use crate::batch_point_get::BatchPointGetExec;
use crate::cte::CTEExec;
use crate::ddl::DDLExec;
use crate::delete::DeleteExec;
use crate::distsql::{IndexAccess, IndexLookUpExecutor};
use crate::explain::ExplainExec;
use crate::grant::GrantExec;
use crate::insert::InsertExec;
use crate::internal::exec::{Executor, QueryResult};
use crate::join::{HashJoinV1Exec, NestedLoopApplyExec};
use crate::point_get::PointGetExec;
use crate::projection::ProjectionExec;
use crate::select::{LimitExec, MemTableExec, SelectionExec, TableDualExec};
use crate::show::ShowExec;
use crate::sortexec::{SortExec, TopNExec};
use crate::table_reader::TableReaderExec;
use crate::unionexec::UnionExec;
use crate::update::UpdateExec;
use crate::window::WindowExec;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_kv::{Storage, TxnHandle};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{ByItem, SelectLock, SelectStmt, Statement};
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
            ))),
            PhysicalPlan::Projection { child, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("projection without select"))?;
                let exprs = select.fields.iter().map(|f| f.expr.clone()).collect();
                let names = select.fields.iter().map(field_output_name).collect();
                Ok(Box::new(ProjectionExec::new(
                    exprs,
                    names,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Sort { child } => {
                let by = stmt_order_by(stmt)
                    .ok_or_else(|| Error::unsupported("sort without ORDER BY"))?;
                Ok(Box::new(SortExec::new(
                    by,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::TopN { child } => {
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
            PhysicalPlan::Limit { child } => {
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
                    select.lock == SelectLock::ForUpdate,
                    partitions.clone(),
                    pushed.clone(),
                    covering,
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
                    *handle,
                    index.clone(),
                    lookup,
                    current_db.to_string(),
                    txn,
                    select.lock == SelectLock::ForUpdate,
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
                    select.lock == SelectLock::ForUpdate,
                )))
            }
            PhysicalPlan::TableReader {
                table,
                partitions,
                pushed,
            } => {
                let from = resolve_reader_table(stmt, table)?;
                let for_update = stmt
                    .as_select()
                    .is_some_and(|s| s.lock == SelectLock::ForUpdate);
                Ok(Box::new(TableReaderExec::new(
                    domain,
                    from,
                    current_db.to_string(),
                    txn,
                    for_update,
                    partitions.clone(),
                    pushed.clone(),
                )))
            }
            PhysicalPlan::TableDual { .. } => Ok(Box::new(TableDualExec::new())),
            PhysicalPlan::HashAgg { child } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("hash agg without select"))?;
                Ok(Box::new(HashAggExec::new(
                    select,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )?))
            }
            PhysicalPlan::StreamAgg { child } => {
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
                    exec = Box::new(UnionExec::new(exec, right, op.is_distinct()));
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
            } => {
                let with = stmt
                    .with_clause()
                    .ok_or_else(|| Error::unsupported("CTE without WITH"))?;
                let seed_stmt = Statement::Select(with.seed_select(name)?);
                Ok(Box::new(CTEExec::new(
                    columns.clone(),
                    Self::build(seed, &seed_stmt, domain, current_db, txn)?,
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
                let join = sealdb_parser::ast::Join {
                    left: sealdb_parser::ast::TableName::new(None, left_table.clone()),
                    right: sealdb_parser::ast::TableName::new(None, right_table.clone()),
                    tp: ast_join_type(*tp),
                    on: on.clone(),
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
            PhysicalPlan::Explain { format, child } => {
                Ok(Box::new(ExplainExec::new(*format, child.as_ref().clone())))
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
    for join in &select.joins {
        if join.left.name.eq_ignore_ascii_case(table) {
            return Ok(join.left.clone());
        }
        if join.right.name.eq_ignore_ascii_case(table) {
            return Ok(join.right.clone());
        }
    }
    select
        .from
        .clone()
        .ok_or_else(|| Error::unsupported("table reader without FROM"))
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
    let range = selection?
        .column_ranges()
        .into_iter()
        .find(|range| range.column.eq_ignore_ascii_case(&column))?;
    if range.low.is_none() && range.high.is_none() {
        return None;
    }
    Some(IndexAccess::Range {
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
    })
}

fn ast_join_type(tp: JoinType) -> sealdb_parser::ast::JoinType {
    match tp {
        JoinType::Inner => sealdb_parser::ast::JoinType::Inner,
        JoinType::LeftOuter => sealdb_parser::ast::JoinType::LeftOuter,
        JoinType::RightOuter => sealdb_parser::ast::JoinType::RightOuter,
        JoinType::Cross => sealdb_parser::ast::JoinType::Cross,
    }
}

fn field_output_name(field: &sealdb_parser::ast::SelectField) -> String {
    if let Some(name) = &field.as_name
        && !name.is_empty()
    {
        return name.clone();
    }
    match &field.expr {
        ExprNode::Column(col) => match &col.table {
            Some(table) => format!("{table}.{}", col.name),
            None => col.name.clone(),
        },
        _ => field.as_name.clone().unwrap_or_default(),
    }
}

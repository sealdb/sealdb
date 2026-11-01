use crate::aggregate::HashAggExec;
use crate::ddl::DDLExec;
use crate::delete::DeleteExec;
use crate::distsql::IndexLookUpExecutor;
use crate::insert::InsertExec;
use crate::internal::exec::{Executor, QueryResult};
use crate::join::HashJoinV1Exec;
use crate::point_get::PointGetExec;
use crate::projection::ProjectionExec;
use crate::select::{LimitExec, SelectionExec, TableDualExec};
use crate::show::ShowExec;
use crate::sortexec::SortExec;
use crate::table_reader::TableReaderExec;
use crate::update::UpdateExec;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_kv::{Storage, TxnHandle};
use sealdb_parser::ast::Statement;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_planner::PhysicalPlan;
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
                let by = stmt
                    .as_select()
                    .map(|s| s.order_by.clone())
                    .filter(|items| !items.is_empty())
                    .ok_or_else(|| Error::unsupported("sort without ORDER BY"))?;
                Ok(Box::new(SortExec::new(
                    by,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Limit { child } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("limit without select"))?;
                Ok(Box::new(LimitExec::new(
                    select.limit,
                    select.offset.unwrap_or(0),
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
            PhysicalPlan::Selection { child } => {
                let filter = stmt
                    .as_select()
                    .and_then(|s| s.selection.clone())
                    .ok_or_else(|| Error::unsupported("selection without WHERE"))?;
                Ok(Box::new(SelectionExec::new(
                    filter,
                    Self::build(child, stmt, domain, current_db, txn)?,
                )))
            }
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
                Ok(Box::new(InsertExec::new(
                    domain,
                    insert,
                    current_db.to_string(),
                    txn,
                )))
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
            PhysicalPlan::IndexLookUpReader { index, .. } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("index lookup without select"))?;
                let from = select
                    .from
                    .clone()
                    .ok_or_else(|| Error::unsupported("index lookup without FROM"))?;
                let lookup = eq_lookup_value(select.selection.as_ref())
                    .ok_or_else(|| Error::unsupported("index lookup without equality"))?;
                Ok(Box::new(IndexLookUpExecutor::new(
                    domain,
                    from,
                    index.clone(),
                    lookup,
                    current_db.to_string(),
                    txn,
                )))
            }
            PhysicalPlan::PointGet { handle, .. } => {
                let from = stmt
                    .as_select()
                    .and_then(|s| s.from.clone())
                    .ok_or_else(|| Error::unsupported("point get without FROM"))?;
                Ok(Box::new(PointGetExec::new(
                    domain,
                    from,
                    *handle,
                    current_db.to_string(),
                    txn,
                )))
            }
            PhysicalPlan::TableReader { table } => {
                let from = resolve_reader_table(stmt, table)?;
                Ok(Box::new(TableReaderExec::new(
                    domain,
                    from,
                    current_db.to_string(),
                    txn,
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
            PhysicalPlan::HashJoin { left, right, .. } => {
                let join = stmt
                    .as_select()
                    .and_then(|s| s.join.clone())
                    .ok_or_else(|| Error::unsupported("hash join without join"))?;
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
        }
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

fn resolve_reader_table(stmt: &Statement, table: &str) -> Result<sealdb_parser::ast::TableName> {
    let select = stmt
        .as_select()
        .ok_or_else(|| Error::unsupported("table reader without FROM"))?;
    if let Some(join) = &select.join {
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

fn eq_lookup_value(selection: Option<&ExprNode>) -> Option<sealdb_types::Datum> {
    let ExprNode::Binary(bin) = selection? else {
        return None;
    };
    if !matches!(bin.op, sealdb_parser::opcode::Op::Eq) {
        return None;
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(_), ExprNode::Value(v)) | (ExprNode::Value(v), ExprNode::Column(_)) => {
            Some(v.datum.clone())
        }
        _ => None,
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

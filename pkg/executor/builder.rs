use crate::ddl::DDLExec;
use crate::insert::InsertExec;
use crate::internal::exec::{Executor, QueryResult};
use crate::point_get::PointGetExec;
use crate::projection::ProjectionExec;
use crate::select::{LimitExec, SelectionExec, TableDualExec};
use crate::show::ShowExec;
use crate::sortexec::SortExec;
use crate::table_reader::TableReaderExec;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_parser::ast::Statement;
use sealdb_planner::PhysicalPlan;
use std::sync::Arc;

pub struct ExecutorBuilder;

impl ExecutorBuilder {
    pub fn build(
        plan: &PhysicalPlan,
        stmt: &Statement,
        domain: Arc<Domain>,
        current_db: &str,
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
                let names = select
                    .fields
                    .iter()
                    .map(|f| f.as_name.clone().unwrap_or_default())
                    .collect();
                Ok(Box::new(ProjectionExec::new(
                    exprs,
                    names,
                    Self::build(child, stmt, domain, current_db)?,
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
                    Self::build(child, stmt, domain, current_db)?,
                )))
            }
            PhysicalPlan::Limit { child } => {
                let select = stmt
                    .as_select()
                    .ok_or_else(|| Error::unsupported("limit without select"))?;
                Ok(Box::new(LimitExec::new(
                    select.limit,
                    select.offset.unwrap_or(0),
                    Self::build(child, stmt, domain, current_db)?,
                )))
            }
            PhysicalPlan::Selection { child } => {
                let filter = stmt
                    .as_select()
                    .and_then(|s| s.selection.clone())
                    .ok_or_else(|| Error::unsupported("selection without WHERE"))?;
                Ok(Box::new(SelectionExec::new(
                    filter,
                    Self::build(child, stmt, domain, current_db)?,
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
                )))
            }
            PhysicalPlan::TableReader { .. } => {
                let from = stmt
                    .as_select()
                    .and_then(|s| s.from.clone())
                    .ok_or_else(|| Error::unsupported("table reader without FROM"))?;
                Ok(Box::new(TableReaderExec::new(
                    domain,
                    from,
                    current_db.to_string(),
                )))
            }
            PhysicalPlan::TableDual { .. } => Ok(Box::new(TableDualExec::new())),
        }
    }

    pub fn build_select(
        plan: &PhysicalPlan,
        select: &sealdb_parser::ast::SelectStmt,
    ) -> Result<Box<dyn Executor>> {
        Self::build(
            plan,
            &Statement::Select(select.clone()),
            Arc::new(Domain::new()),
            "",
        )
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

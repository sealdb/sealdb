use crate::internal::exec::{Executor, QueryResult};
use crate::projection::ProjectionExec;
use crate::select::TableDualExec;
use sealdb_errno::Result;
use sealdb_parser::ast::SelectStmt;
use sealdb_planner::PhysicalPlan;

pub struct ExecutorBuilder;

impl ExecutorBuilder {
    pub fn build_select(plan: &PhysicalPlan, select: &SelectStmt) -> Result<Box<dyn Executor>> {
        match plan {
            PhysicalPlan::Projection { .. } => {
                let exprs = select.fields.iter().map(|f| f.expr.clone()).collect();
                let names = select
                    .fields
                    .iter()
                    .map(|f| f.as_name.clone().unwrap_or_default())
                    .collect();
                Ok(Box::new(ProjectionExec::new(
                    exprs,
                    names,
                    Box::new(TableDualExec::new()),
                )))
            }
            PhysicalPlan::TableDual { .. } => Ok(Box::new(TableDualExec::new())),
        }
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

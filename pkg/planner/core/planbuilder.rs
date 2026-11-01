use super::physical_plans::PhysicalPlan;
use sealdb_errno::{Error, Result};
use sealdb_parser::ast::{Statement, StmtNode};

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        match stmt {
            Statement::Select(select) => {
                if let Some(from) = &select.from
                    && !from.eq_ignore_ascii_case("dual")
                {
                    return Err(Error::unsupported(format!("FROM {from}")));
                }
                let exprs = select
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("col{i}"))
                    .collect();
                Ok(PhysicalPlan::Projection {
                    exprs,
                    child: Box::new(PhysicalPlan::TableDual { row_count: 1 }),
                })
            }
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

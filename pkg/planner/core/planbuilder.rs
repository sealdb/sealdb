use super::physical_plans::{DdlKind, PhysicalPlan};
use sealdb_errno::Result;
use sealdb_parser::ast::{Statement, StmtNode};

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        match stmt {
            Statement::Select(select) => {
                let mut plan = if let Some(from) = &select.from
                    && !from.name.eq_ignore_ascii_case("dual")
                {
                    PhysicalPlan::TableReader {
                        table: from.name.clone(),
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
                if select.selection.is_some() {
                    if matches!(plan, PhysicalPlan::Projection { .. }) {
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
            Statement::Show(show) => Ok(PhysicalPlan::Show {
                kind: format!("{:?}", show.kind),
            }),
            Statement::Use(_) => Ok(PhysicalPlan::Show {
                kind: "Use".to_string(),
            }),
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

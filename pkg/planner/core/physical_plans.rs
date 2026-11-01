use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub enum PhysicalPlan {
    TableDual {
        row_count: usize,
    },
    Projection {
        exprs: Vec<String>,
        child: Box<PhysicalPlan>,
    },
}

impl PhysicalPlan {
    pub fn explain(&self) -> String {
        match self {
            PhysicalPlan::TableDual { row_count } => format!("TableDual_{row_count}"),
            PhysicalPlan::Projection { exprs, child } => {
                format!("Projection({}) -> {}", exprs.join(","), child.explain())
            }
        }
    }
}

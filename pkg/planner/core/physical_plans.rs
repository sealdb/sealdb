use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub enum DdlKind {
    CreateDatabase,
    DropDatabase,
    CreateTable,
    DropTable,
}

#[derive(Clone, Debug, Serialize)]
pub enum PhysicalPlan {
    TableDual {
        row_count: usize,
    },
    Projection {
        exprs: Vec<String>,
        child: Box<PhysicalPlan>,
    },
    DDL {
        kind: DdlKind,
    },
    Show {
        kind: String,
    },
    Insert,
    TableReader {
        table: String,
    },
    Selection {
        child: Box<PhysicalPlan>,
    },
    Sort {
        child: Box<PhysicalPlan>,
    },
    Limit {
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
            PhysicalPlan::DDL { kind } => format!("DDL_{kind:?}"),
            PhysicalPlan::Show { kind } => format!("Show_{kind}"),
            PhysicalPlan::Insert => "Insert".to_string(),
            PhysicalPlan::TableReader { table } => format!("TableReader_{table}"),
            PhysicalPlan::Selection { child } => format!("Selection -> {}", child.explain()),
            PhysicalPlan::Sort { child } => format!("Sort -> {}", child.explain()),
            PhysicalPlan::Limit { child } => format!("Limit -> {}", child.explain()),
        }
    }
}

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub enum DdlKind {
    CreateDatabase,
    DropDatabase,
    CreateTable,
    DropTable,
    CreateIndex,
    DropIndex,
    AlterTable,
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
    Update,
    Delete,
    TableReader {
        table: String,
        partitions: Vec<i64>,
    },
    PointGet {
        table: String,
        handle: i64,
    },
    IndexLookUpReader {
        table: String,
        index: String,
        partitions: Vec<i64>,
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
    HashAgg {
        child: Box<PhysicalPlan>,
    },
    HashJoin {
        tp: JoinType,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },
    Apply {
        child: Box<PhysicalPlan>,
    },
    Union {
        distinct: bool,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },
    Window {
        child: Box<PhysicalPlan>,
    },
    CTE {
        name: String,
        columns: Vec<String>,
        seed: Box<PhysicalPlan>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
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
            PhysicalPlan::Update => "Update".to_string(),
            PhysicalPlan::Delete => "Delete".to_string(),
            PhysicalPlan::TableReader { table, partitions } => {
                if partitions.is_empty() {
                    format!("TableReader_{table}")
                } else {
                    format!(
                        "TableReader_{table}_{}",
                        partitions
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
            }
            PhysicalPlan::PointGet { table, handle } => format!("PointGet_{table}_{handle}"),
            PhysicalPlan::IndexLookUpReader {
                table,
                index,
                partitions,
            } => {
                if partitions.is_empty() {
                    format!("IndexLookUpReader_{table}_{index}")
                } else {
                    format!(
                        "IndexLookUpReader_{table}_{index}_{}",
                        partitions
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
            }
            PhysicalPlan::Selection { child } => format!("Selection -> {}", child.explain()),
            PhysicalPlan::Sort { child } => format!("Sort -> {}", child.explain()),
            PhysicalPlan::Limit { child } => format!("Limit -> {}", child.explain()),
            PhysicalPlan::HashAgg { child } => format!("HashAgg -> {}", child.explain()),
            PhysicalPlan::HashJoin { tp, left, right } => {
                format!("HashJoin_{tp:?}({}, {})", left.explain(), right.explain())
            }
            PhysicalPlan::Apply { child } => format!("Apply -> {}", child.explain()),
            PhysicalPlan::Union {
                distinct,
                left,
                right,
            } => format!(
                "Union_{}({}, {})",
                if *distinct { "Distinct" } else { "All" },
                left.explain(),
                right.explain()
            ),
            PhysicalPlan::Window { child } => format!("Window -> {}", child.explain()),
            PhysicalPlan::CTE { name, seed, .. } => format!("CTE_{name} -> {}", seed.explain()),
        }
    }
}

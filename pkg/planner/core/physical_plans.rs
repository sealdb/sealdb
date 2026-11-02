use sealdb_parser::ast::ExplainFormat;
use sealdb_parser::ast::ExprNode;
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
    Simple {
        kind: String,
    },
    Insert,
    InfoSchema {
        table: String,
    },
    Update,
    Delete,
    TableReader {
        table: String,
        partitions: Vec<i64>,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    PointGet {
        table: String,
        handle: Option<i64>,
        index: Option<String>,
    },
    BatchPointGet {
        table: String,
        column: String,
        index: Option<String>,
        n_keys: usize,
    },
    IndexLookUpReader {
        table: String,
        index: String,
        partitions: Vec<i64>,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    IndexReader {
        table: String,
        index: String,
        partitions: Vec<i64>,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    Selection {
        child: Box<PhysicalPlan>,
        #[serde(skip)]
        filter: ExprNode,
    },
    Sort {
        child: Box<PhysicalPlan>,
    },
    TopN {
        child: Box<PhysicalPlan>,
    },
    Limit {
        child: Box<PhysicalPlan>,
    },
    HashAgg {
        child: Box<PhysicalPlan>,
    },
    StreamAgg {
        child: Box<PhysicalPlan>,
    },
    HashJoin {
        tp: JoinType,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        left_table: String,
        right_table: String,
    },
    Explain {
        format: ExplainFormat,
        child: Box<PhysicalPlan>,
    },
    Apply {
        child: Box<PhysicalPlan>,
        #[serde(skip)]
        filter: ExprNode,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
    Cross,
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
            PhysicalPlan::Simple { kind } => format!("Simple_{kind}"),
            PhysicalPlan::Insert => "Insert".to_string(),
            PhysicalPlan::InfoSchema { table } => format!("InfoSchema_{table}"),
            PhysicalPlan::Update => "Update".to_string(),
            PhysicalPlan::Delete => "Delete".to_string(),
            PhysicalPlan::TableReader {
                table,
                partitions,
                pushed,
            } => {
                let mut name = if partitions.is_empty() {
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
                };
                if let Some(filter) = pushed {
                    name.push_str(&format!("[{}]", filter.restore_string()));
                }
                name
            }
            PhysicalPlan::PointGet {
                table,
                handle: Some(handle),
                index: None,
            } => format!("PointGet_{table}_{handle}"),
            PhysicalPlan::PointGet {
                table,
                index: Some(index),
                ..
            } => format!("PointGet_{table}_{index}"),
            PhysicalPlan::PointGet { table, .. } => format!("PointGet_{table}"),
            PhysicalPlan::BatchPointGet {
                table,
                index: Some(index),
                n_keys,
                ..
            } => format!("BatchPointGet_{table}_{index}_{n_keys}"),
            PhysicalPlan::BatchPointGet { table, n_keys, .. } => {
                format!("BatchPointGet_{table}_{n_keys}")
            }
            PhysicalPlan::IndexLookUpReader {
                table,
                index,
                partitions,
                pushed,
            } => index_plan_name(
                "IndexLookUpReader",
                table,
                index,
                partitions,
                pushed.as_ref(),
            ),
            PhysicalPlan::IndexReader {
                table,
                index,
                partitions,
                pushed,
            } => index_plan_name("IndexReader", table, index, partitions, pushed.as_ref()),
            PhysicalPlan::Selection { child, .. } => format!("Selection -> {}", child.explain()),
            PhysicalPlan::Sort { child } => format!("Sort -> {}", child.explain()),
            PhysicalPlan::TopN { child } => format!("TopN -> {}", child.explain()),
            PhysicalPlan::Limit { child } => format!("Limit -> {}", child.explain()),
            PhysicalPlan::HashAgg { child } => format!("HashAgg -> {}", child.explain()),
            PhysicalPlan::StreamAgg { child } => format!("StreamAgg -> {}", child.explain()),
            PhysicalPlan::HashJoin {
                tp, left, right, ..
            } => {
                format!("HashJoin_{tp:?}({}, {})", left.explain(), right.explain())
            }
            PhysicalPlan::Explain { format, child } => {
                format!("Explain_{format:?} -> {}", child.explain())
            }
            PhysicalPlan::Apply { child, .. } => format!("Apply -> {}", child.explain()),
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

    pub fn table_reader(table: impl Into<String>, partitions: Vec<i64>) -> Self {
        PhysicalPlan::TableReader {
            table: table.into(),
            partitions,
            pushed: None,
        }
    }
}

fn index_plan_name(
    kind: &str,
    table: &str,
    index: &str,
    partitions: &[i64],
    pushed: Option<&ExprNode>,
) -> String {
    let mut name = if partitions.is_empty() {
        format!("{kind}_{table}_{index}")
    } else {
        format!(
            "{kind}_{table}_{index}_{}",
            partitions
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    if let Some(filter) = pushed {
        name.push_str(&format!("[{}]", filter.restore_string()));
    }
    name
}

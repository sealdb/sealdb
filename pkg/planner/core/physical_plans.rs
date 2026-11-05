use sealdb_parser::ast::ExplainFormat;
use sealdb_parser::ast::ExprNode;
use sealdb_parser::ast::SetOprType;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub enum DdlKind {
    CreateDatabase,
    DropDatabase,
    AlterDatabase,
    CreateTable,
    CreateView,
    CreateSequence,
    DropSequence,
    AlterSequence,
    DropTable,
    TruncateTable,
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
        /// Static EXPLAIN names on TableFullScan (`partition:p2`).
        partition_names: Vec<String>,
        /// Dynamic EXPLAIN access on TableReader (`all` / `p1,p2`). Empty = omit.
        partition_access: String,
        /// Scan cardinality (pseudo 10000 or analyzed count).
        row_count: f64,
        /// When true, EXPLAIN shows `stats:pseudo` on the cop scan.
        stats_pseudo: bool,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    /// Static-mode multi-partition gather (TiDB `PartitionUnion`).
    PartitionUnion {
        children: Vec<PhysicalPlan>,
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
        index_columns: Vec<String>,
        partitions: Vec<i64>,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    IndexReader {
        table: String,
        index: String,
        /// Index column names for EXPLAIN `index:idx(b, c)`.
        index_columns: Vec<String>,
        partitions: Vec<i64>,
        /// Analyzed / pseudo cardinality for EXPLAIN estRows.
        row_count: f64,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    /// Union of partial index scans (OR of indexed equalities) then table lookup.
    /// Completeness wave IndexMerge MVB; intersection / cost skyline deferred.
    IndexMerge {
        table: String,
        indexes: Vec<String>,
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
        /// TiDB ExplainInfo (`Column#4` or `select.t.a`).
        by_info: String,
    },
    TopN {
        child: Box<PhysicalPlan>,
        /// TiDB ExplainInfo: `select.t.a, offset:0, count:5`.
        by_info: String,
        offset: u64,
        count: u64,
    },
    Limit {
        child: Box<PhysicalPlan>,
        offset: u64,
        count: u64,
    },
    HashAgg {
        child: Box<PhysicalPlan>,
        /// TiDB ExplainInfo (`group by:…, funcs:…`).
        info: String,
        /// Coprocessor partial HashAgg ExplainInfo under TableReader (`data:HashAgg`).
        /// Empty = no cop push.
        cop_info: String,
        est_rows: f64,
    },
    StreamAgg {
        child: Box<PhysicalPlan>,
        /// TiDB ExplainInfo (`group by:…, funcs:…`).
        info: String,
        /// Coprocessor partial StreamAgg ExplainInfo under IndexReader/TableReader.
        cop_info: String,
        est_rows: f64,
    },
    /// Left-deep hash join. `on` is skipped in explain JSON; `Cross` has no
    /// equality keys; `Semi`/`Anti` emit the left schema only.
    HashJoin {
        tp: JoinType,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        left_table: String,
        right_table: String,
        #[serde(skip)]
        on: ExprNode,
    },
    /// `STRAIGHT_JOIN` on ordered keys (TiDB PhysicalMergeJoin). Executed as
    /// HashJoin; EXPLAIN shows MergeJoin + keep order:true scans.
    MergeJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        left_table: String,
        right_table: String,
        /// EXPLAIN `left key:select.t1.id`.
        left_key: String,
        /// EXPLAIN `right key:select.t2.id`.
        right_key: String,
        #[serde(skip)]
        on: ExprNode,
    },
    Explain {
        format: ExplainFormat,
        #[serde(default)]
        analyze: bool,
        child: Box<PhysicalPlan>,
    },
    Apply {
        /// Build / outer child.
        child: Box<PhysicalPlan>,
        #[serde(skip)]
        filter: ExprNode,
        /// Probe side when dual-child Apply (TiDB correlated left outer semi).
        probe: Option<Box<PhysicalPlan>>,
        /// ExplainInfo override (CARTESIAN left outer semi join, …).
        info: Option<String>,
    },
    /// Index nested-loop join (TiDB PhysicalIndexJoin). Outer listed first (Build).
    IndexJoin {
        info: String,
        est_rows: f64,
        outer: Box<PhysicalPlan>,
        inner: Box<PhysicalPlan>,
    },
    Union {
        op: SetOprType,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },
    Window {
        /// TiDB Window ExplainInfo (`row_number()->Column#5 over(…)`).
        info: String,
        child: Box<PhysicalPlan>,
    },
    /// Parallel window/stream-agg shuffle (TiDB `PhysicalShuffle`).
    Shuffle {
        info: String,
        child: Box<PhysicalPlan>,
    },
    /// Receiver stub under Shuffle (TiDB `PhysicalShuffleReceiverStub`).
    ShuffleReceiver {
        child: Box<PhysicalPlan>,
    },
    CTE {
        name: String,
        columns: Vec<String>,
        seed: Box<PhysicalPlan>,
        /// `AS NOT MATERIALIZED` inlines filters into `seed` and omits the CTE
        /// operator from EXPLAIN; recursive CTEs stay materialized.
        inline: bool,
    },
    /// Dedup child rows by `keys` (TiDB SemiJoinRewriter inner aggregation).
    Distinct {
        keys: Vec<String>,
        child: Box<PhysicalPlan>,
    },
}

/// Physical join kind. `Cross` is a cartesian product with no equality keys.
/// `Semi` / `Anti` emit the left schema only (match / no-match).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
    Cross,
    Semi,
    Anti,
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
                ..
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
            PhysicalPlan::PartitionUnion { children } => {
                format!(
                    "PartitionUnion({})",
                    children
                        .iter()
                        .map(|c| c.explain())
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
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
                ..
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
                ..
            } => index_plan_name("IndexReader", table, index, partitions, pushed.as_ref()),
            PhysicalPlan::IndexMerge {
                table,
                indexes,
                partitions,
                pushed,
            } => {
                let mut name = if partitions.is_empty() {
                    format!("IndexMerge_{table}_{}", indexes.join("+"))
                } else {
                    format!(
                        "IndexMerge_{table}_{}_{}",
                        indexes.join("+"),
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
            PhysicalPlan::Selection { child, .. } => format!("Selection -> {}", child.explain()),
            PhysicalPlan::Sort { child, by_info } => {
                format!("Sort({by_info}) -> {}", child.explain())
            }
            PhysicalPlan::TopN {
                child,
                by_info,
                offset,
                count,
            } => format!("TopN({by_info},offset:{offset},count:{count}) -> {}", child.explain()),
            PhysicalPlan::Limit {
                child,
                offset,
                count,
            } => format!("Limit(offset:{offset},count:{count}) -> {}", child.explain()),
            PhysicalPlan::HashAgg { child, info, .. } => {
                format!("HashAgg({info}) -> {}", child.explain())
            }
            PhysicalPlan::StreamAgg {
                child, info, ..
            } => {
                format!("StreamAgg({info}) -> {}", child.explain())
            }
            PhysicalPlan::HashJoin {
                tp, left, right, ..
            } => {
                format!("HashJoin_{tp:?}({}, {})", left.explain(), right.explain())
            }
            PhysicalPlan::MergeJoin { left, right, .. } => {
                format!("MergeJoin({}, {})", left.explain(), right.explain())
            }
            PhysicalPlan::Explain {
                format,
                analyze,
                child,
            } => {
                let tag = if *analyze { "Analyze" } else { "" };
                format!("Explain{tag}_{format:?} -> {}", child.explain())
            }
            PhysicalPlan::Apply { child, probe, .. } => match probe {
                Some(p) => format!("Apply({}, {})", child.explain(), p.explain()),
                None => format!("Apply -> {}", child.explain()),
            },
            PhysicalPlan::IndexJoin {
                outer, inner, info, ..
            } => format!("IndexJoin({info}, {}, {})", outer.explain(), inner.explain()),
            PhysicalPlan::Union { op, left, right } => {
                format!(
                    "Union_{}({}, {})",
                    op.restore(),
                    left.explain(),
                    right.explain()
                )
            }
            PhysicalPlan::Window { info, child } => {
                format!("Window({info}) -> {}", child.explain())
            }
            PhysicalPlan::Shuffle { info, child } => {
                format!("Shuffle({info}) -> {}", child.explain())
            }
            PhysicalPlan::ShuffleReceiver { child } => {
                format!("ShuffleReceiver -> {}", child.explain())
            }
            PhysicalPlan::CTE {
                name, seed, inline, ..
            } => {
                if *inline {
                    seed.explain()
                } else {
                    format!("CTE_{name} -> {}", seed.explain())
                }
            }
            PhysicalPlan::Distinct { keys, child } => {
                format!("Distinct({}) -> {}", keys.join(","), child.explain())
            }
        }
    }

    pub fn table_reader(table: impl Into<String>, partitions: Vec<i64>) -> Self {
        PhysicalPlan::TableReader {
            table: table.into(),
            partitions,
            partition_names: Vec::new(),
            partition_access: String::new(),
            row_count: 10_000.0,
            stats_pseudo: true,
            pushed: None,
        }
    }

    pub fn table_reader_parts(
        table: impl Into<String>,
        partitions: Vec<i64>,
        partition_names: Vec<String>,
        partition_access: impl Into<String>,
        row_count: f64,
        stats_pseudo: bool,
    ) -> Self {
        PhysicalPlan::TableReader {
            table: table.into(),
            partitions,
            partition_names,
            partition_access: partition_access.into(),
            row_count,
            stats_pseudo,
            pushed: None,
        }
    }

    pub fn hash_agg(child: PhysicalPlan, info: impl Into<String>, est_rows: f64) -> Self {
        PhysicalPlan::HashAgg {
            child: Box::new(child),
            info: info.into(),
            cop_info: String::new(),
            est_rows,
        }
    }

    pub fn hash_agg_cop(
        child: PhysicalPlan,
        info: impl Into<String>,
        cop_info: impl Into<String>,
        est_rows: f64,
    ) -> Self {
        PhysicalPlan::HashAgg {
            child: Box::new(child),
            info: info.into(),
            cop_info: cop_info.into(),
            est_rows,
        }
    }

    pub fn stream_agg(
        child: PhysicalPlan,
        info: impl Into<String>,
        cop_info: impl Into<String>,
        est_rows: f64,
    ) -> Self {
        PhysicalPlan::StreamAgg {
            child: Box::new(child),
            info: info.into(),
            cop_info: cop_info.into(),
            est_rows,
        }
    }

    pub fn limit(child: PhysicalPlan, offset: u64, count: u64) -> Self {
        PhysicalPlan::Limit {
            child: Box::new(child),
            offset,
            count,
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

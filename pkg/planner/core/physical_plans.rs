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
pub struct IndexScanRange {
    /// TiDB `ranger.Range.String()`: `[-inf,3)`, `[1,1]`, `(3,+inf]`.
    pub text: String,
    /// `IndexRangeScan` estRows — rows kept by the range alone.
    pub rows: f64,
    /// Rows kept by the range plus the residual cop Selection.
    pub filter_rows: f64,
    /// Unanalyzed table: EXPLAIN prints `stats:pseudo` on the scan.
    pub pseudo: bool,
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
    /// `INSERT` / `REPLACE`: `child` is the source plan (`INSERT … SELECT`).
    Insert {
        child: Option<Box<PhysicalPlan>>,
    },
    InfoSchema {
        table: String,
    },
    /// `UPDATE`: `child` reads the rows to update.
    Update {
        child: Option<Box<PhysicalPlan>>,
    },
    /// `DELETE`: `child` reads the row handles to delete.
    Delete {
        child: Option<Box<PhysicalPlan>>,
    },
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
        /// Filtered cardinality from exact stats (TiDB partition-prune est).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter_est: Option<f64>,
        /// EXPLAIN-only handle range (`[lo,hi]`) when a clustered PK scan was
        /// widened from a range predicate; execution still filters via `pushed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range_scan: Option<String>,
        /// `ORDER BY` the handle: TiDB scans the table in handle order
        /// (`keep order:true`) and drops the Sort.
        #[serde(default)]
        keep_order: bool,
        /// `max(_tidb_rowid)`: TiDB pushes `Limit 1` into the cop task and scans
        /// the handle in descending order (`data:Limit`, `keep order:true, desc`).
        #[serde(default)]
        data_limit: bool,
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
        /// Columns of the unique index serving the point get (`index:uidx(c)`).
        #[serde(default)]
        index_columns: Vec<String>,
        /// `FROM t PARTITION (pN)` pins the lookup to those physical partitions.
        #[serde(default)]
        partition_names: Vec<String>,
        /// GLOBAL index: partitions constrain execution but stay out of EXPLAIN.
        #[serde(default)]
        global_index: bool,
        /// `FOR UPDATE` handled by the point get itself (`handle:1, lock`).
        #[serde(default)]
        lock: bool,
    },
    BatchPointGet {
        table: String,
        column: String,
        index: Option<String>,
        n_keys: usize,
        /// Deduped, ascending row handles for a handle batch point get
        /// (`handle:[1 2 3]`).
        #[serde(default)]
        handles: Vec<i64>,
        /// Columns of the unique index serving the batch point get.
        #[serde(default)]
        index_columns: Vec<String>,
        /// Row-IN column order (`(a, b) IN ((1, 2), …)`); empty for a plain
        /// single-column IN list.
        #[serde(default)]
        tuple_columns: Vec<String>,
        /// Pruned partitions (`table:t, partition:p0,p1`).
        #[serde(default)]
        partition_names: Vec<String>,
        /// GLOBAL index: partitions constrain execution but stay out of EXPLAIN.
        #[serde(default)]
        global_index: bool,
        /// `FOR UPDATE` handled by the batch point get (`desc:false, lock`).
        #[serde(default)]
        lock: bool,
    },
    /// Row lock above a scan (`SELECT … FOR UPDATE`).
    SelectLock {
        child: Box<PhysicalPlan>,
        /// `for update 0` / `for share 0`.
        info: String,
    },
    IndexLookUpReader {
        table: String,
        index: String,
        index_columns: Vec<String>,
        partitions: Vec<i64>,
        /// TiDB `partition:all` / `partition:p0` label for a GLOBAL index read.
        #[serde(default)]
        partition_access: String,
        /// Analyzed / pseudo cardinality for EXPLAIN estRows.
        row_count: f64,
        /// When set, EXPLAIN Selection(Build) wraps IndexRangeScan.
        selection_info: Option<String>,
        /// When set, EXPLAIN Selection(Probe) wraps TableRowIDScan.
        probe_selection_info: Option<String>,
        /// Rows kept by the probe-side Selection (EXPLAIN estRows).
        #[serde(default)]
        probe_est: f64,
        #[serde(skip)]
        pushed: Option<ExprNode>,
    },
    IndexReader {
        table: String,
        index: String,
        /// Index column names for EXPLAIN `index:idx(b, c)`.
        index_columns: Vec<String>,
        partitions: Vec<i64>,
        /// TiDB `partition:all` / `partition:p0` label for a GLOBAL index read.
        #[serde(default)]
        partition_access: String,
        /// Analyzed / pseudo cardinality for EXPLAIN estRows.
        row_count: f64,
        #[serde(skip)]
        pushed: Option<ExprNode>,
        /// Access range from predicates on the index prefix (`None` = full
        /// index scan). Drives the `IndexRangeScan range:…` EXPLAIN shape and
        /// its estRows.
        #[serde(default)]
        range_scan: Option<IndexScanRange>,
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
        /// TiDB keeps the SQL join kind here (`left outer join …`).
        tp: JoinType,
        /// Full EXPLAIN info when `tp` / residual conds decorate it.
        #[serde(skip)]
        info: Option<String>,
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
    /// Dirty-read gather over snapshot + uncommitted writes (TiDB PhysicalUnionScan).
    UnionScan {
        /// ExplainInfo (filters); empty for Build-side bare UnionScan.
        info: String,
        est_rows: f64,
        child: Box<PhysicalPlan>,
    },
    /// TiDB `PhysicalUnionAll`: n-ary children. `op` keeps the set-operator
    /// semantics for the executor; EXPLAIN always prints empty info.
    Union {
        op: SetOprType,
        children: Vec<Box<PhysicalPlan>>,
    },
    /// TiDB `HashAgg` that eliminates duplicates for a distinct UNION group
    /// (`buildDistinct` over `LogicalUnionAll`). EXPLAIN prints `HashAgg` →
    /// child `Union`.
    DistinctUnion {
        /// `group by:…, funcs:firstrow(…)`.
        info: String,
        est_rows: f64,
        child: Box<PhysicalPlan>,
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
        /// FROM alias when different from `name` (`cte1 AS dt2` → alias dt2).
        alias: Option<String>,
        columns: Vec<String>,
        seed: Box<PhysicalPlan>,
        /// Recursive UNION arm plan for EXPLAIN (Seed Part / Recursive Part).
        recursive_plan: Option<Box<PhysicalPlan>>,
        /// `AS NOT MATERIALIZED` inlines filters into `seed` and omits the CTE
        /// operator from EXPLAIN; recursive CTEs stay materialized.
        inline: bool,
        /// WITH RECURSIVE — EXPLAIN shows CTEFullScan + separate producer tree.
        recursive: bool,
        /// Outer LIMIT/OFFSET on a recursive CTE (`… UNION ALL … LIMIT n`).
        limit: Option<u64>,
        offset: Option<u64>,
    },
    /// Dedup child rows by `keys` (TiDB SemiJoinRewriter inner aggregation).
    /// `fields`/`names` are the `SELECT DISTINCT` output projection; empty for
    /// the semi-join rewriter, which emits full child rows.
    Distinct {
        keys: Vec<String>,
        #[serde(skip)]
        fields: Vec<ExprNode>,
        #[serde(skip)]
        names: Vec<String>,
        child: Box<PhysicalPlan>,
    },
    /// Null-aware anti join (TiDB `PhysicalHashJoin` with `NullAwareAntiJoin`):
    /// the row-constructor rewrite of `(a, b) NOT IN (SELECT a, b …)` /
    /// `(a, b) != ALL (…)`. `left_outer` emits `probe row + bool` (anti left
    /// outer semi), otherwise only the matching probe rows (anti semi).
    NullAwareAntiJoin {
        /// TiDB ExplainInfo (`Null-aware anti semi join, equal:[…]`).
        info: String,
        est_rows: f64,
        /// Emit `probe row + bool` instead of probe rows only.
        left_outer: bool,
        /// Emit only the boolean column (`SELECT (a,b) IN (…)` form).
        bool_only: bool,
        /// Null-aware row-constructor semantics; `false` for the correlated
        /// equality rewrite, which behaves like a plain anti join.
        null_aware: bool,
        /// Name of the appended boolean column (`Column#N`).
        bool_column: String,
        /// (probe expr, build expr) equality key pairs.
        #[serde(skip)]
        keys: Vec<(ExprNode, ExprNode)>,
        /// ON condition over `[probe cols…, build cols…]`.
        #[serde(skip)]
        other_cond: Option<ExprNode>,
        /// Outer / inner table names, for qualified column resolution.
        probe_table: String,
        build_table: String,
        /// Emit only the first N probe columns (`SELECT *` drops the injected
        /// row-operand columns TiDB keeps internal to the operator).
        probe_width: Option<usize>,
        probe: Box<PhysicalPlan>,
        build: Box<PhysicalPlan>,
    },
    /// Plan-level Projection that computes row-constructor operands under a
    /// null-aware anti join (TiDB `injectExtraProjection`). `info` is the
    /// EXPLAIN rendering; `exprs`/`names` feed the real projection executor so
    /// the injected `Column#N` operands exist at runtime.
    RowProjection {
        info: String,
        #[serde(skip)]
        exprs: Vec<ExprNode>,
        names: Vec<String>,
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
            PhysicalPlan::Insert { child } => format!("Insert{}", dml_child(child)),
            PhysicalPlan::InfoSchema { table } => format!("InfoSchema_{table}"),
            PhysicalPlan::Update { child } => format!("Update{}", dml_child(child)),
            PhysicalPlan::Delete { child } => format!("Delete{}", dml_child(child)),
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
                ..
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
            PhysicalPlan::SelectLock { child, .. } => {
                format!("SelectLock -> {}", child.explain())
            }
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
            PhysicalPlan::UnionScan { child, info, .. } => {
                format!("UnionScan({info}) -> {}", child.explain())
            }
            PhysicalPlan::Union { op, children } => {
                let parts: Vec<String> = children.iter().map(|c| c.explain()).collect();
                format!("Union_{}({})", op.restore(), parts.join(", "))
            }
            PhysicalPlan::DistinctUnion { child, .. } => {
                format!("DistinctUnion -> {}", child.explain())
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
                name,
                seed,
                inline,
                recursive,
                ..
            } => {
                if *inline {
                    seed.explain()
                } else if *recursive {
                    format!("CTEFullScan_{name}")
                } else {
                    format!("CTE_{name} -> {}", seed.explain())
                }
            }
            PhysicalPlan::Distinct {
                keys,
                fields: _,
                names: _,
                child,
            } => {
                format!("Distinct({}) -> {}", keys.join(","), child.explain())
            }
            PhysicalPlan::NullAwareAntiJoin { info, probe, build, .. } => {
                format!("NullAwareAntiJoin({info}, {}, {})", probe.explain(), build.explain())
            }
            PhysicalPlan::RowProjection { info, child, .. } => {
                format!("RowProjection({info}) -> {}", child.explain())
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
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
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
            data_limit: false,
            filter_est: None,
            range_scan: None,
            keep_order: false,
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

fn dml_child(child: &Option<Box<PhysicalPlan>>) -> String {
    match child {
        Some(child) => format!(" -> {}", child.explain()),
        None => String::new(),
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

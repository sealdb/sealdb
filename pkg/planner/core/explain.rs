use super::physical_plans::PhysicalPlan;
use sealdb_parser::ast::ExplainFormat;
use serde_json::json;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ExplainRow {
    pub id: String,
    pub est_rows: String,
    pub task: String,
    pub access: String,
    pub info: String,
}

pub fn explain_result(
    plan: &PhysicalPlan,
    format: ExplainFormat,
) -> (Vec<String>, Vec<Vec<String>>) {
    match format {
        ExplainFormat::Row => {
            let rows = explain_rows(plan);
            (
                vec![
                    "id".to_string(),
                    "estRows".to_string(),
                    "task".to_string(),
                    "access object".to_string(),
                    "operator info".to_string(),
                ],
                rows.into_iter()
                    .map(|r| vec![r.id, r.est_rows, r.task, r.access, r.info])
                    .collect(),
            )
        }
        ExplainFormat::Json => (vec!["json".to_string()], vec![vec![explain_json(plan)]]),
        ExplainFormat::Dot => (vec!["dot".to_string()], vec![vec![explain_dot(plan)]]),
    }
}

/// TiDB-shaped EXPLAIN ANALYZE columns. MVB attributes measured root
/// cardinality to every operator; per-operator RuntimeStats stay deferred.
pub fn explain_analyze_result(
    plan: &PhysicalPlan,
    format: ExplainFormat,
    act_rows: usize,
    elapsed: Duration,
) -> (Vec<String>, Vec<Vec<String>>) {
    let time_ms = elapsed.as_secs_f64() * 1000.0;
    let exec_info = format!("time:{time_ms:.2}ms, loops:1");
    match format {
        ExplainFormat::Row => {
            let rows = explain_rows(plan);
            (
                vec![
                    "id".to_string(),
                    "estRows".to_string(),
                    "actRows".to_string(),
                    "task".to_string(),
                    "access object".to_string(),
                    "execution info".to_string(),
                    "operator info".to_string(),
                ],
                rows.into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        vec![
                            r.id,
                            r.est_rows,
                            act_rows.to_string(),
                            r.task,
                            r.access,
                            if i == 0 {
                                exec_info.clone()
                            } else {
                                String::new()
                            },
                            r.info,
                        ]
                    })
                    .collect(),
            )
        }
        ExplainFormat::Json => {
            let payload = json!({
                "plan": plan,
                "explain": plan.explain(),
                "actRows": act_rows,
                "executionInfo": exec_info,
            });
            (
                vec!["json".to_string()],
                vec![vec![
                    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
                ]],
            )
        }
        ExplainFormat::Dot => {
            let mut out = String::from("digraph {\n");
            let mut nodes = Vec::new();
            let mut edges = Vec::new();
            collect_dot(plan, &mut 1, &mut nodes, &mut edges);
            for (i, node) in nodes.into_iter().enumerate() {
                if i == 0 {
                    // Annotate root with measured actRows / time.
                    let annotated = node.replacen(
                        "estRows=",
                        &format!("actRows={act_rows}\\ntime={time_ms:.2}ms\\nestRows="),
                        1,
                    );
                    out.push_str(&annotated);
                } else {
                    out.push_str(&node);
                }
                out.push('\n');
            }
            for edge in edges {
                out.push_str(&edge);
                out.push('\n');
            }
            out.push('}');
            (vec!["dot".to_string()], vec![vec![out]])
        }
    }
}

pub fn explain_rows(plan: &PhysicalPlan) -> Vec<ExplainRow> {
    let mut rows = Vec::new();
    walk(plan, 0, true, &mut 1, &mut rows);
    rows
}

pub fn explain_json(plan: &PhysicalPlan) -> String {
    serde_json::to_string(&json!({
        "plan": plan,
        "explain": plan.explain(),
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

pub fn explain_dot(plan: &PhysicalPlan) -> String {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    collect_dot(plan, &mut 1, &mut nodes, &mut edges);
    let mut out = String::from("digraph {\n");
    for node in nodes {
        out.push_str(&node);
        out.push('\n');
    }
    for edge in edges {
        out.push_str(&edge);
        out.push('\n');
    }
    out.push('}');
    out
}

fn walk(
    plan: &PhysicalPlan,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
) {
    if let PhysicalPlan::CTE {
        seed, inline: true, ..
    } = plan
    {
        return walk(seed, depth, last, next_id, rows);
    }
    let id = *next_id;
    *next_id += 1;
    let (name, access, info) = node_meta(plan);
    let prefix = tree_prefix(depth, last);
    rows.push(ExplainRow {
        id: format!("{prefix}{name}_{id}"),
        est_rows: format!("{:.2}", est_rows(plan)),
        task: task_of(plan).to_string(),
        access,
        info,
    });
    // DistSQL readers expand into TiDB-shaped cop children (TableFullScan /
    // IndexRangeScan / TableRowIDScan) so EXPLAIN shows task=cop estRows.
    if emit_cop_children(plan, depth + 1, next_id, rows) {
        return;
    }
    let children = children_of(plan);
    for (i, child) in children.iter().enumerate() {
        walk(child, depth + 1, i + 1 == children.len(), next_id, rows);
    }
}

/// Synthetic coprocessor subtree under TableReader / IndexReader / IndexLookUp.
/// Returns true when the plan was a DistSQL reader (children already emitted).
fn emit_cop_children(
    plan: &PhysicalPlan,
    depth: usize,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
) -> bool {
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            pushed,
        } => {
            let scan_access = table_access(table, partitions);
            let scan_est = table_scan_est_rows();
            if let Some(filter) = pushed.as_ref() {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: est_rows(plan),
                        task: "cop",
                        access: String::new(),
                        info: format!("pushed:{}", filter.restore_string()),
                    },
                );
                push_explain_row(
                    rows,
                    depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "TableFullScan",
                        est: scan_est,
                        task: "cop",
                        access: scan_access,
                        info: "keep order:false".to_string(),
                    },
                );
            } else {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TableFullScan",
                        est: scan_est,
                        task: "cop",
                        access: scan_access,
                        info: "keep order:false".to_string(),
                    },
                );
            }
            true
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            partitions,
            pushed,
        } => {
            let access = index_access(table, index, partitions);
            let (scan_est, info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: est_rows(plan),
                        task: "cop",
                        access: String::new(),
                        info: format!("pushed:{}", filter.restore_string()),
                    },
                );
                push_explain_row(
                    rows,
                    depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexRangeScan",
                        est: scan_est,
                        task: "cop",
                        access,
                        info,
                    },
                );
            } else {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexFullScan",
                        est: scan_est,
                        task: "cop",
                        access,
                        info,
                    },
                );
            }
            true
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            partitions,
            pushed,
        } => {
            let idx_access = index_access(table, index, partitions);
            let (scan_est, mut info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                info = format!("pushed:{}", filter.restore_string());
            }
            push_explain_row(
                rows,
                depth,
                false,
                next_id,
                CopRow {
                    name: "IndexRangeScan",
                    est: scan_est,
                    task: "cop",
                    access: idx_access,
                    info,
                },
            );
            push_explain_row(
                rows,
                depth,
                true,
                next_id,
                CopRow {
                    name: "TableRowIDScan",
                    est: scan_est,
                    task: "cop",
                    access: table_access(table, partitions),
                    info: "keep order:false".to_string(),
                },
            );
            true
        }
        PhysicalPlan::IndexMerge {
            table,
            indexes,
            partitions,
            pushed,
        } => {
            let (scan_est, mut info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                info = format!("pushed:{}", filter.restore_string());
            }
            let n = indexes.len();
            for (i, index) in indexes.iter().enumerate() {
                let last_idx = i + 1 == n;
                // TableRowIDScan always follows as the final sibling.
                push_explain_row(
                    rows,
                    depth,
                    false,
                    next_id,
                    CopRow {
                        name: "IndexRangeScan",
                        est: scan_est,
                        task: "cop",
                        access: index_access(table, index, partitions),
                        info: if last_idx {
                            info.clone()
                        } else {
                            "keep order:false".to_string()
                        },
                    },
                );
            }
            push_explain_row(
                rows,
                depth,
                true,
                next_id,
                CopRow {
                    name: "TableRowIDScan",
                    est: scan_est,
                    task: "cop",
                    access: table_access(table, partitions),
                    info: "keep order:false".to_string(),
                },
            );
            true
        }
        _ => false,
    }
}

struct CopRow<'a> {
    name: &'a str,
    est: f64,
    task: &'a str,
    access: String,
    info: String,
}

fn push_explain_row(
    rows: &mut Vec<ExplainRow>,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    row: CopRow<'_>,
) {
    let id = *next_id;
    *next_id += 1;
    rows.push(ExplainRow {
        id: format!("{}{}_{id}", tree_prefix(depth, last), row.name),
        est_rows: format!("{:.2}", row.est),
        task: row.task.to_string(),
        access: row.access,
        info: row.info,
    });
}

fn table_access(table: &str, partitions: &[i64]) -> String {
    if partitions.is_empty() {
        format!("table:{table}")
    } else {
        format!("table:{table}, partition:{partitions:?}")
    }
}

fn index_access(table: &str, index: &str, partitions: &[i64]) -> String {
    if partitions.is_empty() {
        format!("table:{table}, index:{index}")
    } else {
        format!("table:{table}, index:{index}, partition:{partitions:?}")
    }
}

fn table_scan_est_rows() -> f64 {
    10_000.0
}

fn index_scan_meta(pushed: Option<&sealdb_parser::ast::ExprNode>) -> (f64, String) {
    if pushed.is_some() {
        (8_000.0, "keep order:false".to_string())
    } else {
        (10_000.0, "keep order:false".to_string())
    }
}

fn collect_dot(
    plan: &PhysicalPlan,
    next_id: &mut usize,
    nodes: &mut Vec<String>,
    edges: &mut Vec<String>,
) -> usize {
    if let PhysicalPlan::CTE {
        seed, inline: true, ..
    } = plan
    {
        return collect_dot(seed, next_id, nodes, edges);
    }
    let id = *next_id;
    *next_id += 1;
    let (name, access, info) = node_meta(plan);
    nodes.push(format!(
        "  N{id}[label=\"{name}_{id}\\nestRows={:.2}\\n{access} {info}\"]",
        est_rows(plan)
    ));
    if let Some(cop_ids) = collect_dot_cop_children(plan, next_id, nodes) {
        for child_id in cop_ids {
            edges.push(format!("  N{id} -> N{child_id}"));
        }
        return id;
    }
    for child in children_of(plan) {
        let child_id = collect_dot(child, next_id, nodes, edges);
        edges.push(format!("  N{id} -> N{child_id}"));
    }
    id
}

fn collect_dot_cop_children(
    plan: &PhysicalPlan,
    next_id: &mut usize,
    nodes: &mut Vec<String>,
) -> Option<Vec<usize>> {
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            pushed,
        } => {
            let scan_access = table_access(table, partitions);
            let scan_est = table_scan_est_rows();
            if let Some(filter) = pushed.as_ref() {
                Some(vec![
                    push_dot_node(
                        next_id,
                        nodes,
                        "Selection",
                        est_rows(plan),
                        "",
                        &format!("pushed:{}", filter.restore_string()),
                    ),
                    push_dot_node(
                        next_id,
                        nodes,
                        "TableFullScan",
                        scan_est,
                        &scan_access,
                        "keep order:false",
                    ),
                ])
            } else {
                Some(vec![push_dot_node(
                    next_id,
                    nodes,
                    "TableFullScan",
                    scan_est,
                    &scan_access,
                    "keep order:false",
                )])
            }
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            partitions,
            pushed,
        } => {
            let access = index_access(table, index, partitions);
            let (scan_est, info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                Some(vec![
                    push_dot_node(
                        next_id,
                        nodes,
                        "Selection",
                        est_rows(plan),
                        "",
                        &format!("pushed:{}", filter.restore_string()),
                    ),
                    push_dot_node(next_id, nodes, "IndexRangeScan", scan_est, &access, &info),
                ])
            } else {
                Some(vec![push_dot_node(
                    next_id,
                    nodes,
                    "IndexFullScan",
                    scan_est,
                    &access,
                    &info,
                )])
            }
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            partitions,
            pushed,
        } => {
            let idx_access = index_access(table, index, partitions);
            let (scan_est, mut info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                info = format!("pushed:{}", filter.restore_string());
            }
            let idx = push_dot_node(
                next_id,
                nodes,
                "IndexRangeScan",
                scan_est,
                &idx_access,
                &info,
            );
            let row = push_dot_node(
                next_id,
                nodes,
                "TableRowIDScan",
                scan_est,
                &table_access(table, partitions),
                "keep order:false",
            );
            Some(vec![idx, row])
        }
        PhysicalPlan::IndexMerge {
            table,
            indexes,
            partitions,
            pushed,
        } => {
            let (scan_est, mut info) = index_scan_meta(pushed.as_ref());
            if let Some(filter) = pushed.as_ref() {
                info = format!("pushed:{}", filter.restore_string());
            }
            let mut ids = Vec::with_capacity(indexes.len() + 1);
            for (i, index) in indexes.iter().enumerate() {
                let branch_info = if i + 1 == indexes.len() {
                    info.as_str()
                } else {
                    "keep order:false"
                };
                ids.push(push_dot_node(
                    next_id,
                    nodes,
                    "IndexRangeScan",
                    scan_est,
                    &index_access(table, index, partitions),
                    branch_info,
                ));
            }
            ids.push(push_dot_node(
                next_id,
                nodes,
                "TableRowIDScan",
                scan_est,
                &table_access(table, partitions),
                "keep order:false",
            ));
            Some(ids)
        }
        _ => None,
    }
}

fn push_dot_node(
    next_id: &mut usize,
    nodes: &mut Vec<String>,
    name: &str,
    est: f64,
    access: &str,
    info: &str,
) -> usize {
    let id = *next_id;
    *next_id += 1;
    nodes.push(format!(
        "  N{id}[label=\"{name}_{id}\\nestRows={est:.2}\\n{access} {info}\"]"
    ));
    id
}

fn children_of(plan: &PhysicalPlan) -> Vec<&PhysicalPlan> {
    match plan {
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child }
        | PhysicalPlan::TopN { child }
        | PhysicalPlan::Limit { child }
        | PhysicalPlan::HashAgg { child }
        | PhysicalPlan::StreamAgg { child }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child }
        | PhysicalPlan::CTE { seed: child, .. }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => vec![child.as_ref()],
        PhysicalPlan::HashJoin { left, right, .. } | PhysicalPlan::Union { left, right, .. } => {
            vec![left.as_ref(), right.as_ref()]
        }
        _ => Vec::new(),
    }
}

fn node_meta(plan: &PhysicalPlan) -> (String, String, String) {
    match plan {
        PhysicalPlan::TableDual { row_count } => (
            "TableDual".to_string(),
            String::new(),
            format!("rows:{row_count}"),
        ),
        PhysicalPlan::Projection { .. } => ("Projection".to_string(), String::new(), String::new()),
        PhysicalPlan::DDL { kind } => ("DDL".to_string(), String::new(), format!("{kind:?}")),
        PhysicalPlan::Show { kind } => ("Show".to_string(), String::new(), kind.clone()),
        PhysicalPlan::Simple { kind } => ("Simple".to_string(), String::new(), kind.clone()),
        PhysicalPlan::Insert => ("Insert".to_string(), String::new(), String::new()),
        PhysicalPlan::InfoSchema { table } => (
            "MemTable".to_string(),
            format!("table:{table}"),
            String::new(),
        ),
        PhysicalPlan::Update => ("Update".to_string(), String::new(), String::new()),
        PhysicalPlan::Delete => ("Delete".to_string(), String::new(), String::new()),
        PhysicalPlan::TableReader { pushed, .. } => (
            "TableReader".to_string(),
            String::new(),
            if pushed.is_some() {
                "data:Selection".to_string()
            } else {
                "data:TableFullScan".to_string()
            },
        ),
        PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
        } => (
            "PointGet".to_string(),
            format!("table:{table}"),
            format!("handle:{handle}"),
        ),
        PhysicalPlan::PointGet {
            table,
            index: Some(index),
            ..
        } => (
            "PointGet".to_string(),
            format!("table:{table}, index:{index}"),
            String::new(),
        ),
        PhysicalPlan::PointGet { table, .. } => (
            "PointGet".to_string(),
            format!("table:{table}"),
            String::new(),
        ),
        PhysicalPlan::BatchPointGet {
            table,
            index: Some(index),
            n_keys,
            ..
        } => (
            "BatchPointGet".to_string(),
            format!("table:{table}, index:{index}"),
            format!("keys:{n_keys}"),
        ),
        PhysicalPlan::BatchPointGet { table, n_keys, .. } => (
            "BatchPointGet".to_string(),
            format!("table:{table}"),
            format!("keys:{n_keys}"),
        ),
        PhysicalPlan::IndexLookUpReader { .. } => (
            "IndexLookUp".to_string(),
            String::new(),
            "data:IndexRangeScan, TableRowIDScan".to_string(),
        ),
        PhysicalPlan::IndexReader { pushed, .. } => (
            "IndexReader".to_string(),
            String::new(),
            if pushed.is_some() {
                "data:Selection".to_string()
            } else {
                "data:IndexFullScan".to_string()
            },
        ),
        PhysicalPlan::IndexMerge { indexes, .. } => (
            "IndexMerge".to_string(),
            String::new(),
            format!(
                "data:{}, TableRowIDScan",
                indexes
                    .iter()
                    .map(|_| "IndexRangeScan")
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        PhysicalPlan::Selection { filter, .. } => (
            "Selection".to_string(),
            String::new(),
            filter.restore_string(),
        ),
        PhysicalPlan::Sort { .. } => ("Sort".to_string(), String::new(), String::new()),
        PhysicalPlan::TopN { .. } => ("TopN".to_string(), String::new(), String::new()),
        PhysicalPlan::Limit { .. } => ("Limit".to_string(), String::new(), String::new()),
        PhysicalPlan::HashAgg { .. } => ("HashAgg".to_string(), String::new(), String::new()),
        PhysicalPlan::StreamAgg { .. } => ("StreamAgg".to_string(), String::new(), String::new()),
        PhysicalPlan::HashJoin {
            tp,
            left_table,
            right_table,
            ..
        } => {
            let info = if *tp == super::physical_plans::JoinType::Cross {
                format!("{tp:?}")
            } else {
                format!("{tp:?}, equal:[{left_table} = {right_table}]")
            };
            ("HashJoin".to_string(), String::new(), info)
        }
        PhysicalPlan::Apply { filter, .. } => {
            ("Apply".to_string(), String::new(), filter.restore_string())
        }
        PhysicalPlan::Union { op, .. } => {
            ("Union".to_string(), String::new(), op.restore().to_string())
        }
        PhysicalPlan::Window { .. } => ("Window".to_string(), String::new(), String::new()),
        PhysicalPlan::CTE { name, .. } => ("CTE".to_string(), format!("cte:{name}"), String::new()),
        PhysicalPlan::Distinct { keys, .. } => (
            "HashAgg".to_string(),
            String::new(),
            format!("distinct:{}", keys.join(",")),
        ),
        PhysicalPlan::Explain { format, .. } => {
            ("Explain".to_string(), String::new(), format!("{format:?}"))
        }
    }
}

fn est_rows(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableDual { row_count } => *row_count as f64,
        PhysicalPlan::PointGet { .. } => 1.0,
        PhysicalPlan::BatchPointGet { n_keys, .. } => *n_keys as f64,
        PhysicalPlan::Selection { child, .. } => est_rows(child) * 0.8,
        PhysicalPlan::TableReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexLookUpReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexMerge {
            pushed: Some(_), ..
        } => 8_000.0,
        PhysicalPlan::Limit { child } | PhysicalPlan::TopN { child } => est_rows(child).min(1.0),
        PhysicalPlan::HashJoin { left, right, .. } => est_rows(left).max(est_rows(right)),
        PhysicalPlan::Union { left, right, .. } => est_rows(left) + est_rows(right),
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child }
        | PhysicalPlan::HashAgg { child }
        | PhysicalPlan::StreamAgg { child }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child }
        | PhysicalPlan::CTE { seed: child, .. }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => est_rows(child),
        _ => 10_000.0,
    }
}

fn task_of(_plan: &PhysicalPlan) -> &'static str {
    // PhysicalPlan nodes are root gatherers; DistSQL scans are emitted as
    // synthetic task=cop rows in emit_cop_children.
    "root"
}

fn tree_prefix(depth: usize, last: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    for _ in 1..depth {
        prefix.push_str("│ ");
    }
    prefix.push_str(if last { "└─" } else { "├─" });
    prefix
}

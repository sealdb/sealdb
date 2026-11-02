use super::physical_plans::PhysicalPlan;
use sealdb_parser::ast::ExplainFormat;
use serde_json::json;

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
    let children = children_of(plan);
    for (i, child) in children.iter().enumerate() {
        walk(child, depth + 1, i + 1 == children.len(), next_id, rows);
    }
}

fn collect_dot(
    plan: &PhysicalPlan,
    next_id: &mut usize,
    nodes: &mut Vec<String>,
    edges: &mut Vec<String>,
) -> usize {
    let id = *next_id;
    *next_id += 1;
    let (name, access, info) = node_meta(plan);
    nodes.push(format!(
        "  N{id}[label=\"{name}_{id}\\nestRows={:.2}\\n{access} {info}\"]",
        est_rows(plan)
    ));
    for child in children_of(plan) {
        let child_id = collect_dot(child, next_id, nodes, edges);
        edges.push(format!("  N{id} -> N{child_id}"));
    }
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
        | PhysicalPlan::Explain { child, .. } => vec![child.as_ref()],
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
        PhysicalPlan::TableReader {
            table,
            partitions,
            pushed,
        } => (
            "TableReader".to_string(),
            format!("table:{table}"),
            cop_info(partitions, pushed.as_ref()),
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
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            pushed,
            ..
        } => (
            "IndexLookUp".to_string(),
            format!("table:{table}, index:{index}"),
            pushed
                .as_ref()
                .map(|filter| format!("pushed:{}", filter.restore_string()))
                .unwrap_or_default(),
        ),
        PhysicalPlan::IndexReader {
            table,
            index,
            pushed,
            ..
        } => (
            "IndexReader".to_string(),
            format!("table:{table}, index:{index}"),
            pushed
                .as_ref()
                .map(|filter| format!("pushed:{}", filter.restore_string()))
                .unwrap_or_default(),
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
        } => (
            "HashJoin".to_string(),
            String::new(),
            format!("{tp:?}, equal:[{left_table} = {right_table}]"),
        ),
        PhysicalPlan::Apply { filter, .. } => {
            ("Apply".to_string(), String::new(), filter.restore_string())
        }
        PhysicalPlan::Union { distinct, .. } => (
            "Union".to_string(),
            String::new(),
            if *distinct { "distinct" } else { "all" }.to_string(),
        ),
        PhysicalPlan::Window { .. } => ("Window".to_string(), String::new(), String::new()),
        PhysicalPlan::CTE { name, .. } => ("CTE".to_string(), format!("cte:{name}"), String::new()),
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
        | PhysicalPlan::Explain { child, .. } => est_rows(child),
        _ => 10_000.0,
    }
}

fn task_of(plan: &PhysicalPlan) -> &'static str {
    match plan {
        PhysicalPlan::TableReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexLookUpReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexReader {
            pushed: Some(_), ..
        } => "cop",
        _ => "root",
    }
}

fn cop_info(partitions: &[i64], pushed: Option<&sealdb_parser::ast::ExprNode>) -> String {
    let mut parts = Vec::new();
    if !partitions.is_empty() {
        parts.push(format!("partition:{partitions:?}"));
    }
    if let Some(filter) = pushed {
        parts.push(format!("pushed:{}", filter.restore_string()));
    }
    parts.join(", ")
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

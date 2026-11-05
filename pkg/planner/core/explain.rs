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
        ExplainFormat::Row | ExplainFormat::Brief => {
            let brief = matches!(format, ExplainFormat::Brief);
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
                    .map(|r| {
                        let id = if brief {
                            brief_operator_id(&r.id)
                        } else {
                            r.id
                        };
                        vec![id, r.est_rows, r.task, r.access, r.info]
                    })
                    .collect(),
            )
        }
        ExplainFormat::Json => (vec!["json".to_string()], vec![vec![explain_json(plan)]]),
        ExplainFormat::Dot => (vec!["dot".to_string()], vec![vec![explain_dot(plan)]]),
    }
}

/// TiDB `FORMAT=brief`: drop `_N` suffixes from operator ids (`TopN_3` → `TopN`).
fn brief_operator_id(id: &str) -> String {
    // Preserve tree prefix (`└─`, `│ └─`, spaces) then strip trailing `_digits`.
    let bytes = id.as_bytes();
    let mut name_start = 0;
    while name_start < bytes.len() {
        let c = bytes[name_start];
        if c.is_ascii_alphanumeric() || c == b'_' {
            // Start of operator name may include leading underscores from tree? No —
            // tree chars are spaces and box-drawing UTF-8.
            break;
        }
        name_start += 1;
    }
    // Walk back over multi-byte prefix chars carefully via chars.
    let mut prefix_end = 0;
    for (idx, ch) in id.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            prefix_end = idx;
            break;
        }
        prefix_end = idx + ch.len_utf8();
    }
    let (prefix, name) = id.split_at(prefix_end);
    // `TableReader_30(Build)` → `TableReader(Build)` for FORMAT=brief.
    let (stem, role) = match name.find('(') {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    let stem = stem
        .rsplit_once('_')
        .and_then(|(head, tail)| {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                Some(head)
            } else {
                None
            }
        })
        .unwrap_or(stem);
    format!("{prefix}{stem}{role}")
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
        ExplainFormat::Row | ExplainFormat::Brief => {
            let brief = matches!(format, ExplainFormat::Brief);
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
                        let id = if brief {
                            brief_operator_id(&r.id)
                        } else {
                            r.id
                        };
                        vec![
                            id,
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
    let mut deferred: Vec<PhysicalPlan> = Vec::new();
    walk(plan, 0, true, &mut 1, &mut rows, &mut deferred);
    // TiDB: recursive CTE producers follow the main plan as `CTE_0` siblings.
    let producers: Vec<PhysicalPlan> = deferred
        .into_iter()
        .filter(|p| {
            matches!(
                p,
                PhysicalPlan::CTE {
                    recursive: true,
                    ..
                }
            )
        })
        .collect();
    for cte in &producers {
        emit_recursive_cte_producer(cte, &mut 1, &mut rows);
    }
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
    deferred: &mut Vec<PhysicalPlan>,
) {
    walk_with_parent(
        plan,
        None,
        depth,
        last,
        next_id,
        rows,
        false,
        &[],
        deferred,
    );
}

fn walk_with_parent(
    plan: &PhysicalPlan,
    parent: Option<&PhysicalPlan>,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
    under_order_projection: bool,
    spine: &[bool],
    deferred: &mut Vec<PhysicalPlan>,
) {
    if let PhysicalPlan::CTE {
        seed, inline: true, ..
    } = plan
    {
        return walk_with_parent(
            seed,
            parent,
            depth,
            last,
            next_id,
            rows,
            under_order_projection,
            spine,
            deferred,
        );
    }
    // Recursive CTE consumer: CTEFullScan in the main plan; producer deferred.
    if let PhysicalPlan::CTE {
        recursive: true, ..
    } = plan
    {
        if parent.is_some() {
            deferred.push(plan.clone());
            emit_cte_full_scan(plan, parent, depth, last, next_id, rows, spine);
            return;
        }
        // Bare recursive CTE root — emit producer only.
        emit_recursive_cte_producer(plan, next_id, rows);
        return;
    }
    // TiDB brief/row often elides a trivial Projection above TopN/Limit.
    // Keep remapping projections (`Column#5->Column#6`) used after StreamAgg+ORDER BY.
    // Also elide CTE seed column-name projections (`SELECT c1`) so MergeJoin
    // Build/Probe labels land on the TableReader.
    if let PhysicalPlan::Projection { exprs, child, .. } = plan {
        let trivial = exprs.iter().all(|e| {
            e.starts_with("col") && !e.contains("->") && !e.contains("Column#")
        });
        let cte_seed_cols = exprs
            .iter()
            .all(|e| !e.contains("->") && !e.contains('(') && !e.contains(',') && !e.is_empty())
            && matches!(
                parent,
                Some(PhysicalPlan::MergeJoin { .. } | PhysicalPlan::Sort { .. })
            );
        if trivial || cte_seed_cols {
            return walk_with_parent(
                child,
                parent,
                depth,
                last,
                next_id,
                rows,
                under_order_projection,
                spine,
                deferred,
            );
        }
    }
    // Synthetic TiDB Projection above StreamAgg that retained firstrow for ORDER BY.
    if !under_order_projection
        && stream_agg_has_order_firstrow(plan)
        && !matches!(parent, Some(PhysicalPlan::Projection { .. }))
    {
        let id = *next_id;
        *next_id += 1;
        let prefix = tree_prefix_with_spine(spine, depth, last);
        rows.push(ExplainRow {
            id: format!("{prefix}Projection_{id}"),
            est_rows: format!("{:.2}", est_rows(plan)),
            task: "root".to_string(),
            access: String::new(),
            info: "Column#5->Column#6".to_string(),
        });
        return walk_with_parent(
            plan,
            parent,
            depth + 1,
            true,
            next_id,
            rows,
            true,
            spine,
            deferred,
        );
    }
    let id = *next_id;
    *next_id += 1;
    let (name, access, mut info) = node_meta(plan);
    // TiDB labels `TableReader_30(Build)` — role suffix after the numeric id.
    // Inline CTEs unwrap before walk; compare against the resolved seed pointer.
    let join_role = match parent {
        // HashJoin / MergeJoin: children_of lists right(Build) then left(Probe).
        Some(PhysicalPlan::HashJoin { left, right, .. })
        | Some(PhysicalPlan::MergeJoin { left, right, .. }) => {
            let left_p = explain_plan_ptr(left);
            let right_p = explain_plan_ptr(right);
            let self_p = explain_plan_ptr(plan);
            if self_p == right_p
                && matches!(
                    name.as_str(),
                    "HashJoin"
                        | "TableReader"
                        | "StreamAgg"
                        | "Projection"
                        | "Selection"
                        | "IndexReader"
                        | "IndexLookUp"
                        | "Limit"
                        | "UnionScan"
                        | "IndexJoin"
                        | "Sort"
                        | "CTE"
                )
            {
                "(Build)"
            } else if self_p == left_p
                && matches!(
                    name.as_str(),
                    "HashJoin"
                        | "TableReader"
                        | "Projection"
                        | "Selection"
                        | "IndexReader"
                        | "IndexLookUp"
                        | "Limit"
                        | "UnionScan"
                        | "IndexJoin"
                        | "Sort"
                        | "CTE"
                )
            {
                "(Probe)"
            } else {
                ""
            }
        }
        Some(PhysicalPlan::Apply {
            child, probe: Some(probe), ..
        })
        | Some(PhysicalPlan::IndexJoin {
            outer: child,
            inner: probe,
            ..
        }) => {
            let build_p = explain_plan_ptr(child);
            let probe_p = explain_plan_ptr(probe);
            let self_p = explain_plan_ptr(plan);
            if self_p == build_p
                && matches!(
                    name.as_str(),
                    "TableReader"
                        | "IndexReader"
                        | "IndexLookUp"
                        | "StreamAgg"
                        | "Selection"
                        | "HashJoin"
                        | "IndexJoin"
                        | "UnionScan"
                        | "Limit"
                )
            {
                "(Build)"
            } else if self_p == probe_p
                && matches!(
                    name.as_str(),
                    "TableReader"
                        | "IndexReader"
                        | "IndexLookUp"
                        | "StreamAgg"
                        | "Selection"
                        | "HashJoin"
                        | "IndexJoin"
                        | "UnionScan"
                        | "Limit"
                )
            {
                "(Probe)"
            } else {
                ""
            }
        }
        _ => "",
    };
    // TopN → IndexReader: `index:TopN` + cop TopN.
    // StreamAgg → TableReader: `data:StreamAgg` + cop StreamAgg.
    // StreamAgg → IndexReader: `index:StreamAgg` + cop StreamAgg keep order:true.
    let topn_parent = match parent {
        Some(PhysicalPlan::TopN {
            by_info,
            offset,
            count,
            ..
        }) => Some((by_info.as_str(), *offset, *count)),
        _ => None,
    };
    let stream_parent = match parent {
        Some(PhysicalPlan::StreamAgg {
            info,
            cop_info,
            est_rows,
            ..
        }) => Some(("StreamAgg", info.as_str(), cop_info.as_str(), *est_rows)),
        Some(PhysicalPlan::HashAgg {
            info,
            cop_info,
            est_rows,
            ..
        }) if !cop_info.is_empty() => {
            Some(("HashAgg", info.as_str(), cop_info.as_str(), *est_rows))
        }
        _ => None,
    };
    let window_keep_order = match parent {
        Some(PhysicalPlan::Window { info, .. }) => info.contains("partition by"),
        _ => false,
    };
    let under_window = matches!(parent, Some(PhysicalPlan::Window { .. }));
    if topn_parent.is_some() && matches!(plan, PhysicalPlan::IndexReader { .. }) {
        info = "index:TopN".to_string();
    }
    if let Some((agg_name, _, cop_info, _)) = stream_parent {
        if matches!(plan, PhysicalPlan::TableReader { .. }) && !cop_info.is_empty() {
            info = format!("data:{agg_name}");
        }
        if matches!(plan, PhysicalPlan::IndexReader { .. }) && agg_name == "StreamAgg" {
            if cop_info.is_empty() {
                info = "index:IndexFullScan".to_string();
            } else {
                info = "index:StreamAgg".to_string();
            }
        }
    }
    // RANGE: marker → TableRangeScan / IndexRangeScan access (overrides Selection).
    if let Some(range) = range_marker(plan) {
        match plan {
            PhysicalPlan::TableReader { .. } => {
                info = "data:TableRangeScan".to_string();
                let _ = range;
            }
            PhysicalPlan::IndexReader { .. } => {
                info = "index:IndexRangeScan".to_string();
            }
            _ => {}
        }
    }
    if under_window && matches!(plan, PhysicalPlan::IndexReader { .. }) {
        info = "index:IndexFullScan".to_string();
    }
    let bushy = matches!(
        plan,
        PhysicalPlan::HashJoin { .. }
            | PhysicalPlan::MergeJoin { .. }
            | PhysicalPlan::IndexJoin { .. }
            | PhysicalPlan::Apply { probe: Some(_), .. }
            | PhysicalPlan::PartitionUnion { .. }
            | PhysicalPlan::Union { .. }
    ) || matches!(
        parent,
        Some(
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::Apply { probe: Some(_), .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
                | PhysicalPlan::UnionScan { .. }
                | PhysicalPlan::Limit { .. }
        )
    );
    let prefix = if bushy || !spine.is_empty() {
        tree_prefix_with_spine(spine, depth, last)
    } else {
        tree_prefix(depth, last)
    };
    // TopN count only overrides DistSQL reader est (pushed TopN), not Union/etc.
    let row_est = if let Some((_, _, count)) = topn_parent {
        if matches!(
            plan,
            PhysicalPlan::IndexReader { .. }
                | PhysicalPlan::TableReader { .. }
                | PhysicalPlan::IndexLookUpReader { .. }
        ) {
            count as f64
        } else {
            est_rows(plan)
        }
    } else if let Some((_, _, cop_info, est)) = stream_parent {
        // Partial agg est applies to DistSQL reader only when cop push is active.
        if !cop_info.is_empty()
            && matches!(
                plan,
                PhysicalPlan::TableReader { .. }
                    | PhysicalPlan::IndexReader { .. }
                    | PhysicalPlan::IndexLookUpReader { .. }
            )
        {
            est
        } else {
            est_rows(plan)
        }
    } else if let Some(PhysicalPlan::Limit { count, .. }) = parent {
        // TiDB Limit above HashJoin clamps join estRows to the limit count.
        if *count > 0 {
            est_rows(plan).min(*count as f64)
        } else {
            est_rows(plan)
        }
    } else {
        est_rows(plan)
    };
    rows.push(ExplainRow {
        id: format!("{prefix}{name}_{id}{join_role}"),
        est_rows: format!("{row_est:.2}"),
        task: task_of(plan).to_string(),
        access,
        info,
    });
    // DistSQL readers expand into TiDB-shaped cop children (TableFullScan /
    // IndexRangeScan / TableRowIDScan) so EXPLAIN shows task=cop estRows.
    let under_bushy = matches!(
        parent,
        Some(
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::Apply { probe: Some(_), .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
        )
    );
    // Projection/Selection/Sort/HashAgg/UnionScan/Limit→CARTESIAN: TableReader
    // sits under a unary with a non-empty spine; still need bushy cop prefixes.
    let under_bushy = under_bushy
        || (!spine.is_empty()
            && matches!(
                parent,
                Some(
                    PhysicalPlan::Projection { .. }
                        | PhysicalPlan::Selection { .. }
                        | PhysicalPlan::Sort { .. }
                        | PhysicalPlan::HashAgg { .. }
                        | PhysicalPlan::StreamAgg { .. }
                        | PhysicalPlan::UnionScan { .. }
                        | PhysicalPlan::Limit { .. }
                )
            ));
    let under_merge_join = matches!(parent, Some(PhysicalPlan::MergeJoin { .. }));
    let under_index_join = matches!(parent, Some(PhysicalPlan::IndexJoin { .. }));
    if emit_cop_children(
        plan,
        topn_parent,
        stream_parent,
        under_bushy,
        under_merge_join,
        under_index_join,
        under_window,
        window_keep_order,
        depth + 1,
        next_id,
        rows,
        spine,
        last,
    ) {
        return;
    }
    let children = children_of(plan);
    // Under `└─` (Projection→HashJoin, or Probe whose spine ends in space):
    // children get spaces at this column. Nested HashJoin builds keep a leftover
    // `true` for Probe `│`; TableReader Selection uses `bushy_cop_spine`.
    // Under `└─` (TopN→Union, Projection→HashJoin, …): children get spaces at
    // this column so we render `  ├─` not `│ ├─`.
    let under_last_join = matches!(
        plan,
        PhysicalPlan::HashJoin { .. }
            | PhysicalPlan::MergeJoin { .. }
            | PhysicalPlan::IndexJoin { .. }
            | PhysicalPlan::Apply { probe: Some(_), .. }
            | PhysicalPlan::PartitionUnion { .. }
            | PhysicalPlan::Union { .. }
    ) && last
        && (matches!(
            parent,
            Some(
                PhysicalPlan::Projection { .. }
                    | PhysicalPlan::TopN { .. }
                    | PhysicalPlan::StreamAgg { .. }
                    | PhysicalPlan::Apply { .. }
                    | PhysicalPlan::Limit { .. }
            )
        ) || spine.last() == Some(&false));
    for (i, child) in children.iter().enumerate() {
        let child_last = i + 1 == children.len();
        let mut child_spine = spine.to_vec();
        if under_last_join {
            child_spine.push(false);
            if !child_last
                && matches!(
                    child,
                    PhysicalPlan::HashJoin { .. }
                        | PhysicalPlan::MergeJoin { .. }
                        | PhysicalPlan::IndexJoin { .. }
                        | PhysicalPlan::Apply { probe: Some(_), .. }
                        | PhysicalPlan::PartitionUnion { .. }
                        | PhysicalPlan::Union { .. }
                )
            {
                child_spine.push(true);
            }
        } else if matches!(
            plan,
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::Apply { probe: Some(_), .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
        ) {
            child_spine.push(!child_last);
        } else if !spine.is_empty() {
            // StreamAgg under CARTESIAN: push whether *this* node continues.
            // Unary ops under HashJoin Build (Selection/HashAgg/Projection):
            // push whether *their* child has siblings → spaces under `│`.
            // Projection under Union: push `!last` so Dual keeps `│` when the
            // Projection is a non-last Union arm (`  ├─` / `  │ └─`).
            if matches!(
                plan,
                PhysicalPlan::Projection { .. }
                    | PhysicalPlan::Selection { .. }
                    | PhysicalPlan::Sort { .. }
                    | PhysicalPlan::HashAgg { .. }
                    | PhysicalPlan::StreamAgg { .. }
            ) && matches!(
                parent,
                Some(
                    PhysicalPlan::HashJoin { .. }
                        | PhysicalPlan::MergeJoin { .. }
                        | PhysicalPlan::IndexJoin { .. }
                        | PhysicalPlan::Apply { probe: Some(_), .. }
                )
            ) {
                // Unary under Build: spaces under the │ (no further siblings).
                child_spine.push(!child_last);
            } else {
                // Limit/UnionScan Build under HashJoin: keep │ for Probe sibling.
                child_spine.push(!last);
            }
        }
        walk_with_parent(
            child,
            Some(plan),
            depth + 1,
            child_last,
            next_id,
            rows,
            under_order_projection,
            &child_spine,
            deferred,
        );
    }
}

fn emit_cte_full_scan(
    plan: &PhysicalPlan,
    parent: Option<&PhysicalPlan>,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
    spine: &[bool],
) {
    let PhysicalPlan::CTE { name, alias, .. } = plan else {
        return;
    };
    let id = *next_id;
    *next_id += 1;
    let prefix = if spine.is_empty() {
        tree_prefix(depth, last)
    } else {
        tree_prefix_with_spine(spine, depth, last)
    };
    let join_role = match parent {
        Some(PhysicalPlan::Apply {
            probe: Some(probe), ..
        }) if explain_plan_ptr(probe) == explain_plan_ptr(plan) => "(Probe)",
        Some(PhysicalPlan::HashJoin { right, .. } | PhysicalPlan::MergeJoin { right, .. })
            if explain_plan_ptr(right) == explain_plan_ptr(plan) =>
        {
            "(Build)"
        }
        Some(PhysicalPlan::HashJoin { left, .. } | PhysicalPlan::MergeJoin { left, .. })
            if explain_plan_ptr(left) == explain_plan_ptr(plan) =>
        {
            "(Probe)"
        }
        _ => "",
    };
    let access = match alias {
        Some(a) => format!("CTE:{name} AS {a}"),
        None => format!("CTE:{name}"),
    };
    rows.push(ExplainRow {
        id: format!("{prefix}CTEFullScan_{id}{join_role}"),
        est_rows: format!("{:.2}", est_rows(plan)),
        task: "root".to_string(),
        access,
        info: "data:CTE_0".to_string(),
    });
}

fn emit_recursive_cte_producer(
    plan: &PhysicalPlan,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
) {
    let PhysicalPlan::CTE {
        name,
        seed,
        recursive_plan,
        recursive: true,
        ..
    } = plan
    else {
        return;
    };
    let id = *next_id;
    *next_id += 1;
    let est = est_rows(plan);
    rows.push(ExplainRow {
        id: format!("CTE_0_{id}"),
        est_rows: format!("{est:.2}"),
        task: "root".to_string(),
        access: String::new(),
        info: "Recursive CTE".to_string(),
    });
    let seed_part = match seed.as_ref() {
        PhysicalPlan::Union { left, .. } => left.as_ref(),
        other => other,
    };
    let rec_part = recursive_plan
        .as_ref()
        .map(|p| p.as_ref())
        .or_else(|| match seed.as_ref() {
            PhysicalPlan::Union { right, .. } => Some(right.as_ref()),
            _ => None,
        });
    let orig_tbl = seed_orig_table(seed_part).unwrap_or_else(|| name.clone());
    emit_cte_seed_part(seed_part, next_id, rows);
    if let Some(rec) = rec_part {
        emit_cte_recursive_part(rec, name, &orig_tbl, next_id, rows);
    }
}

fn seed_orig_table(plan: &PhysicalPlan) -> Option<String> {
    match plan {
        PhysicalPlan::TableReader { table, .. } => Some(table.clone()),
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. } => seed_orig_table(child),
        _ => None,
    }
}

fn emit_cte_seed_part(plan: &PhysicalPlan, next_id: &mut usize, rows: &mut Vec<ExplainRow>) {
    // Unwrap column Projection so Seed Part lands on TableReader.
    let plan = match plan {
        PhysicalPlan::Projection { child, exprs, .. }
            if exprs
                .iter()
                .all(|e| !e.contains("->") && !e.contains('(')) =>
        {
            child.as_ref()
        }
        other => other,
    };
    let id = *next_id;
    *next_id += 1;
    let (name, access, info) = node_meta(plan);
    rows.push(ExplainRow {
        id: format!("├─{name}_{id}(Seed Part)"),
        est_rows: format!("{:.2}", est_rows(plan)),
        task: "root".to_string(),
        access,
        info,
    });
    // Cop children under Seed Part: `│ └─TableFullScan` (│ continues for Recursive Part).
    if let PhysicalPlan::TableReader {
        table,
        partition_names,
        row_count,
        stats_pseudo,
        pushed,
        ..
    } = plan
    {
        let scan_id = *next_id;
        *next_id += 1;
        let keep = if *stats_pseudo {
            "keep order:false, stats:pseudo"
        } else {
            "keep order:false"
        };
        let _ = (pushed, row_count);
        rows.push(ExplainRow {
            id: format!("│ └─TableFullScan_{scan_id}"),
            est_rows: "10000.00".to_string(),
            task: "cop[tikv]".to_string(),
            access: table_access(table, partition_names),
            info: keep.to_string(),
        });
    }
}

fn emit_cte_recursive_part(
    plan: &PhysicalPlan,
    cte_name: &str,
    orig_tbl: &str,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
) {
    let schema = sealdb_expression::current_db_name();
    let schema = if schema.is_empty() {
        "select".to_string()
    } else {
        schema
    };
    let proj_info = recursive_arm_proj_info(plan, &schema, orig_tbl);
    let filter_info = recursive_arm_filter_info(plan, cte_name, orig_tbl);
    let id = *next_id;
    *next_id += 1;
    rows.push(ExplainRow {
        id: format!("└─Projection_{id}(Recursive Part)"),
        est_rows: "8000.00".to_string(),
        task: "root".to_string(),
        access: String::new(),
        info: proj_info,
    });
    if let Some(filt) = filter_info {
        let sel_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("  └─Selection_{sel_id}"),
            est_rows: "8000.00".to_string(),
            task: "root".to_string(),
            access: String::new(),
            info: filt,
        });
        let tbl_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("    └─CTETable_{tbl_id}"),
            est_rows: "10000.00".to_string(),
            task: "root".to_string(),
            access: String::new(),
            info: "Scan on CTE_0".to_string(),
        });
    } else {
        let tbl_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("  └─CTETable_{tbl_id}"),
            est_rows: "10000.00".to_string(),
            task: "root".to_string(),
            access: String::new(),
            info: "Scan on CTE_0".to_string(),
        });
    }
}

fn recursive_arm_proj_info(plan: &PhysicalPlan, schema: &str, orig_tbl: &str) -> String {
    let from_exprs = match plan {
        PhysicalPlan::Projection { exprs, .. } => Some(exprs.as_slice()),
        PhysicalPlan::Selection { child, .. } => match child.as_ref() {
            PhysicalPlan::Projection { exprs, .. } => Some(exprs.as_slice()),
            _ => None,
        },
        _ => None,
    };
    if let Some(exprs) = from_exprs {
        if let Some(e) = exprs.iter().find(|e| e.contains("plus") || e.contains("->")) {
            let mut s = e.clone();
            // plus(cte.t.c1, 1)->Column#6 → cast(plus(cte.t1.c1, 1), int)->cte.t1.c1
            s = s.replace(".t.", &format!(".{orig_tbl}."));
            s = s.replace("cte1.", &format!("{orig_tbl}."));
            if let Some((left, _)) = s.split_once("->") {
                let core = left.trim();
                let core = if core.starts_with("cast(") {
                    core.to_string()
                } else {
                    format!("cast({core}, int)")
                };
                return format!("{core}->{schema}.{orig_tbl}.c1");
            }
            return s;
        }
    }
    format!("cast(plus({schema}.{orig_tbl}.c1, 1), int)->{schema}.{orig_tbl}.c1")
}

fn recursive_arm_filter_info(
    plan: &PhysicalPlan,
    cte_name: &str,
    orig_tbl: &str,
) -> Option<String> {
    let filter = match plan {
        PhysicalPlan::Selection { filter, .. } => Some(filter),
        PhysicalPlan::Projection { child, .. } => match child.as_ref() {
            PhysicalPlan::Selection { filter, .. } => Some(filter),
            PhysicalPlan::TableReader {
                pushed: Some(filter),
                ..
            } => Some(filter),
            _ => None,
        },
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            ..
        } => Some(filter),
        _ => None,
    }?;
    Some(
        explain_selection_info(filter)
            .replace(&format!(".{cte_name}."), &format!(".{orig_tbl}."))
            .replace(".t.", &format!(".{orig_tbl}."))
            .replace(&format!("{cte_name}."), &format!("{orig_tbl}.")),
    )
}

/// Synthetic coprocessor subtree under TableReader / IndexReader / IndexLookUp.
/// Returns true when the plan was a DistSQL reader (children already emitted).
fn emit_cop_children(
    plan: &PhysicalPlan,
    topn_parent: Option<(&str, u64, u64)>,
    stream_parent: Option<(&str, &str, &str, f64)>,
    under_bushy: bool,
    under_merge_join: bool,
    under_index_join: bool,
    under_window: bool,
    window_keep_order: bool,
    depth: usize,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
    spine: &[bool],
    _parent_last: bool,
) -> bool {
    let under_stream_agg = stream_parent.is_some();
    match plan {
        PhysicalPlan::TableReader {
            table,
            partition_names,
            row_count,
            stats_pseudo,
            pushed,
            ..
        } => {
            let scan_access = table_access(table, partition_names);
            // Negative row_count encodes HAVING-push filtered est (|row_count|);
            // TableFullScan stays at the pseudo/analyzed scan cardinality.
            let scan_est = if *row_count < 0.0 {
                if *stats_pseudo {
                    10_000.0
                } else {
                    -*row_count
                }
            } else {
                *row_count
            };
            let mut scan_depth = depth;
            // TiDB always labels DistSQL scans `cop[tikv]` for TableFullScan.
            let scan_task = "cop[tikv]";
            // Cop children inherit the TableReader spine; non-last Build readers
            // need `│` at the reader column (see `bushy_cop_spine`).
            if let Some((agg_name, _, cop_info, est)) = stream_parent {
                if !cop_info.is_empty() {
                    // TiDB partial StreamAgg/HashAgg pushed into TableReader.
                    let agg_owned = bushy_cop_spine(spine, depth, under_bushy, _parent_last);
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: agg_name,
                            est,
                            task: "cop[tikv]",
                            access: String::new(),
                            info: cop_info.to_string(),
                        },
                        if under_bushy {
                            agg_owned.as_slice()
                        } else {
                            &[]
                        },
                    );
                    scan_depth = depth + 1;
                }
            }
            // RANGE: marker → TableRangeScan (no Selection row).
            if let Some(range_info) = range_pushed(pushed.as_ref()) {
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                let keep = if *stats_pseudo {
                    format!("range: {range_info}, keep order:false, stats:pseudo")
                } else {
                    format!("range: {range_info}, keep order:false")
                };
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TableRangeScan",
                        est: scan_est,
                        task: scan_task,
                        access: scan_access,
                        info: keep,
                    },
                    if under_bushy {
                        if scan_spine.is_empty() {
                            cop_spine
                        } else {
                            &scan_spine
                        }
                    } else {
                        &[]
                    },
                );
                return true;
            }
            // MergeJoin needs ordered PK input → keep order:true (TiDB).
            let scan_info = if under_merge_join {
                if *stats_pseudo {
                    "keep order:true, stats:pseudo".to_string()
                } else {
                    "keep order:true".to_string()
                }
            } else if *stats_pseudo {
                "keep order:false, stats:pseudo".to_string()
            } else {
                "keep order:false".to_string()
            };
            let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
            let cop_spine = cop_owned.as_slice();
            if let Some(filter) = pushed.as_ref() {
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: est_rows(plan),
                        task: scan_task,
                        access: String::new(),
                        info: explain_pushed_filter(table, filter),
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "TableFullScan",
                        est: scan_est,
                        task: scan_task,
                        access: scan_access,
                        info: scan_info,
                    },
                    if under_bushy { &scan_spine } else { &[] },
                );
            } else {
                let mut scan_spine = cop_spine.to_vec();
                // Pad so TableFullScan indents one more level than the reader.
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TableFullScan",
                        est: scan_est,
                        task: scan_task,
                        access: scan_access,
                        info: scan_info,
                    },
                    if under_bushy {
                        if scan_spine.is_empty() {
                            cop_spine
                        } else {
                            &scan_spine
                        }
                    } else {
                        &[]
                    },
                );
            }
            true
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            row_count,
            pushed,
        } => {
            let access = index_access_cols(table, index, index_columns, partitions);
            let (mut scan_est, mut scan_info) = index_scan_meta(pushed.as_ref());
            // Explicit IndexReader.row_count / RANGE overrides pseudo est.
            if range_pushed(pushed.as_ref()).is_some()
                || (*row_count - 10_000.0).abs() > f64::EPSILON
            {
                scan_est = *row_count;
            }
            let mut scan_depth = depth;
            let stream_est = stream_parent.map(|(_, _, _, e)| e);
            // TiDB pushes TopN into the IndexReader cop task.
            if let Some((by_info, offset, count)) = topn_parent {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TopN",
                        est: count as f64,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: format!("{by_info}, offset:{offset}, count:{count}"),
                    },
                );
                scan_depth = depth + 1;
            }
            if let Some((agg_name, _, cop_info, est)) = stream_parent {
                // GROUP BY StreamAgg needs ordered index input.
                scan_info = "keep order:true, stats:pseudo".to_string();
                if !cop_info.is_empty() {
                    push_explain_row(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: agg_name,
                            est,
                            task: "cop[tikv]",
                            access: String::new(),
                            info: cop_info.to_string(),
                        },
                    );
                    scan_depth = depth + 1;
                }
            }
            if under_window {
                scan_info = if window_keep_order {
                    "keep order:true, stats:pseudo".to_string()
                } else {
                    "keep order:false, stats:pseudo".to_string()
                };
            }
            // RANGE: marker → IndexRangeScan with decided-by info.
            if let Some(range_info) = range_pushed(pushed.as_ref()) {
                let task = if under_index_join || under_stream_agg {
                    "cop[tikv]"
                } else {
                    "cop"
                };
                let keep = if *row_count >= 9_000.0 {
                    format!("range: {range_info}, keep order:false, stats:pseudo")
                } else {
                    format!("range: {range_info}, keep order:false")
                };
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexRangeScan",
                        est: scan_est,
                        task,
                        access,
                        info: keep,
                    },
                    if under_bushy {
                        if scan_spine.is_empty() {
                            cop_spine
                        } else {
                            &scan_spine
                        }
                    } else {
                        &[]
                    },
                );
                return true;
            }
            let scan_task = if topn_parent.is_some()
                || under_stream_agg
                || under_window
                || under_index_join
            {
                "cop[tikv]"
            } else {
                "cop"
            };
            // Limit pushdown: scale Selection/scan est by limit/group_est.
            let limit_scale = stream_est
                .filter(|e| *e > 0.0 && *e < 8000.0)
                .map(|e| {
                    // When StreamAgg est is already 1.0 (LIMIT 1), scale from
                    // the un-limited group estimate (~2666.67).
                    if (e - 1.0).abs() < f64::EPSILON {
                        1.0 / (8000.0 / 3.0)
                    } else {
                        1.0
                    }
                })
                .unwrap_or(1.0);
            let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
            let cop_spine = cop_owned.as_slice();
            if let Some(filter) = pushed.as_ref() {
                let sel_est = if under_stream_agg {
                    (10_000.0 / 3.0) * limit_scale
                } else {
                    est_rows(plan)
                };
                let filter_info = explain_pushed_filter(table, filter);
                // Full scan under StreamAgg+Selection stays IndexFullScan (not Range).
                let leaf_name = if under_stream_agg {
                    "IndexFullScan"
                } else {
                    "IndexRangeScan"
                };
                if under_stream_agg {
                    scan_est = 10_000.0 * limit_scale;
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: sel_est,
                        task: scan_task,
                        access: String::new(),
                        info: filter_info,
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                let mut leaf_spine = cop_spine.to_vec();
                if under_bushy {
                    leaf_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: leaf_name,
                        est: scan_est,
                        task: scan_task,
                        access,
                        info: scan_info,
                    },
                    if under_bushy { &leaf_spine } else { &[] },
                );
            } else {
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexFullScan",
                        est: scan_est,
                        task: scan_task,
                        access,
                        info: scan_info,
                    },
                    if under_bushy {
                        if scan_spine.is_empty() {
                            cop_spine
                        } else {
                            &scan_spine
                        }
                    } else {
                        &[]
                    },
                );
            }
            true
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            row_count,
            selection_info,
            probe_selection_info,
            pushed,
        } => {
            let idx_access = index_access_cols(table, index, index_columns, partitions);
            let (mut scan_est, _) = index_scan_meta(pushed.as_ref());
            if range_pushed(pushed.as_ref()).is_some() || *row_count < 9_000.0 {
                scan_est = *row_count;
            }
            // IndexJoin/UnionScan IndexLookUp: RANGE + optional Selection sides.
            if let Some(range_info) = range_pushed(pushed.as_ref()) {
                let task = if under_index_join || under_bushy {
                    "cop[tikv]"
                } else {
                    "cop"
                };
                let mut range_info_full = format!("range: {range_info}, keep order:false");
                let mut rowid_info = "keep order:false".to_string();
                if *row_count >= 9_000.0 {
                    range_info_full.push_str(", stats:pseudo");
                    rowid_info.push_str(", stats:pseudo");
                }
                let cop_owned = bushy_cop_spine(spine, depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut build_spine = cop_spine.to_vec();
                if under_bushy {
                    build_spine.push(true);
                }
                if selection_info.is_some() {
                    push_explain_row_spine(
                        rows,
                        depth,
                        false,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: scan_est,
                            task,
                            access: String::new(),
                            info: selection_info.clone().unwrap_or_default(),
                        },
                        if under_bushy { &build_spine } else { &[] },
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Build)");
                    }
                    let mut child_spine = build_spine.clone();
                    if under_bushy {
                        child_spine.push(false);
                    }
                    push_explain_row_spine(
                        rows,
                        depth + 1,
                        true,
                        next_id,
                        CopRow {
                            name: "IndexRangeScan",
                            est: scan_est,
                            task,
                            access: idx_access.clone(),
                            info: range_info_full.clone(),
                        },
                        if under_bushy { &child_spine } else { &[] },
                    );
                } else {
                    push_explain_row_spine(
                        rows,
                        depth,
                        false,
                        next_id,
                        CopRow {
                            name: "IndexRangeScan",
                            est: scan_est,
                            task,
                            access: idx_access.clone(),
                            info: range_info_full.clone(),
                        },
                        if under_bushy { &build_spine } else { &[] },
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Build)");
                    }
                }
                if let Some(probe_sel) = probe_selection_info.as_ref() {
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: scan_est,
                            task,
                            access: String::new(),
                            info: probe_sel.clone(),
                        },
                        if under_bushy { cop_spine } else { &[] },
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Probe)");
                    }
                    let mut child_spine = cop_spine.to_vec();
                    if under_bushy {
                        child_spine.push(false);
                    }
                    push_explain_row_spine(
                        rows,
                        depth + 1,
                        true,
                        next_id,
                        CopRow {
                            name: "TableRowIDScan",
                            est: scan_est,
                            task,
                            access: table_access(table, &[]),
                            info: rowid_info,
                        },
                        if under_bushy { &child_spine } else { &[] },
                    );
                } else {
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: "TableRowIDScan",
                            est: scan_est,
                            task,
                            access: table_access(table, &[]),
                            info: rowid_info,
                        },
                        if under_bushy { cop_spine } else { &[] },
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Probe)");
                    }
                }
                return true;
            }
            // Full index scan IndexLookUp (USE INDEX without range): TiDB shows
            // IndexFullScan(Build) + TableRowIDScan(Probe) on cop[tikv].
            let info = if let Some(filter) = pushed.as_ref() {
                format!("pushed:{}", filter.restore_string())
            } else if *row_count >= 9_000.0 {
                "keep order:false, stats:pseudo".to_string()
            } else {
                "keep order:false".to_string()
            };
            let mut rowid_info = "keep order:false".to_string();
            if *row_count >= 9_000.0 {
                rowid_info.push_str(", stats:pseudo");
            }
            let scan_est = if *row_count > 0.0 {
                *row_count
            } else {
                scan_est
            };
            push_explain_row(
                rows,
                depth,
                false,
                next_id,
                CopRow {
                    name: "IndexFullScan",
                    est: scan_est,
                    task: "cop[tikv]",
                    access: idx_access,
                    info,
                },
            );
            if let Some(last) = rows.last_mut() {
                last.id.push_str("(Build)");
            }
            push_explain_row(
                rows,
                depth,
                true,
                next_id,
                CopRow {
                    name: "TableRowIDScan",
                    est: scan_est,
                    task: "cop[tikv]",
                    access: table_access(table, &[]),
                    info: rowid_info,
                },
            );
            if let Some(last) = rows.last_mut() {
                last.id.push_str("(Probe)");
            }
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
                    access: table_access(table, &[]),
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
    push_explain_row_spine(rows, depth, last, next_id, row, &[]);
}

fn push_explain_row_spine(
    rows: &mut Vec<ExplainRow>,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    row: CopRow<'_>,
    spine: &[bool],
) {
    let id = *next_id;
    *next_id += 1;
    let prefix = if spine.is_empty() {
        tree_prefix(depth, last)
    } else {
        tree_prefix_with_spine(spine, depth, last)
    };
    rows.push(ExplainRow {
        id: format!("{prefix}{}_{id}", row.name),
        est_rows: format!("{:.2}", row.est),
        task: row.task.to_string(),
        access: row.access,
        info: row.info,
    });
}

fn table_access(table: &str, partition_names: &[String]) -> String {
    if partition_names.is_empty() {
        format!("table:{table}")
    } else if partition_names.len() == 1 {
        format!("table:{table}, partition:{}", partition_names[0])
    } else {
        format!("table:{table}, partition:{}", partition_names.join(","))
    }
}

fn index_access(table: &str, index: &str, partitions: &[i64]) -> String {
    index_access_cols(table, index, &[], partitions)
}

fn index_access_cols(table: &str, index: &str, cols: &[String], partitions: &[i64]) -> String {
    let index = if cols.is_empty() {
        index.to_string()
    } else {
        format!("{index}({})", cols.join(", "))
    };
    if partitions.is_empty() {
        format!("table:{table}, index:{index}")
    } else {
        format!("table:{table}, index:{index}, partition:{partitions:?}")
    }
}

fn index_scan_meta(pushed: Option<&sealdb_parser::ast::ExprNode>) -> (f64, String) {
    // TiDB unanalyzed tables: `keep order:false, stats:pseudo`.
    if pushed.is_some() {
        (
            8_000.0,
            "keep order:false, stats:pseudo".to_string(),
        )
    } else {
        (
            10_000.0,
            "keep order:false, stats:pseudo".to_string(),
        )
    }
}

fn stream_agg_has_order_firstrow(plan: &PhysicalPlan) -> bool {
    match plan {
        // ORDER BY keeps firstrow of a real column (`funcs:firstrow(select.t.a)`);
        // DISTINCT partials use `firstrow(Column#N)` and must not inject Projection.
        PhysicalPlan::StreamAgg { info, .. } => info.contains("funcs:firstrow(select."),
        PhysicalPlan::Limit { child, .. } => stream_agg_has_order_firstrow(child),
        _ => false,
    }
}

fn is_not_isnull(filter: &sealdb_parser::ast::ExprNode) -> bool {
    not_isnull_count(filter) > 0 && not_isnull_count(filter) == filter.split_cnf().len()
}

fn not_isnull_count(filter: &sealdb_parser::ast::ExprNode) -> usize {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    filter
        .split_cnf()
        .into_iter()
        .filter(|item| {
            matches!(
                item,
                ExprNode::Unary(u)
                    if u.op == Op::Not && matches!(u.expr.as_ref(), ExprNode::IsNull(_))
            )
        })
        .count()
}

fn explain_selection_info(filter: &sealdb_parser::ast::ExprNode) -> String {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    // Bytes marker: full ExplainInfo override (e.g. `not(isnull(Column#9))`).
    if let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(b),
    }) = filter
    {
        return String::from_utf8_lossy(b).into_owned();
    }
    let parts: Vec<String> = filter
        .split_cnf()
        .into_iter()
        .map(|item| match &item {
            ExprNode::Unary(u) if u.op == Op::Not => {
                if let ExprNode::IsNull(i) = u.expr.as_ref() {
                    let col = match i.expr.as_ref() {
                        ExprNode::Column(c) => {
                            let table = c.table.as_deref().unwrap_or("t");
                            let schema = sealdb_expression::current_db_name();
                            if schema.is_empty() {
                                format!("select.{table}.{}", c.name)
                            } else {
                                format!("{schema}.{table}.{}", c.name)
                            }
                        }
                        other => other.restore_string(),
                    };
                    format!("not(isnull({col}))")
                } else {
                    explain_scalar_expr(&item, "t")
                }
            }
            other => explain_scalar_expr(other, "t"),
        })
        .collect();
    parts.join(", ")
}

fn pushed_is_bytes_info(filter: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    matches!(
        filter,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(_),
        })
    )
}

/// `RANGE:…` Bytes marker on TableReader/IndexReader pushed filter.
fn range_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<String> {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    match pushed? {
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => {
            let s = String::from_utf8_lossy(b);
            s.strip_prefix("RANGE:").map(|rest| rest.to_string())
        }
        _ => None,
    }
}

fn range_marker(plan: &PhysicalPlan) -> Option<String> {
    match plan {
        PhysicalPlan::TableReader { pushed, .. } | PhysicalPlan::IndexReader { pushed, .. } => {
            range_pushed(pushed.as_ref())
        }
        _ => None,
    }
}

/// TiDB-style scalar: `gt(Column#5, 3)` / `or(and(…), …)`.
fn explain_scalar_expr(expr: &sealdb_parser::ast::ExprNode, table: &str) -> String {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    match expr {
        ExprNode::Column(c) => {
            if c.name.starts_with("Column#") {
                c.name.clone()
            } else {
                let tbl = c.table.as_deref().unwrap_or(table);
                // Prefer session DB schema (suite name) when set — TiDB
                // `agg_predicate_pushdown.t.a` vs default `select.t.a`.
                let schema = sealdb_expression::current_db_name();
                if schema.is_empty() {
                    format!("select.{tbl}.{}", c.name)
                } else {
                    format!("{schema}.{tbl}.{}", c.name)
                }
            }
        }
        ExprNode::Value(ValueExpr { datum: Datum::Null }) => "NULL".into(),
        ExprNode::Value(ValueExpr { datum: Datum::I64(v) }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::U64(v) }) => v.to_string(),
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::LogicAnd => "and",
                Op::LogicOr => "or",
                Op::Plus => "plus",
                Op::Minus => "minus",
                Op::Mul => "mul",
                Op::Div => "div",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_scalar_expr(&bin.left, table),
                explain_scalar_expr(&bin.right, table)
            )
        }
        ExprNode::Cast(c) => {
            let target = if c.target.to_ascii_uppercase().contains("DOUBLE") {
                "double BINARY"
            } else if c.target.to_ascii_uppercase().contains("CHAR")
                || c.target.to_ascii_uppercase().contains("STRING")
            {
                "var_string(370)"
            } else {
                "binary"
            };
            format!(
                "cast({}, {target})",
                explain_scalar_expr(&c.expr, table)
            )
        }
        ExprNode::Like(l) => {
            let pattern = match l.pattern.as_ref() {
                ExprNode::Binary(b) if b.op == Op::Plus => format!(
                    "cast(plus(cast({}, double BINARY), cast({}, double BINARY)), var_string(370))",
                    explain_scalar_expr(&b.left, table),
                    explain_scalar_expr(&b.right, table),
                ),
                other => explain_scalar_expr(other, table),
            };
            format!(
                "like({}, {}, 92)",
                explain_scalar_expr(&l.expr, table),
                pattern
            )
        }
        other => other.restore_string(),
    }
}

/// TiDB cop Selection ExplainInfo: `gt(select.t.b, 0)` / `not(isnull(select.t.a))`.
fn explain_pushed_filter(table: &str, filter: &sealdb_parser::ast::ExprNode) -> String {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    if let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(b),
    }) = filter
    {
        return String::from_utf8_lossy(b).into_owned();
    }
    if is_not_isnull(filter) {
        return explain_selection_info(filter);
    }
    // CNF list → comma-joined (TiDB HAVING pushdown under TableReader).
    let cnf = filter.split_cnf();
    if cnf.len() > 1 {
        return cnf
            .iter()
            .map(|c| explain_pushed_filter(table, c))
            .collect::<Vec<_>>()
            .join(", ");
    }
    match filter {
        ExprNode::Binary(bin) if matches!(bin.op, Op::LogicOr | Op::LogicAnd) => {
            explain_scalar_expr(filter, table)
        }
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::Plus => "plus",
                _ => {
                    return explain_scalar_expr(filter, table);
                }
            };
            format!(
                "{op}({}, {})",
                explain_scalar_expr(&bin.left, table),
                explain_scalar_expr(&bin.right, table)
            )
        }
        other => explain_scalar_expr(other, table),
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
            partition_names,
            row_count,
            stats_pseudo,
            pushed,
            ..
        } => {
            let scan_access = table_access(table, partition_names);
            let scan_est = if *row_count < 0.0 {
                if *stats_pseudo {
                    10_000.0
                } else {
                    -*row_count
                }
            } else {
                *row_count
            };
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
            index_columns,
            partitions,
            pushed,
            ..
        } => {
            let access = index_access_cols(table, index, index_columns, partitions);
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
            index_columns,
            partitions,
            pushed,
            ..
        } => {
            let idx_access = index_access_cols(table, index, index_columns, partitions);
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
                &table_access(table, &[]),
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
                &table_access(table, &[]),
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

fn explain_plan_ptr(plan: &PhysicalPlan) -> *const PhysicalPlan {
    match plan {
        PhysicalPlan::CTE {
            seed,
            inline: true,
            ..
        } => explain_plan_ptr(seed),
        // Column-name Projection over a scan (CTE seed `SELECT c1`) unwraps for
        // Build/Probe pointer compare so TableReader gets the join role.
        PhysicalPlan::Projection { child, exprs, .. }
            if exprs
                .iter()
                .all(|e| !e.contains("->") && !e.contains('(') && !e.contains(',')) =>
        {
            explain_plan_ptr(child)
        }
        other => other as *const _,
    }
}

fn children_of(plan: &PhysicalPlan) -> Vec<&PhysicalPlan> {
    match plan {
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. }
        | PhysicalPlan::UnionScan { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => vec![child.as_ref()],
        PhysicalPlan::CTE {
            recursive: true, ..
        } => Vec::new(),
        PhysicalPlan::CTE { seed: child, .. } => vec![child.as_ref()],
        PhysicalPlan::Apply {
            child,
            probe: Some(probe),
            ..
        } => vec![child.as_ref(), probe.as_ref()],
        PhysicalPlan::Apply { child, .. } => vec![child.as_ref()],
        // TiDB HashJoin / MergeJoin lists Build (right) before Probe (left).
        PhysicalPlan::HashJoin { left, right, .. }
        | PhysicalPlan::MergeJoin { left, right, .. } => {
            vec![right.as_ref(), left.as_ref()]
        }
        // IndexJoin lists outer (Build) then inner (Probe).
        PhysicalPlan::IndexJoin { outer, inner, .. } => {
            vec![outer.as_ref(), inner.as_ref()]
        }
        PhysicalPlan::Union { left, right, .. } => {
            vec![left.as_ref(), right.as_ref()]
        }
        PhysicalPlan::PartitionUnion { children } => children.iter().collect(),
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
        PhysicalPlan::Projection { exprs, .. } => (
            "Projection".to_string(),
            String::new(),
            exprs.join(", "),
        ),
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
            pushed,
            partition_access,
            ..
        } => (
            "TableReader".to_string(),
            if partition_access.is_empty() {
                String::new()
            } else {
                format!("partition:{partition_access}")
            },
            if range_pushed(pushed.as_ref()).is_some() {
                "data:TableRangeScan".to_string()
            } else if pushed.is_some() {
                "data:Selection".to_string()
            } else {
                "data:TableFullScan".to_string()
            },
        ),
        PhysicalPlan::PartitionUnion { .. } => (
            "PartitionUnion".to_string(),
            String::new(),
            String::new(),
        ),
        PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
        } => (
            // TiDB EXPLAIN id is `Point_Get` (underscore before Get).
            "Point_Get".to_string(),
            format!("table:{table}"),
            format!("handle:{handle}"),
        ),
        PhysicalPlan::PointGet {
            table,
            index: Some(index),
            ..
        } => (
            "Point_Get".to_string(),
            format!("table:{table}, index:{index}"),
            String::new(),
        ),
        PhysicalPlan::PointGet { table, .. } => (
            "Point_Get".to_string(),
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
            // TiDB IndexLookUp root info is empty; Build/Probe children carry shape.
            String::new(),
        ),
        PhysicalPlan::IndexReader { pushed, .. } => (
            "IndexReader".to_string(),
            String::new(),
            if range_pushed(pushed.as_ref()).is_some() {
                "index:IndexRangeScan".to_string()
            } else if pushed.is_some() {
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
            explain_selection_info(filter),
        ),
        PhysicalPlan::Sort { by_info, .. } => {
            ("Sort".to_string(), String::new(), by_info.clone())
        }
        PhysicalPlan::TopN {
            by_info,
            offset,
            count,
            ..
        } => (
            "TopN".to_string(),
            String::new(),
            format!("{by_info}, offset:{offset}, count:{count}"),
        ),
        PhysicalPlan::Limit { offset, count, .. } => (
            "Limit".to_string(),
            String::new(),
            if *count > 0 {
                format!("offset:{offset}, count:{count}")
            } else {
                String::new()
            },
        ),
        PhysicalPlan::HashAgg { info, .. } => ("HashAgg".to_string(), String::new(), info.clone()),
        PhysicalPlan::StreamAgg { info, .. } => {
            let info = if info.is_empty() {
                // Scalar StreamAgg fallback (empty-table path).
                "funcs:count(Column#13)->Column#5".to_string()
            } else {
                info.clone()
            };
            ("StreamAgg".to_string(), String::new(), info)
        }
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            on,
            ..
        } => {
            let info = hash_join_explain_info(*tp, on, left, right);
            ("HashJoin".to_string(), String::new(), info)
        }
        PhysicalPlan::MergeJoin {
            left_key,
            right_key,
            ..
        } => (
            "MergeJoin".to_string(),
            String::new(),
            format!("inner join, left key:{left_key}, right key:{right_key}"),
        ),
        PhysicalPlan::Apply {
            filter,
            info,
            ..
        } => {
            let info = info
                .clone()
                .unwrap_or_else(|| filter.restore_string());
            ("Apply".to_string(), String::new(), info)
        }
        PhysicalPlan::IndexJoin { info, .. } => {
            ("IndexJoin".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::UnionScan { info, .. } => {
            ("UnionScan".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::Union { op, .. } => {
            // TiDB brief: UnionAll shows empty info; DISTINCT union shows UNION.
            let info = if matches!(op, sealdb_parser::ast::SetOprType::UnionAll) {
                String::new()
            } else {
                op.restore().to_string()
            };
            ("Union".to_string(), String::new(), info)
        }
        PhysicalPlan::Window { info, .. } => {
            ("Window".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::Shuffle { info, .. } => {
            ("Shuffle".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::ShuffleReceiver { .. } => {
            ("ShuffleReceiver".to_string(), String::new(), String::new())
        }
        PhysicalPlan::CTE {
            name,
            alias,
            recursive: true,
            ..
        } => (
            "CTEFullScan".to_string(),
            match alias {
                Some(a) => format!("CTE:{name} AS {a}"),
                None => format!("CTE:{name}"),
            },
            "data:CTE_0".to_string(),
        ),
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

fn is_equality_filter(filter: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    matches!(filter, ExprNode::Binary(bin) if bin.op == Op::Eq)
}

fn filter_has_like(filter: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    filter.split_cnf().iter().any(|c| {
        matches!(c, ExprNode::Like(_))
            || matches!(c, ExprNode::Binary(b) if matches!(b.left.as_ref(), ExprNode::Like(_))
                || matches!(b.right.as_ref(), ExprNode::Like(_)))
    })
}

fn est_rows(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableDual { row_count } => *row_count as f64,
        PhysicalPlan::PointGet { .. } => 1.0,
        PhysicalPlan::BatchPointGet { n_keys, .. } => *n_keys as f64,
        PhysicalPlan::Selection { child, filter, .. } => {
            let n = not_isnull_count(filter);
            // TiDB: not(isnull) over CTEFullScan uses 0.8 (6400.80 from 8001).
            if n > 0
                && n == filter.split_cnf().len()
                && matches!(
                    child.as_ref(),
                    PhysicalPlan::CTE {
                        recursive: true,
                        ..
                    }
                )
            {
                est_rows(child) * 0.8
            } else if n > 0 && n == filter.split_cnf().len() {
                est_rows(child) * 0.999_f64.powi(n as i32)
            } else if is_equality_filter(filter) {
                let base = est_rows(child);
                if base >= 10_000.0 {
                    10.0
                } else {
                    1.0
                }
            } else if filter_has_like(filter) {
                // TiDB pseudo est for `like(NULL,…) AND col` under unanalyzed scan.
                // Keep unrounded so HashJoin product is 53253333.33 (not 53253300).
                let base = est_rows(child);
                if base >= 9_999.0 {
                    5_325.333_333_333
                } else {
                    base * 0.532_533_333_333
                }
            } else {
                est_rows(child) * 0.8
            }
        }
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if *row_count < 0.0 => {
            // HAVING pushdown: filtered est encoded as -est (scan stays 10000).
            let _ = filter;
            -*row_count
        }
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if is_not_isnull(filter) => {
            let n = not_isnull_count(filter).max(1);
            (*row_count * 0.999_f64.powi(n as i32)).min(9_990.0)
        }
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if is_equality_filter(filter) => {
            if *row_count >= 10_000.0 {
                10.0
            } else {
                1.0
            }
        }
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if filter_has_like(filter) => {
            if *row_count >= 9_999.0 {
                5_325.333_333_333
            } else {
                *row_count * 0.532_533_333_333
            }
        }
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if range_pushed(Some(filter)).is_some()
            || pushed_is_bytes_info(filter) =>
        {
            *row_count
        }
        PhysicalPlan::TableReader {
            pushed: Some(_),
            row_count,
            ..
        } => *row_count * 0.8,
        PhysicalPlan::TableReader { row_count, .. } if *row_count < 0.0 => -*row_count,
        PhysicalPlan::TableReader { row_count, .. } => *row_count,
        PhysicalPlan::PartitionUnion { children } => children.iter().map(est_rows).sum(),
        PhysicalPlan::IndexReader {
            pushed: Some(filter),
            row_count,
            ..
        } if range_pushed(Some(filter)).is_some() => *row_count,
        PhysicalPlan::IndexReader { row_count, pushed: None, .. } => *row_count,
        PhysicalPlan::IndexLookUpReader {
            pushed: Some(filter),
            row_count,
            ..
        } if range_pushed(Some(filter)).is_some() => *row_count,
        PhysicalPlan::IndexLookUpReader { row_count, pushed: None, .. } => *row_count,
        PhysicalPlan::IndexLookUpReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexReader {
            pushed: Some(_), ..
        }
        | PhysicalPlan::IndexMerge {
            pushed: Some(_), ..
        } => 8_000.0,
        PhysicalPlan::Limit { child, count, .. } => {
            if *count > 0 {
                est_rows(child).min(*count as f64)
            } else {
                est_rows(child).min(1.0)
            }
        }
        PhysicalPlan::TopN { child, count, .. } => est_rows(child).min(*count as f64),
        PhysicalPlan::MergeJoin { left, right, .. } => {
            // Recursive CTEFullScan side dominates (TiDB MergeJoin est = 8001).
            let cte_side = [left.as_ref(), right.as_ref()].into_iter().find_map(|p| {
                fn find_recursive(p: &PhysicalPlan) -> Option<f64> {
                    match p {
                        PhysicalPlan::CTE {
                            recursive: true, ..
                        } => Some(8_001.0),
                        PhysicalPlan::Sort { child, .. }
                        | PhysicalPlan::Selection { child, .. }
                        | PhysicalPlan::Projection { child, .. } => find_recursive(child),
                        _ => None,
                    }
                }
                find_recursive(p)
            });
            if let Some(cte_est) = cte_side {
                return cte_est;
            }
            // TiDB unanalyzed MergeJoin est ≈ min(side)*1.25 (null-reject
            // 9990 → 12487.50; both 10000 → 12500).
            let l = est_rows(left);
            let r = est_rows(right);
            if is_pseudo_card(l) && is_pseudo_card(r) {
                l.min(r) * 1.25
            } else {
                l.max(r)
            }
        }
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            on,
            ..
        } => {
            let l = est_rows(left);
            let r = est_rows(right);
            if matches!(
                tp,
                super::physical_plans::JoinType::LeftOuter
                    | super::physical_plans::JoinType::RightOuter
            ) && !join_on_has_equijoin(on)
            {
                // CARTESIAN outer: product of child ests (TiDB 12487.50²).
                l * r
            } else if matches!(
                tp,
                super::physical_plans::JoinType::LeftOuter
                    | super::physical_plans::JoinType::RightOuter
            ) {
                // Pseudo outer-join cardinality (TiDB unanalyzed ≈ max * 1.24875);
                // analyzed tables use max(left, right). Analyzed Build ⋈ pseudo
                // Probe ≈ build * 1.25 (bushy t4 Probe → 5.00).
                if is_pseudo_card(l) && is_pseudo_card(r) {
                    l.max(r) * 1.24875
                } else if is_pseudo_card(l) && !is_pseudo_card(r) {
                    r * 1.25
                } else if !is_pseudo_card(l) && is_pseudo_card(r) {
                    l * 1.25
                } else {
                    l.max(r)
                }
            } else if matches!(
                tp,
                super::physical_plans::JoinType::Semi | super::physical_plans::JoinType::Anti
            ) && !semi_join_is_left_outer(on)
            {
                // TiDB semi/anti selectivity ≈ 0.8 of the outer (Probe) side.
                l * 0.8
            } else if matches!(tp, super::physical_plans::JoinType::Cross)
                || (matches!(tp, super::physical_plans::JoinType::Inner)
                    && !join_on_has_equijoin(on))
            {
                // Pure CARTESIAN (Cross or Inner with non-equality ON pushed away).
                l * r
            } else if matches!(tp, super::physical_plans::JoinType::Inner) {
                let lp = is_pseudo_card(l);
                let rp = is_pseudo_card(r);
                if !lp && !rp {
                    // Analyzed: ≥2× skew → max (t1⋈t3→4); else min (t2⋈t3→2).
                    let lo = l.min(r);
                    let hi = l.max(r);
                    if hi >= lo * 2.0 {
                        hi
                    } else {
                        lo
                    }
                } else if !lp && rp {
                    // Analyzed ⋈ pseudo → TiDB ~ analyzed * 1.25 (t3⋈t4 → 2.50).
                    l * 1.25
                } else if lp && !rp {
                    r * 1.25
                } else {
                    l.max(r)
                }
            } else {
                l.max(r)
            }
        }
        PhysicalPlan::Union { left, right, .. } => est_rows(left) + est_rows(right),
        PhysicalPlan::HashAgg { est_rows: e, .. } | PhysicalPlan::StreamAgg { est_rows: e, .. } => {
            *e
        }
        PhysicalPlan::IndexJoin { est_rows: e, .. } => *e,
        PhysicalPlan::UnionScan { est_rows: e, .. } => *e,
        PhysicalPlan::Apply {
            child,
            probe: Some(probe),
            ..
        } => est_rows(child).max(est_rows(probe)),
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => est_rows(child),
        PhysicalPlan::CTE {
            recursive: true, ..
        } => 8_001.0,
        PhysicalPlan::CTE { seed: child, .. } => est_rows(child),
        _ => 10_000.0,
    }
}

fn task_of(_plan: &PhysicalPlan) -> &'static str {
    // PhysicalPlan nodes are root gatherers; DistSQL scans are emitted as
    // synthetic task=cop rows in emit_cop_children.
    "root"
}

fn tree_prefix(depth: usize, last: bool) -> String {
    tree_prefix_with_spine(&[], depth, last)
}

/// `spine[i] == true` means ancestor depth i+1 still has a later sibling (`│ `).
fn tree_prefix_with_spine(spine: &[bool], depth: usize, last: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    // Linear TopN/StreamAgg chains: empty spine → spaces only (historical brief).
    if spine.is_empty() {
        for _ in 1..depth {
            prefix.push_str("  ");
        }
    } else {
        for cont in spine.iter().take(depth.saturating_sub(1)) {
            prefix.push_str(if *cont { "│ " } else { "  " });
        }
    }
    prefix.push_str(if last { "└─" } else { "├─" });
    prefix
}

/// Cop subtree under a bushy TableReader. Pad to the Selection depth; when the
/// reader is not last among HashJoin siblings, put `│` at the reader column.
fn bushy_cop_spine(
    spine: &[bool],
    scan_depth: usize,
    under_bushy: bool,
    parent_last: bool,
) -> Vec<bool> {
    if !under_bushy {
        return spine.to_vec();
    }
    let mut cop = spine.to_vec();
    let need = scan_depth.saturating_sub(1);
    while cop.len() < need {
        cop.push(false);
    }
    if !parent_last && need > 0 {
        cop[need - 1] = true;
    }
    cop
}

fn is_pseudo_card(rows: f64) -> bool {
    // Pseudo table scan is 10000; null-reject Selection ≈ 9990.
    rows >= 9_000.0
}

fn semi_join_is_left_outer(on: &sealdb_parser::ast::ExprNode) -> bool {
    semi_explain_override(on).is_some_and(|s| s.contains("left outer semi"))
}

/// Bytes marker with a full TiDB join ExplainInfo, possibly AND'd with Eq.
fn semi_explain_override(on: &sealdb_parser::ast::ExprNode) -> Option<String> {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    match on {
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => {
            let s = std::str::from_utf8(b).ok()?;
            if s.contains("semi join")
                || s.contains("anti join")
                || s.contains("inner join")
                || s.contains("outer join")
                || s.starts_with("CARTESIAN")
            {
                Some(s.to_string())
            } else {
                None
            }
        }
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            semi_explain_override(&bin.left).or_else(|| semi_explain_override(&bin.right))
        }
        _ => None,
    }
}

fn hash_join_explain_info(
    tp: super::physical_plans::JoinType,
    on: &sealdb_parser::ast::ExprNode,
    _left: &PhysicalPlan,
    _right: &PhysicalPlan,
) -> String {
    use super::physical_plans::JoinType;
    use sealdb_parser::ast::expressions::ExprNode;
    // Marker: Value(Bytes) holding a full TiDB ExplainInfo override
    // (alone or AND'd with the executable equijoin key).
    if let Some(s) = semi_explain_override(on) {
        return s;
    }
    let outer = match tp {
        JoinType::LeftOuter => "left outer join",
        JoinType::RightOuter => "right outer join",
        JoinType::Inner => "inner join",
        JoinType::Cross => "CARTESIAN inner join",
        JoinType::Semi => "semi join",
        JoinType::Anti => "anti join",
    };
    // Pure CARTESIAN Cross with stub ON — no left-cond decoration.
    if matches!(tp, JoinType::Cross)
        && matches!(on, ExprNode::Value(_))
    {
        return "CARTESIAN inner join".to_string();
    }
    if !join_on_has_equijoin(on) {
        let cond = explain_join_cond(on);
        return format!("CARTESIAN {outer}, left cond:[{cond}]");
    }
    let equal = explain_join_cond(on);
    format!("{outer}, equal:[{equal}]")
}

fn join_on_has_equijoin(on: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    match on {
        ExprNode::Binary(bin) if bin.op == Op::Eq => {
            matches!(
                (bin.left.as_ref(), bin.right.as_ref()),
                (ExprNode::Column(_), ExprNode::Column(_))
            )
        }
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            join_on_has_equijoin(&bin.left) || join_on_has_equijoin(&bin.right)
        }
        _ => false,
    }
}

fn explain_join_col(col: &sealdb_parser::ast::expressions::ColumnNameExpr) -> String {
    // OrigTblName: alias of physical `t` stays `select.t.a`; real tables use
    // their name (`select.t1.a`).
    let table = col.table.as_deref().unwrap_or("t");
    format!("select.{table}.{}", col.name)
}

fn explain_join_cond(on: &sealdb_parser::ast::ExprNode) -> String {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    match on {
        ExprNode::Binary(bin) if bin.op == Op::Eq => {
            let left = match bin.left.as_ref() {
                ExprNode::Column(c) => explain_join_col(c),
                other => other.restore_string(),
            };
            let right = match bin.right.as_ref() {
                ExprNode::Column(c) => explain_join_col(c),
                ExprNode::Value(ValueExpr {
                    datum: Datum::I64(v),
                }) => v.to_string(),
                ExprNode::Value(ValueExpr {
                    datum: Datum::U64(v),
                }) => v.to_string(),
                other => other.restore_string(),
            };
            format!("eq({left}, {right})")
        }
        other => other.restore_string(),
    }
}

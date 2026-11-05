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
    walk_with_parent(plan, None, depth, last, next_id, rows, false, &[]);
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
        );
    }
    // TiDB brief/row often elides a trivial Projection above TopN/Limit.
    // Keep remapping projections (`Column#5->Column#6`) used after StreamAgg+ORDER BY.
    if let PhysicalPlan::Projection { exprs, child, .. } = plan {
        let trivial = exprs.iter().all(|e| {
            e.starts_with("col") && !e.contains("->") && !e.contains("Column#")
        });
        if trivial {
            return walk_with_parent(
                child,
                parent,
                depth,
                last,
                next_id,
                rows,
                under_order_projection,
                spine,
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
        return walk_with_parent(plan, parent, depth + 1, true, next_id, rows, true, spine);
    }
    let id = *next_id;
    *next_id += 1;
    let (name, access, mut info) = node_meta(plan);
    // TiDB labels `TableReader_30(Build)` — role suffix after the numeric id.
    let join_role = match parent {
        Some(PhysicalPlan::HashJoin { left, right, .. })
        | Some(PhysicalPlan::MergeJoin { left, right, .. }) => {
            let left_p = left.as_ref() as *const _;
            let right_p = right.as_ref() as *const _;
            if std::ptr::eq(plan as *const _, right_p)
                && matches!(
                    name.as_str(),
                    "HashJoin" | "TableReader" | "StreamAgg" | "Projection"
                )
            {
                "(Build)"
            } else if std::ptr::eq(plan as *const _, left_p)
                && matches!(name.as_str(), "HashJoin" | "TableReader" | "Projection")
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
        }) => Some((info.as_str(), cop_info.as_str(), *est_rows)),
        _ => None,
    };
    let window_keep_order = match parent {
        Some(PhysicalPlan::Window { info, .. }) => info.contains("partition by"),
        _ => false,
    };
    let under_window = matches!(parent, Some(PhysicalPlan::Window { .. }));
    let under_stream_agg = stream_parent.is_some();
    if topn_parent.is_some() && matches!(plan, PhysicalPlan::IndexReader { .. }) {
        info = "index:TopN".to_string();
    }
    if under_stream_agg && matches!(plan, PhysicalPlan::TableReader { .. }) {
        info = "data:StreamAgg".to_string();
    }
    if under_stream_agg && matches!(plan, PhysicalPlan::IndexReader { .. }) {
        info = "index:StreamAgg".to_string();
    }
    if under_window && matches!(plan, PhysicalPlan::IndexReader { .. }) {
        info = "index:IndexFullScan".to_string();
    }
    let bushy = matches!(
        plan,
        PhysicalPlan::HashJoin { .. }
            | PhysicalPlan::MergeJoin { .. }
            | PhysicalPlan::PartitionUnion { .. }
            | PhysicalPlan::Union { .. }
    ) || matches!(
        parent,
        Some(
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
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
    } else if let Some((_, _, est)) = stream_parent {
        // Partial StreamAgg est applies to the DistSQL reader, not Projection.
        if matches!(
            plan,
            PhysicalPlan::TableReader { .. }
                | PhysicalPlan::IndexReader { .. }
                | PhysicalPlan::IndexLookUpReader { .. }
        ) {
            est
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
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
        )
    );
    // Projection→CARTESIAN: TableReader sits under Projection with a non-empty
    // spine; still need bushy cop prefixes for TableFullScan.
    let under_bushy = under_bushy
        || (matches!(parent, Some(PhysicalPlan::Projection { .. })) && !spine.is_empty());
    let under_merge_join = matches!(parent, Some(PhysicalPlan::MergeJoin { .. }));
    if emit_cop_children(
        plan,
        topn_parent,
        stream_parent,
        under_bushy,
        under_merge_join,
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
            | PhysicalPlan::PartitionUnion { .. }
            | PhysicalPlan::Union { .. }
    ) && last
        && (matches!(
            parent,
            Some(PhysicalPlan::Projection { .. } | PhysicalPlan::TopN { .. })
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
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
        ) {
            child_spine.push(!child_last);
        } else if !spine.is_empty() {
            // StreamAgg under CARTESIAN: push whether *this* node continues.
            // Projection under HashJoin: push whether the child has siblings
            // (Projection is usually unary → spaces under `│`).
            // Projection under Union: push `!last` so Dual keeps `│` when the
            // Projection is a non-last Union arm (`  ├─` / `  │ └─`).
            if matches!(plan, PhysicalPlan::Projection { .. })
                && matches!(parent, Some(PhysicalPlan::HashJoin { .. }))
            {
                child_spine.push(!child_last);
            } else {
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
        );
    }
}

/// Synthetic coprocessor subtree under TableReader / IndexReader / IndexLookUp.
/// Returns true when the plan was a DistSQL reader (children already emitted).
fn emit_cop_children(
    plan: &PhysicalPlan,
    topn_parent: Option<(&str, u64, u64)>,
    stream_parent: Option<(&str, &str, f64)>,
    under_bushy: bool,
    under_merge_join: bool,
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
            if let Some((_, cop_info, est)) = stream_parent {
                // TiDB partial StreamAgg pushed into TableReader.
                let info = if cop_info.is_empty() {
                    format!("funcs:count(select.{table}.a)->Column#13")
                } else {
                    cop_info.to_string()
                };
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "StreamAgg",
                        est,
                        task: "cop[tikv]",
                        access: String::new(),
                        info,
                    },
                );
                scan_depth = depth + 1;
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
            // Cop children inherit the TableReader spine; non-last Build readers
            // need `│` at the reader column (see `bushy_cop_spine`).
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
            pushed,
        } => {
            let access = index_access_cols(table, index, index_columns, partitions);
            let (mut scan_est, mut scan_info) = index_scan_meta(pushed.as_ref());
            let mut scan_depth = depth;
            let stream_est = stream_parent.map(|(_, _, e)| e);
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
            if let Some((_, cop_info, est)) = stream_parent {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "StreamAgg",
                        est,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: cop_info.to_string(),
                    },
                );
                scan_depth = depth + 1;
                // GROUP BY StreamAgg needs ordered index input.
                scan_info = "keep order:true, stats:pseudo".to_string();
            }
            if under_window {
                scan_info = if window_keep_order {
                    "keep order:true, stats:pseudo".to_string()
                } else {
                    "keep order:false, stats:pseudo".to_string()
                };
            }
            let scan_task = if topn_parent.is_some() || under_stream_agg || under_window {
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
            if let Some(filter) = pushed.as_ref() {
                let sel_est = if under_stream_agg {
                    (10_000.0 / 3.0) * limit_scale
                } else {
                    est_rows(plan)
                };
                let filter_info = if under_stream_agg {
                    explain_pushed_filter(table, filter)
                } else {
                    format!("pushed:{}", filter.restore_string())
                };
                // Full scan under StreamAgg+Selection stays IndexFullScan (not Range).
                let leaf_name = if under_stream_agg {
                    "IndexFullScan"
                } else {
                    "IndexRangeScan"
                };
                if under_stream_agg {
                    scan_est = 10_000.0 * limit_scale;
                }
                push_explain_row(
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
                );
                push_explain_row(
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
                );
            } else {
                push_explain_row(
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
                );
            }
            true
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            pushed,
        } => {
            let idx_access = index_access_cols(table, index, index_columns, partitions);
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
                    access: table_access(table, &[]),
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
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    let parts: Vec<String> = filter
        .split_cnf()
        .into_iter()
        .map(|item| match &item {
            ExprNode::Unary(u) if u.op == Op::Not => {
                if let ExprNode::IsNull(i) = u.expr.as_ref() {
                    let col = match i.expr.as_ref() {
                        ExprNode::Column(c) => {
                            let table = c.table.as_deref().unwrap_or("t");
                            format!("select.{table}.{}", c.name)
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
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_scalar_expr(&bin.left, table),
                explain_scalar_expr(&bin.right, table)
            )
        }
        other => other.restore_string(),
    }
}

/// TiDB cop Selection ExplainInfo: `gt(select.t.b, 0)` / `not(isnull(select.t.a))`.
fn explain_pushed_filter(table: &str, filter: &sealdb_parser::ast::ExprNode) -> String {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
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
                _ => {
                    return format!("pushed:{}", filter.restore_string());
                }
            };
            format!(
                "{op}({}, {})",
                explain_scalar_expr(&bin.left, table),
                explain_scalar_expr(&bin.right, table)
            )
        }
        other => format!("pushed:{}", other.restore_string()),
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

fn children_of(plan: &PhysicalPlan) -> Vec<&PhysicalPlan> {
    match plan {
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
        | PhysicalPlan::CTE { seed: child, .. }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => vec![child.as_ref()],
        // TiDB HashJoin / MergeJoin lists Build (right) before Probe (left).
        PhysicalPlan::HashJoin { left, right, .. }
        | PhysicalPlan::MergeJoin { left, right, .. } => {
            vec![right.as_ref(), left.as_ref()]
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
            if pushed.is_some() {
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
        PhysicalPlan::Apply { filter, .. } => {
            ("Apply".to_string(), String::new(), filter.restore_string())
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

fn est_rows(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableDual { row_count } => *row_count as f64,
        PhysicalPlan::PointGet { .. } => 1.0,
        PhysicalPlan::BatchPointGet { n_keys, .. } => *n_keys as f64,
        PhysicalPlan::Selection { child, filter, .. } => {
            let n = not_isnull_count(filter);
            if n > 0 && n == filter.split_cnf().len() {
                est_rows(child) * 0.999_f64.powi(n as i32)
            } else if is_equality_filter(filter) {
                let base = est_rows(child);
                if base >= 10_000.0 {
                    10.0
                } else {
                    1.0
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
            pushed: Some(_),
            row_count,
            ..
        } => *row_count * 0.8,
        PhysicalPlan::TableReader { row_count, .. } if *row_count < 0.0 => -*row_count,
        PhysicalPlan::TableReader { row_count, .. } => *row_count,
        PhysicalPlan::PartitionUnion { children } => children.iter().map(est_rows).sum(),
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
            // TiDB unanalyzed MergeJoin est ≈ probe * 1.25 (10000 → 12500).
            let l = est_rows(left);
            let r = est_rows(right);
            if is_pseudo_card(l) && is_pseudo_card(r) {
                l.max(r) * 1.25
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
            } else if matches!(tp, super::physical_plans::JoinType::Semi)
                && !semi_join_is_left_outer(on)
            {
                // TiDB semi-join selectivity ≈ 0.8 of the outer (Probe) side.
                l * 0.8
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
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
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
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    matches!(
        on,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) if std::str::from_utf8(b).is_ok_and(|s| s.contains("left outer semi"))
    )
}

fn hash_join_explain_info(
    tp: super::physical_plans::JoinType,
    on: &sealdb_parser::ast::ExprNode,
    _left: &PhysicalPlan,
    _right: &PhysicalPlan,
) -> String {
    use super::physical_plans::JoinType;
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    // Marker: Value(Bytes) holding a full TiDB ExplainInfo override.
    if let ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(b),
    }) = on
        && let Ok(s) = std::str::from_utf8(b)
        && (s.contains("semi join") || s.contains("anti join"))
    {
        return s.to_string();
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

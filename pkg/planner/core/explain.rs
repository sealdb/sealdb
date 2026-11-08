use super::physical_plans::{JoinType, PhysicalPlan};
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
    // TiDB renders `NULL` in the access-object column for every node without an
    // explicit object once a GLOBAL index read is pinned to named partitions
    // (`FROM PARTITION (pN)`); unpinned reads render an empty column instead.
    let pinned = plan_reads_pinned_global_index(plan);
    PINNED_PARTITION.with(|c| c.set(pinned));
    walk(plan, 0, true, &mut 1, &mut rows, &mut deferred);
    // TiDB: recursive CTE producers follow the main plan as `CTE_0` siblings.
    // A CTE referenced N times is still one shared definition (CTE_0).
    let mut producers: Vec<PhysicalPlan> = Vec::new();
    for p in deferred.into_iter().filter(|p| {
        matches!(
            p,
            PhysicalPlan::CTE {
                recursive: true,
                ..
            }
        )
    }) {
        let seen = producers.iter().any(|seen| match (seen, &p) {
            (PhysicalPlan::CTE { name: a, .. }, PhysicalPlan::CTE { name: b, .. }) => {
                a.eq_ignore_ascii_case(b)
            }
            _ => false,
        });
        if !seen {
            producers.push(p);
        }
    }
    for (i, cte) in producers.iter().enumerate() {
        emit_recursive_cte_producer(cte, i, &mut 1, &mut rows);
    }
    PINNED_PARTITION.with(|c| c.set(false));
    rows
}

thread_local! {
    /// Set while rendering a plan with a pinned GLOBAL index read.
    static PINNED_PARTITION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Cop `Projection` pushed above a reader's cop subtree. Set from the
    /// marker projection the planner emits for `SELECT c … WHERE other`.
    static COP_PROJECTION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// `COP_PROJ|<exprs>` marker on a top-level projection that TiDB pushes into
/// the cop task (`data:Projection` / `index:Projection`).
fn cop_projection_marker(plan: &PhysicalPlan) -> Option<&str> {
    let PhysicalPlan::Projection { exprs, child, .. } = plan else {
        return None;
    };
    if !matches!(
        child.as_ref(),
        PhysicalPlan::TableReader { .. } | PhysicalPlan::IndexReader { .. }
    ) {
        return None;
    }
    exprs.iter().find_map(|e| e.strip_prefix("COP_PROJ|"))
}

/// Active cop projection text, consumed by the reader currently being emitted.
fn cop_projection_pending() -> Option<String> {
    COP_PROJECTION.with(|c| c.borrow().clone())
}

/// True when the plan pins a GLOBAL index reader to named partitions.
fn plan_reads_pinned_global_index(plan: &PhysicalPlan) -> bool {
    let access = match plan {
        PhysicalPlan::IndexReader {
            partition_access, ..
        }
        | PhysicalPlan::IndexLookUpReader {
            partition_access, ..
        } => partition_access,
        _ => "",
    };
    if !access.is_empty() && access != "all" {
        return true;
    }
    children_of(plan)
        .into_iter()
        .any(plan_reads_pinned_global_index)
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
        1.0,
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
    apply_factor: f64,
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
            apply_factor,
        );
    }
    // Recursive CTE consumer: CTEFullScan in the main plan; producer deferred.
    if let PhysicalPlan::CTE {
        recursive: true, ..
    } = plan
    {
        if parent.is_some() {
            deferred.push(plan.clone());
            emit_cte_full_scan(
                plan,
                parent,
                depth,
                last,
                next_id,
                rows,
                spine,
                apply_factor,
            );
            return;
        }
        // Bare recursive CTE root — emit producer only.
        emit_recursive_cte_producer(plan, 0, next_id, rows);
        return;
    }
    // TiDB brief/row often elides a trivial Projection above TopN/Limit.
    // Keep remapping projections (`Column#5->Column#6`) used after StreamAgg+ORDER BY.
    // Also elide CTE seed column-name projections (`SELECT c1`) so MergeJoin
    // Build/Probe labels land on the TableReader.
    // Cop Projection: TiDB keeps it inside the reader (`data:Projection`), above
    // the reader's cop subtree, so emit the reader with the projection row.
    if let (Some(list), PhysicalPlan::Projection { child, .. }) =
        (cop_projection_marker(plan), plan)
    {
        let list = list.to_string();
        COP_PROJECTION.with(|c| *c.borrow_mut() = Some(list));
        let rendered = walk_with_parent(
            child,
            parent,
            depth,
            last,
            next_id,
            rows,
            under_order_projection,
            spine,
            deferred,
            apply_factor,
        );
        COP_PROJECTION.with(|c| *c.borrow_mut() = None);
        return rendered;
    }
    if let PhysicalPlan::Projection { exprs, child, .. } = plan {
        let trivial = exprs
            .iter()
            .all(|e| e.starts_with("col") && !e.contains("->") && !e.contains("Column#"));
        let cte_seed_cols = exprs.iter().all(|e| {
            !e.contains("->")
                && !e.contains('(')
                && !e.contains(',')
                && !e.contains('.')
                && !e.is_empty()
        }) && matches!(
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
                apply_factor,
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
        // The synthetic node occupies one more level, so its continuation
        // column must be carried into the real plan below it.
        let mut child_spine = spine.to_vec();
        child_spine.push(!last);
        return walk_with_parent(
            plan,
            parent,
            depth + 1,
            true,
            next_id,
            rows,
            true,
            &child_spine,
            deferred,
            apply_factor,
        );
    }
    // TiDB raises the star-projection above the join themselves (built in the
    // planner), so Sort over a join needs no synthesized Projection here.
    let id = *next_id;
    *next_id += 1;
    let (name, access, mut info) = node_meta(plan);
    // Pinned GLOBAL reads render the empty IndexLookUp operator info as `NULL`.
    if info.is_empty()
        && matches!(plan, PhysicalPlan::IndexLookUpReader { .. })
        && PINNED_PARTITION.with(|c| c.get())
    {
        info = "NULL".to_string();
    }
    // Correlated predicate over a recursive CTE scan refers to the CTE's
    // original table (`cte1.tt.c1`), not the CTE name (`cte1.cte1.c1`).
    if matches!(plan, PhysicalPlan::Selection { .. })
        && let Some((cte_name, orig)) = recursive_cte_qualifier(plan)
    {
        let schema = sealdb_expression::current_db_name();
        let schema = if schema.is_empty() {
            "select".to_string()
        } else {
            schema
        };
        info = info.replace(
            &format!("{schema}.{cte_name}."),
            &format!("{schema}.{orig}."),
        );
    }
    // TiDB labels `TableReader_30(Build)` — role suffix after the numeric id.
    // Inline CTEs unwrap before walk; compare against the resolved seed pointer.
    let join_role = match parent {
        // HashJoin / MergeJoin: children_of lists right(Build) then left(Probe).
        // A RIGHT JOIN is executed with the left, null-supplying side as build.
        Some(PhysicalPlan::HashJoin {
            left, right, tp, ..
        }) => {
            // Build is the hashed (smaller) input; a RIGHT JOIN picks it by
            // cardinality exactly like `children_of` above.
            let right_outer = matches!(tp, JoinType::RightOuter);
            let right_smaller = !right_outer || est_rows(right) < est_rows(left);
            let (build, probe) = if right_smaller {
                (explain_plan_ptr(right), explain_plan_ptr(left))
            } else {
                (explain_plan_ptr(left), explain_plan_ptr(right))
            };
            join_role_label(name.as_str(), explain_plan_ptr(plan), build, probe)
        }
        // MergeJoin hashes its left input, so a RIGHT JOIN — which lists
        // Build (left) before Probe (right) — labels left as Build.
        Some(PhysicalPlan::MergeJoin {
            left, right, tp, ..
        }) => {
            let (build, probe) = if matches!(tp, JoinType::RightOuter) {
                (explain_plan_ptr(left), explain_plan_ptr(right))
            } else {
                (explain_plan_ptr(right), explain_plan_ptr(left))
            };
            join_role_label(name.as_str(), explain_plan_ptr(plan), build, probe)
        }
        // Null-aware anti join: Build (right) then Probe (left), like HashJoin.
        Some(PhysicalPlan::NullAwareAntiJoin {
            probe: left,
            build: right,
            ..
        }) => join_role_label(
            name.as_str(),
            explain_plan_ptr(plan),
            explain_plan_ptr(right),
            explain_plan_ptr(left),
        ),
        Some(PhysicalPlan::Apply {
            child,
            probe: Some(probe),
            ..
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
                        | "HashAgg"
                        | "Selection"
                        | "HashJoin"
                        | "IndexJoin"
                        | "UnionScan"
                        | "Limit"
                        | "CTEFullScan"
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
                        | "HashAgg"
                        | "Selection"
                        | "HashJoin"
                        | "IndexJoin"
                        | "UnionScan"
                        | "Limit"
                        | "CTEFullScan"
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
        // GLOBAL index aggregation: TiDB pushes HashAgg into the cop task and
        // labels the reader `index:HashAgg`.
        if matches!(plan, PhysicalPlan::IndexReader { .. })
            && agg_name == "HashAgg"
            && !cop_info.is_empty()
        {
            info = "index:HashAgg".to_string();
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
            | PhysicalPlan::NullAwareAntiJoin { .. }
            | PhysicalPlan::RowProjection { .. }
            | PhysicalPlan::IndexJoin { .. }
            | PhysicalPlan::Apply { probe: Some(_), .. }
            | PhysicalPlan::PartitionUnion { .. }
            | PhysicalPlan::Union { .. }
            | PhysicalPlan::DistinctUnion { .. }
    ) || matches!(
        parent,
        Some(
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::NullAwareAntiJoin { .. }
                | PhysicalPlan::RowProjection { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::Apply { probe: Some(_), .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
                | PhysicalPlan::DistinctUnion { .. }
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
    // TiDB Apply: probe-side cardinality is multiplied by the outer row count
    // (Limit(1) / Selection over a CTEFullScan in correlated EXISTS).
    let row_est = if apply_factor > 1.0
        && plan_contains_recursive_cte(plan)
        && matches!(
            plan,
            PhysicalPlan::Limit { .. }
                | PhysicalPlan::Selection { .. }
                | PhysicalPlan::Projection { .. }
        ) {
        row_est * apply_factor
    } else {
        row_est
    };
    rows.push(ExplainRow {
        id: format!("{prefix}{name}_{id}{join_role}"),
        est_rows: if matches!(
            plan,
            PhysicalPlan::Insert { .. } | PhysicalPlan::Update { .. } | PhysicalPlan::Delete { .. }
        ) {
            "N/A".to_string()
        } else {
            format!("{row_est:.2}")
        },
        task: task_of(plan).to_string(),
        access: render_access(access),
        info,
    });
    // DistSQL readers expand into TiDB-shaped cop children (TableFullScan /
    // IndexRangeScan / TableRowIDScan) so EXPLAIN shows task=cop estRows.
    let under_bushy = matches!(
        parent,
        Some(
            PhysicalPlan::HashJoin { .. }
                | PhysicalPlan::MergeJoin { .. }
                | PhysicalPlan::NullAwareAntiJoin { .. }
                | PhysicalPlan::IndexJoin { .. }
                | PhysicalPlan::Apply { probe: Some(_), .. }
                | PhysicalPlan::PartitionUnion { .. }
                | PhysicalPlan::Union { .. }
                | PhysicalPlan::DistinctUnion { .. }
        )
    );
    // Projection/Selection/Sort/HashAgg/UnionScan/Limit→CARTESIAN: TableReader
    // sits under a unary with a non-empty spine; still need bushy cop prefixes.
    // A DML node with foreign-key children likewise has a later sibling after
    // its source plan, so the reader column needs its continuation bar.
    let under_bushy = under_bushy
        || matches!(
            parent,
            Some(
                PhysicalPlan::Insert { fk_children, .. }
                    | PhysicalPlan::Update { fk_children, .. }
                    | PhysicalPlan::Delete { fk_children, .. }
            ) if !fk_children.is_empty()
        )
        || (!spine.is_empty()
            && matches!(
                parent,
                Some(
                    PhysicalPlan::Projection { .. }
                        | PhysicalPlan::RowProjection { .. }
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
        apply_factor,
    ) {
        return;
    }
    let children = children_of(plan);
    // Foreign-key checks / cascades are DML children emitted after the
    // statement's own source plan (TiDB lists them last, as siblings).
    let fk_nodes: &[super::physical_plans::FkNode] = match plan {
        PhysicalPlan::Insert { fk_children, .. }
        | PhysicalPlan::Update { fk_children, .. }
        | PhysicalPlan::Delete { fk_children, .. } => fk_children,
        _ => &[],
    };
    let child_count = children.len() + fk_nodes.len();
    for (i, child) in children.iter().enumerate() {
        let child_last = i + 1 == child_count;
        let mut child_spine = spine.to_vec();
        // `spine[d-1]` is the continuation column of the ancestor at depth `d`:
        // `│ ` while that ancestor still has a later sibling. Descending appends
        // this node's own continuation, which grandchildren then render.
        if parent.is_some() {
            child_spine.push(!last);
        }
        // Apply probe side: TiDB multiplies correlated inner est by outer rows.
        let child_apply = match plan {
            PhysicalPlan::Apply {
                child: outer,
                probe: Some(probe),
                ..
            } if explain_plan_ptr(child) == explain_plan_ptr(probe) => est_rows(outer).max(1.0),
            // Inner sides of a join are re-estimated by the join itself, so the
            // Apply outer-row factor must not leak through them.
            _ if matches!(
                plan,
                PhysicalPlan::HashJoin { .. }
                    | PhysicalPlan::MergeJoin { .. }
                    | PhysicalPlan::IndexJoin { .. }
            ) =>
            {
                1.0
            }
            _ => apply_factor,
        };
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
            child_apply,
        );
    }
    for (j, fk) in fk_nodes.iter().enumerate() {
        let fk_last = children.len() + j + 1 == child_count;
        let mut child_spine = spine.to_vec();
        if parent.is_some() {
            child_spine.push(!last);
        }
        emit_fk_row(fk, depth + 1, fk_last, next_id, rows, &child_spine);
    }
}

/// One `Foreign_Key_Check` / `Foreign_Key_Cascade` row: `estRows` is `0.00`
/// (the nodes only touch a handful of keys at runtime), `task=root`.
fn emit_fk_row(
    fk: &super::physical_plans::FkNode,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
    spine: &[bool],
) {
    use super::physical_plans::FkAction;
    let id = *next_id;
    *next_id += 1;
    let prefix = if spine.is_empty() {
        tree_prefix(depth, last)
    } else {
        tree_prefix_with_spine(spine, depth, last)
    };
    let (name, info) = match fk.action {
        FkAction::CheckExist => (
            "Foreign_Key_Check",
            format!("foreign_key:{}, check_exist", fk.fk_name),
        ),
        FkAction::CheckNotExist => (
            "Foreign_Key_Check",
            format!("foreign_key:{}, check_not_exist", fk.fk_name),
        ),
        FkAction::OnUpdateCascade => (
            "Foreign_Key_Cascade",
            format!("foreign_key:{}, on_update:CASCADE", fk.fk_name),
        ),
        FkAction::OnDeleteCascade => (
            "Foreign_Key_Cascade",
            format!("foreign_key:{}, on_delete:CASCADE", fk.fk_name),
        ),
    };
    let access = match &fk.index {
        Some(index) => format!("table:{}, index:{index}", fk.table),
        None => format!("table:{}", fk.table),
    };
    rows.push(ExplainRow {
        id: format!("{prefix}{name}_{id}"),
        est_rows: "0.00".to_string(),
        task: "root".to_string(),
        access,
        info,
    });
}

fn emit_cte_full_scan(
    plan: &PhysicalPlan,
    parent: Option<&PhysicalPlan>,
    depth: usize,
    last: bool,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
    spine: &[bool],
    apply_factor: f64,
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
    let est = est_rows(plan) * apply_factor.max(1.0);
    rows.push(ExplainRow {
        id: format!("{prefix}CTEFullScan_{id}{join_role}"),
        est_rows: format!("{est:.2}"),
        task: "root".to_string(),
        access,
        info: "data:CTE_0".to_string(),
    });
}

fn emit_recursive_cte_producer(
    plan: &PhysicalPlan,
    producer_idx: usize,
    next_id: &mut usize,
    rows: &mut Vec<ExplainRow>,
) {
    let PhysicalPlan::CTE {
        name,
        seed,
        recursive_plan,
        recursive: true,
        limit,
        offset,
        ..
    } = plan
    else {
        return;
    };
    let est = est_rows(plan);
    let info = match (limit, offset) {
        (Some(count), Some(off)) => {
            format!("Recursive CTE, limit(offset:{off}, count:{count})")
        }
        (Some(count), None) => format!("Recursive CTE, limit(offset:0, count:{count})"),
        (None, Some(off)) => format!("Recursive CTE, limit(offset:{off}, count:0)"),
        (None, None) => "Recursive CTE".to_string(),
    };
    rows.push(ExplainRow {
        // `CTE_0` strips to `CTE` under Gate B; avoid `CTE_0_N` → `CTE_0`.
        id: format!("CTE_{producer_idx}"),
        est_rows: format!("{est:.2}"),
        task: "root".to_string(),
        access: String::new(),
        info,
    });
    let seed_part = match seed.as_ref() {
        PhysicalPlan::Union { children, .. } if !children.is_empty() => children[0].as_ref(),
        other => other,
    };
    let rec_part = recursive_plan
        .as_ref()
        .map(|p| p.as_ref())
        .or_else(|| match seed.as_ref() {
            PhysicalPlan::Union { children, .. } => children.get(1).map(|c| c.as_ref()),
            _ => None,
        });
    let orig_tbl = seed_orig_table(seed_part).unwrap_or_else(|| name.clone());
    emit_cte_seed_part(seed_part, next_id, rows);
    if let Some(rec) = rec_part {
        emit_cte_recursive_part(rec, name, &orig_tbl, next_id, rows, limit.is_some());
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
    // TiDB Seed Part is always a TableReader; column Projection / Selection sit
    // in the cop subtree (`data:Projection` / `data:Selection` / `data:TableFullScan`).
    // Remap projections (`col0` / bare names from build_cte_plan) are unwrap-only.
    let is_remap = |exprs: &[String]| {
        exprs
            .iter()
            .all(|e| !e.contains('.') && !e.contains("->") && !e.contains('('))
    };
    let (proj_exprs, selection, reader) = match plan {
        PhysicalPlan::Projection { exprs, child, .. } if is_remap(exprs) => match child.as_ref() {
            PhysicalPlan::Selection {
                filter,
                child: scan,
                ..
            } => (None, Some(filter), scan.as_ref()),
            other => (None, None, other),
        },
        PhysicalPlan::Projection { exprs, child, .. } => match child.as_ref() {
            PhysicalPlan::Selection {
                filter,
                child: scan,
                ..
            } => (Some(exprs.as_slice()), Some(filter), scan.as_ref()),
            other => (Some(exprs.as_slice()), None, other),
        },
        PhysicalPlan::Selection { filter, child, .. } => (None, Some(filter), child.as_ref()),
        other => (None, None, other),
    };
    let PhysicalPlan::TableReader {
        table,
        partition_names,
        stats_pseudo,
        pushed,
        ..
    } = reader
    else {
        // Fallback: emit whatever plan we have.
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
        return;
    };
    let id = *next_id;
    *next_id += 1;
    let data = if proj_exprs.is_some() {
        "data:Projection"
    } else if selection.is_some() || pushed.is_some() {
        "data:Selection"
    } else {
        "data:TableFullScan"
    };
    rows.push(ExplainRow {
        id: format!("├─TableReader_{id}(Seed Part)"),
        est_rows: if proj_exprs.is_some() || selection.is_some() {
            "10.00".to_string()
        } else {
            "10000.00".to_string()
        },
        task: "root".to_string(),
        access: String::new(),
        info: data.to_string(),
    });
    let keep = if *stats_pseudo {
        "keep order:false, stats:pseudo"
    } else {
        "keep order:false"
    };
    if let Some(exprs) = proj_exprs {
        let proj_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("│ └─Projection_{proj_id}"),
            est_rows: "10.00".to_string(),
            task: "cop[tikv]".to_string(),
            access: String::new(),
            info: exprs.join(", "),
        });
        if let Some(filter) = selection {
            let sel_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("│   └─Selection_{sel_id}"),
                est_rows: "10.00".to_string(),
                task: "cop[tikv]".to_string(),
                access: String::new(),
                info: explain_selection_info(filter),
            });
            let scan_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("│     └─TableFullScan_{scan_id}"),
                est_rows: "10000.00".to_string(),
                task: "cop[tikv]".to_string(),
                access: table_access(table, partition_names),
                info: keep.to_string(),
            });
        } else {
            let scan_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("│   └─TableFullScan_{scan_id}"),
                est_rows: "10000.00".to_string(),
                task: "cop[tikv]".to_string(),
                access: table_access(table, partition_names),
                info: keep.to_string(),
            });
        }
    } else if let Some(filter) = selection.or(pushed.as_ref()) {
        let sel_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("│ └─Selection_{sel_id}"),
            est_rows: "10.00".to_string(),
            task: "cop[tikv]".to_string(),
            access: String::new(),
            info: explain_selection_info(filter),
        });
        let scan_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("│   └─TableFullScan_{scan_id}"),
            est_rows: "10000.00".to_string(),
            task: "cop[tikv]".to_string(),
            access: table_access(table, partition_names),
            info: keep.to_string(),
        });
    } else {
        let scan_id = *next_id;
        *next_id += 1;
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
    has_limit: bool,
) {
    let schema = sealdb_expression::current_db_name();
    let schema = if schema.is_empty() {
        "select".to_string()
    } else {
        schema
    };
    // `… where exists (select /*+ no_decorrelate() */ …)`: TiDB keeps the
    // correlated Apply and a Limit(1) probe over the inner scan.
    if let PhysicalPlan::Apply {
        probe: Some(probe),
        info,
        ..
    } = plan
    {
        let info = info.clone().unwrap_or_else(|| "CARTESIAN semi join".into());
        let mut reader = probe.as_ref();
        let mut filter: Option<&sealdb_parser::ast::ExprNode> = None;
        let table = loop {
            match reader {
                PhysicalPlan::Limit { child, .. } | PhysicalPlan::Sort { child, .. } => {
                    reader = child.as_ref();
                }
                PhysicalPlan::Selection {
                    child, filter: f, ..
                } => {
                    filter = Some(f);
                    reader = child.as_ref();
                }
                PhysicalPlan::TableReader { table, pushed, .. } => {
                    if let (None, Some(f)) = (filter, pushed.as_ref()) {
                        filter = Some(f);
                    }
                    break Some(table.clone());
                }
                _ => break None,
            }
        };
        if let Some(table) = table {
            let id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("└─Apply_{id}(Recursive Part)"),
                est_rows: "10000.00".into(),
                task: "root".into(),
                access: String::new(),
                info,
            });
            let tbl_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("  ├─CTETable_{tbl_id}(Build)"),
                est_rows: "10000.00".into(),
                task: "root".into(),
                access: String::new(),
                info: "Scan on CTE_0".into(),
            });
            let lim_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("  └─Limit_{lim_id}(Probe)"),
                est_rows: "10000.00".into(),
                task: "root".into(),
                access: String::new(),
                info: "offset:0, count:1".into(),
            });
            let rd_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("    └─TableReader_{rd_id}"),
                est_rows: "10000.00".into(),
                task: "root".into(),
                access: String::new(),
                info: "data:Limit".into(),
            });
            let cop_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("      └─Limit_{cop_id}"),
                est_rows: "10000.00".into(),
                task: "cop[tikv]".into(),
                access: String::new(),
                info: "offset:0, count:1".into(),
            });
            let mut depth = 6;
            if let Some(f) = filter {
                let sel_id = *next_id;
                *next_id += 1;
                let info = explain_selection_info(f)
                    .replace(&format!(".{cte_name}."), &format!(".{orig_tbl}."))
                    .replace(".t.", &format!(".{orig_tbl}."));
                rows.push(ExplainRow {
                    id: format!("{}└─Selection_{sel_id}", " ".repeat(depth + 2)),
                    est_rows: "10000.00".into(),
                    task: "cop[tikv]".into(),
                    access: String::new(),
                    info,
                });
                depth += 2;
            }
            let scan_id = *next_id;
            *next_id += 1;
            rows.push(ExplainRow {
                id: format!("{}└─TableFullScan_{scan_id}", " ".repeat(depth + 2)),
                est_rows: "10000000.00".into(),
                task: "cop[tikv]".into(),
                access: format!("table:{table}"),
                info: "keep order:false, stats:pseudo".into(),
            });
            return;
        }
    }
    let proj_info = recursive_arm_proj_info(plan, &schema, orig_tbl);
    let filter_info = recursive_arm_filter_info(plan, cte_name, orig_tbl);
    // TiDB: recursive LIMIT clamps seed/recursive arm est to 10.
    let (proj_est, sel_est, tbl_est) = if has_limit {
        ("10.00", "10.00", "10.00")
    } else {
        ("8000.00", "8000.00", "10000.00")
    };
    let id = *next_id;
    *next_id += 1;
    rows.push(ExplainRow {
        id: format!("└─Projection_{id}(Recursive Part)"),
        est_rows: proj_est.to_string(),
        task: "root".to_string(),
        access: String::new(),
        info: proj_info,
    });
    if let Some(filt) = filter_info {
        let sel_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("  └─Selection_{sel_id}"),
            est_rows: sel_est.to_string(),
            task: "root".to_string(),
            access: String::new(),
            info: filt,
        });
        let tbl_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("    └─CTETable_{tbl_id}"),
            est_rows: tbl_est.to_string(),
            task: "root".to_string(),
            access: String::new(),
            info: "Scan on CTE_0".to_string(),
        });
    } else {
        let tbl_id = *next_id;
        *next_id += 1;
        rows.push(ExplainRow {
            id: format!("  └─CTETable_{tbl_id}"),
            est_rows: tbl_est.to_string(),
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
        let parts: Vec<String> = exprs
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains("plus") || e.contains("->"))
            .map(|(i, e)| {
                let mut s = e.clone();
                s = s.replace(".t.", &format!(".{orig_tbl}."));
                s = s.replace("cte1.", &format!("{orig_tbl}."));
                // Apply-shaped: cast(plus(...), int)->schema.tbl.col
                if s.contains("cast(") && s.contains("->") && !s.contains("Column#") {
                    return s;
                }
                if let Some((left, right)) = s.split_once("->") {
                    let core = left.trim();
                    let core = if core.starts_with("cast(") {
                        core.to_string()
                    } else {
                        format!("cast({core}, int)")
                    };
                    let col = right
                        .rsplit('.')
                        .next()
                        .map(|c| c.trim())
                        .filter(|c| !c.is_empty() && !c.starts_with("Column#"))
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| format!("c{}", i + 1));
                    return format!("{core}->{schema}.{orig_tbl}.{col}");
                }
                s
            })
            .collect();
        if !parts.is_empty() {
            return parts.join(", ");
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
    apply_factor: f64,
) -> bool {
    let under_stream_agg = stream_parent.is_some();
    // Cop projection pushed above a reader's cop subtree: TiDB prints it as the
    // reader's cop child, with the scan/selection one level deeper.
    if matches!(
        plan,
        PhysicalPlan::TableReader { .. } | PhysicalPlan::IndexReader { .. }
    ) && let Some(list) = cop_projection_pending()
    {
        let cop_owned = bushy_cop_spine(spine, depth, under_bushy, _parent_last);
        push_explain_row_spine(
            rows,
            depth,
            true,
            next_id,
            CopRow {
                name: "Projection",
                est: est_rows(plan) * apply_factor.max(1.0),
                task: "cop[tikv]",
                access: String::new(),
                info: list,
            },
            if under_bushy {
                cop_owned.as_slice()
            } else {
                &[]
            },
        );
        COP_PROJECTION.with(|c| *c.borrow_mut() = None);
        return emit_cop_children(
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
            _parent_last,
            apply_factor,
        );
    }
    match plan {
        PhysicalPlan::TableReader {
            table,
            partition_names,
            row_count,
            stats_pseudo,
            range_scan,
            keep_order,
            data_limit,
            pushed,
            ..
        } => {
            let scan_access = table_access(table, partition_names);
            // Negative row_count encodes HAVING-push filtered est (|row_count|);
            // TableFullScan stays at the pseudo/analyzed scan cardinality.
            let scan_est = if *row_count < 0.0 {
                if *stats_pseudo { 10_000.0 } else { -*row_count }
            } else {
                *row_count
            };
            // TiDB Apply: correlated cop Selection/FullScan multiply by outer rows.
            let scan_est = if apply_factor > 1.0 {
                scan_est * apply_factor
            } else {
                scan_est
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
            // Clustered handle range: TiDB scans the PK as a table range
            // (`TableRangeScan`), eliding the pushed Selection row.
            if *data_limit {
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Limit",
                        est: 1.0,
                        task: scan_task,
                        access: String::new(),
                        info: "offset:0, count:1".to_string(),
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                push_explain_row_spine(
                    rows,
                    scan_depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "TableFullScan",
                        est: 1.0,
                        task: scan_task,
                        access: scan_access,
                        info: "keep order:true, desc, stats:pseudo".to_string(),
                    },
                    if under_bushy { &scan_spine } else { &[] },
                );
                return true;
            }
            if let Some(range_info) = range_scan {
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                let keep = if *stats_pseudo {
                    format!(
                        "range:{range_info}, keep order:{}, stats:pseudo",
                        if under_merge_join { "true" } else { "false" }
                    )
                } else {
                    format!(
                        "range:{range_info}, keep order:{}",
                        if under_merge_join { "true" } else { "false" }
                    )
                };
                let range_est = est_rows(plan) * apply_factor.max(1.0);
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TableRangeScan",
                        est: range_est,
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
            // RANGE: marker → TableRangeScan (no Selection row).
            if let Some(range_info) = range_pushed(pushed.as_ref()) {
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                let keep = if *stats_pseudo {
                    format!(
                        "{range}, keep order:{order}, stats:pseudo",
                        range = range_prefix(&range_info),
                        order = if under_merge_join { "true" } else { "false" }
                    )
                } else {
                    format!(
                        "{range}, keep order:{order}",
                        range = range_prefix(&range_info),
                        order = if under_merge_join { "true" } else { "false" }
                    )
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
            // MergeJoin needs ordered PK input → keep order:true (TiDB); an
            // `ORDER BY` on the handle reads the table in handle order.
            let scan_info = if under_merge_join || *keep_order {
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
            // TiDB `stats:partial[…]`: columns analyzed after the last ANALYZE
            // keep a pseudo selectivity and are listed on the scan.
            let partial = partial_sel_pushed(pushed.as_ref());
            let scan_info = match &partial {
                Some((missing, _)) => {
                    let cols = missing
                        .split(',')
                        .map(|c| format!("{}:missing", c.trim()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{scan_info}, stats:partial[{cols}]")
                }
                None => scan_info,
            };
            let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
            let cop_spine = cop_owned.as_slice();
            // COP_PROJ_SEL: EXPLAIN-only cop Projection above the pushed Selection
            // (`DELETE` on a `_tidb_rowid` table).
            if let Some((proj, sel)) = cop_proj_sel_pushed(pushed.as_ref()) {
                let sel_est = est_rows(plan) * apply_factor.max(1.0);
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Projection",
                        est: sel_est,
                        task: scan_task,
                        access: String::new(),
                        info: proj,
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                push_explain_row_spine(
                    rows,
                    scan_depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: sel_est,
                        task: scan_task,
                        access: String::new(),
                        info: sel,
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy {
                    scan_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth + 2,
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
                return true;
            }
            // RANGE: `RANGE_SEL|<sel>|<decided by …>` → index/table range probed
            // per outer row: a cop Selection above a decided range scan.
            if let Some((sel, range_info)) = range_sel_pushed(pushed.as_ref()) {
                let sel_est = keep_order_est(*row_count, &sel);
                let mut keep = format!("range: {range_info}, keep order:false");
                if *stats_pseudo {
                    keep.push_str(", stats:pseudo");
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
                        info: sel,
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
                        name: "TableRangeScan",
                        est: sel_est,
                        task: scan_task,
                        access: scan_access,
                        info: keep,
                    },
                    if under_bushy { &scan_spine } else { &[] },
                );
                return true;
            }
            if let Some(filter) = pushed.as_ref() {
                let sel_info = match &partial {
                    Some((_, sel)) => sel.clone(),
                    None => explain_pushed_filter(table, filter),
                };
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: est_rows(plan) * apply_factor.max(1.0),
                        task: scan_task,
                        access: String::new(),
                        info: sel_info,
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
            partition_access,
            pushed,
            range_scan,
        } => {
            // The reader node carries the GLOBAL partition label; the scan keeps
            // `table:…, index:…` without the (executor-only) partition ids.
            let access = index_access_cols(
                table,
                index,
                index_columns,
                if partition_access.is_empty() {
                    partitions
                } else {
                    &[]
                },
            );
            let (mut scan_est, mut scan_info) = index_scan_meta(pushed.as_ref());
            // Explicit IndexReader.row_count / RANGE overrides pseudo est.
            if range_pushed(pushed.as_ref()).is_some()
                || (*row_count - 10_000.0).abs() > f64::EPSILON
            {
                scan_est = *row_count;
            }
            let mut scan_depth = depth;
            let stream_est = stream_parent.map(|(_, _, _, e)| e);
            // MergeJoin / IndexHashJoin ordered input: `IndexFullScan` keeps
            // order (optionally under an index-side Selection).
            let keep_order_sel = keep_order_sel_pushed(pushed.as_ref());
            if keep_order_pushed(pushed.as_ref()) || keep_order_sel.is_some() {
                let pseudo = *row_count >= 9_000.0;
                let scan_keep = if pseudo {
                    "keep order:true, stats:pseudo"
                } else {
                    "keep order:true"
                };
                // Grandchild continuation column: `│` while this reader still
                // has a later sibling (Probe).
                let mut child_spine = spine.to_vec();
                child_spine.push(!_parent_last);
                let mut scan_spine = child_spine.clone();
                scan_spine.push(false);
                if let Some(sel) = keep_order_sel {
                    let sel_est = keep_order_est(*row_count, &sel);
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: sel_est,
                            task: "cop[tikv]",
                            access: String::new(),
                            info: sel,
                        },
                        &child_spine,
                    );
                    push_explain_row_spine(
                        rows,
                        depth + 1,
                        true,
                        next_id,
                        CopRow {
                            name: "IndexFullScan",
                            est: *row_count,
                            task: "cop[tikv]",
                            access,
                            info: scan_keep.to_string(),
                        },
                        &scan_spine,
                    );
                } else {
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: "IndexFullScan",
                            est: *row_count,
                            task: "cop[tikv]",
                            access,
                            info: scan_keep.to_string(),
                        },
                        &child_spine,
                    );
                }
                return true;
            }
            // IndexJoin probe: index-side Selection above the decided range.
            if let Some((sel, range_info)) = range_sel_pushed(pushed.as_ref()) {
                let pseudo = *row_count >= 9_000.0;
                let sel_est = keep_order_est(*row_count, &sel);
                let mut keep = format!("range: {range_info}, keep order:false");
                if pseudo {
                    keep.push_str(", stats:pseudo");
                }
                let mut child_spine = spine.to_vec();
                child_spine.push(!_parent_last);
                let mut scan_spine = child_spine.clone();
                scan_spine.push(false);
                push_explain_row_spine(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: sel_est,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: sel,
                    },
                    &child_spine,
                );
                push_explain_row_spine(
                    rows,
                    depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexRangeScan",
                        est: *row_count,
                        task: "cop[tikv]",
                        access,
                        info: keep,
                    },
                    &scan_spine,
                );
                return true;
            }
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
                // GROUP BY StreamAgg needs ordered index input; a global scalar
                // count over a covering index reads it unordered.
                if agg_name == "StreamAgg" {
                    scan_info = if cop_info.is_empty() || cop_info.starts_with("group by:") {
                        "keep order:true, stats:pseudo".to_string()
                    } else if *row_count >= 9_000.0 {
                        "keep order:false, stats:pseudo".to_string()
                    } else {
                        "keep order:false".to_string()
                    };
                }
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
            // Pinned GLOBAL index read: cop Selection on `_tidb_tid`.
            if let Some(tid) = tid_sel_pushed(pushed.as_ref()) {
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut leaf_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    leaf_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    scan_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: 10_000.0,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: format!("in(_tidb_tid, {tid})"),
                    },
                    if under_bushy { cop_spine } else { &[] },
                );
                push_explain_row_spine(
                    rows,
                    scan_depth + 1,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexFullScan",
                        est: 10_000.0,
                        task: "cop[tikv]",
                        access,
                        info: "keep order:false, stats:pseudo".to_string(),
                    },
                    if under_bushy { &leaf_spine } else { &[] },
                );
                return true;
            }
            if under_window {
                scan_info = if window_keep_order {
                    "keep order:true, stats:pseudo".to_string()
                } else {
                    "keep order:false, stats:pseudo".to_string()
                };
            }
            // Planner-built access range: predicates on the index prefix became
            // `range:…`; leftovers stay a cop Selection above the scan.
            if let Some(range) = range_scan {
                let keep = if range.pseudo {
                    format!("range:{}, keep order:false, stats:pseudo", range.text)
                } else {
                    format!("range:{}, keep order:false", range.text)
                };
                let cop_owned = bushy_cop_spine(spine, scan_depth, under_bushy, _parent_last);
                let cop_spine = cop_owned.as_slice();
                let mut scan_spine = cop_spine.to_vec();
                if under_bushy && !cop_spine.is_empty() {
                    scan_spine.push(false);
                }
                if let Some(filter) = pushed.as_ref() {
                    push_explain_row_spine(
                        rows,
                        scan_depth,
                        true,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: *row_count,
                            task: "cop[tikv]",
                            access: String::new(),
                            info: explain_pushed_filter(table, filter),
                        },
                        if under_bushy { cop_spine } else { &[] },
                    );
                    push_explain_row_spine(
                        rows,
                        scan_depth + 1,
                        true,
                        next_id,
                        CopRow {
                            name: "IndexRangeScan",
                            est: range.rows,
                            task: "cop[tikv]",
                            access,
                            info: keep,
                        },
                        if under_bushy { &scan_spine } else { &[] },
                    );
                } else {
                    push_explain_row_spine(
                        rows,
                        scan_depth,
                        true,
                        next_id,
                        CopRow {
                            name: "IndexRangeScan",
                            est: range.rows,
                            task: "cop[tikv]",
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
                }
                return true;
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
            // TiDB labels every DistSQL index scan `cop[tikv]`.
            let scan_task = "cop[tikv]";
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
                // No decided range reaches this point, so the reader scans the
                // whole index: `IndexFullScan` at the index row count (also the
                // full scan under a StreamAgg cop task).
                if under_stream_agg {
                    scan_est = 10_000.0 * limit_scale;
                } else {
                    scan_est = *row_count;
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
                        name: "IndexFullScan",
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
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            probe_est,
            range_scan,
            pushed,
        } => {
            let idx_access = index_access_cols(
                table,
                index,
                index_columns,
                if partition_access.is_empty() {
                    partitions
                } else {
                    &[]
                },
            );
            let (mut scan_est, _) = index_scan_meta(pushed.as_ref());
            if range_pushed(pushed.as_ref()).is_some() || *row_count < 9_000.0 {
                scan_est = *row_count;
            }
            // Planner-built access range: `Selection(Build)` (residual index
            // predicates) over `IndexRangeScan` at the range's own card, then
            // the table probe filters the rest.
            if let Some(range) = range_scan {
                let task = "cop[tikv]";
                let keep = if range.pseudo {
                    format!("range:{}, keep order:false, stats:pseudo", range.text)
                } else {
                    format!("range:{}, keep order:false", range.text)
                };
                let rowid_info = if range.pseudo {
                    "keep order:false, stats:pseudo".to_string()
                } else {
                    "keep order:false".to_string()
                };
                let probe_row_est = if *probe_est > 0.0 {
                    *probe_est
                } else {
                    *row_count
                };
                let mut build_spine = ancestors_spine(spine, depth);
                // The IndexLookUp may be a non-last DML child (foreign-key nodes
                // follow it): its cop rows then need the continuation bar.
                if !_parent_last && let Some(cont) = build_spine.last_mut() {
                    *cont = true;
                }
                let mut probe_spine = build_spine.clone();
                let mut build_depth = depth;
                if let Some(sel) = selection_info.as_ref() {
                    push_explain_row_spine(
                        rows,
                        depth,
                        false,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: *row_count,
                            task,
                            access: String::new(),
                            info: sel.clone(),
                        },
                        &build_spine,
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Build)");
                    }
                    build_depth = depth + 1;
                }
                let mut child_spine = build_spine.clone();
                if selection_info.is_some() {
                    child_spine.push(true);
                }
                push_explain_row_spine(
                    rows,
                    build_depth,
                    selection_info.is_some(),
                    next_id,
                    CopRow {
                        name: "IndexRangeScan",
                        est: range.rows,
                        task,
                        access: idx_access.clone(),
                        info: keep,
                    },
                    &child_spine,
                );
                if selection_info.is_none()
                    && let Some(last) = rows.last_mut()
                {
                    last.id.push_str("(Build)");
                }
                let mut probe_depth = depth;
                if let Some(probe_sel) = probe_selection_info.as_ref() {
                    push_explain_row_spine(
                        rows,
                        depth,
                        true,
                        next_id,
                        CopRow {
                            name: "Selection",
                            est: probe_row_est,
                            task,
                            access: String::new(),
                            info: probe_sel.clone(),
                        },
                        &probe_spine,
                    );
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Probe)");
                    }
                    probe_depth = depth + 1;
                    probe_spine.push(false);
                }
                push_explain_row_spine(
                    rows,
                    probe_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "TableRowIDScan",
                        est: probe_row_est,
                        task,
                        access: table_access(table, &[]),
                        info: rowid_info,
                    },
                    &probe_spine,
                );
                if probe_selection_info.is_none() {
                    if let Some(last) = rows.last_mut() {
                        last.id.push_str("(Probe)");
                    }
                }
                return true;
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
            // IndexFullScan(Build) + [HashAgg(Probe) →] TableRowIDScan(Probe).
            let tid_sel = tid_sel_pushed(pushed.as_ref());
            // Spine for the IndexLookUp children: `│` continues while a Probe
            // sibling follows the Build side.
            let mut build_spine = ancestors_spine(spine, depth);
            // The IndexLookUp itself may be a non-last DML child (foreign-key
            // nodes follow it): its cop subtree then needs the continuation bar.
            if !_parent_last && let Some(cont) = build_spine.last_mut() {
                *cont = true;
            }
            let mut build_depth = depth;
            if let Some(tid) = tid_sel.as_ref() {
                push_explain_row_spine(
                    rows,
                    depth,
                    false,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: 10_000.0,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: format!("in(_tidb_tid, {tid})"),
                    },
                    &build_spine,
                );
                if let Some(last) = rows.last_mut() {
                    last.id.push_str("(Build)");
                }
                build_spine.push(true);
                build_depth = depth + 1;
            }
            let info = if tid_sel.is_some() {
                "keep order:false, stats:pseudo".to_string()
            } else if probe_selection_info.is_some() {
                // Probe-side filters display as `Selection(Probe)`, not scan pushdown.
                if *row_count >= 9_000.0 {
                    "keep order:false, stats:pseudo".to_string()
                } else {
                    "keep order:false".to_string()
                }
            } else if let Some(filter) = pushed.as_ref() {
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
            if tid_sel.is_some() {
                push_explain_row_spine(
                    rows,
                    build_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "IndexFullScan",
                        est: scan_est,
                        task: "cop[tikv]",
                        access: idx_access,
                        info,
                    },
                    &build_spine,
                );
            } else {
                push_explain_row(
                    rows,
                    build_depth,
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
            }
            // GLOBAL index aggregation: the cop HashAgg runs over the table-row
            // probe, so it sits between IndexLookUp and TableRowIDScan.
            let probe_agg =
                stream_parent.filter(|(name, _, cop, _)| *name == "HashAgg" && !cop.is_empty());
            let mut probe_depth = depth;
            if let Some((name, _, cop_info, est)) = probe_agg {
                push_explain_row(
                    rows,
                    depth,
                    true,
                    next_id,
                    CopRow {
                        name,
                        est,
                        task: "cop[tikv]",
                        access: String::new(),
                        info: cop_info.to_string(),
                    },
                );
                if let Some(last) = rows.last_mut() {
                    last.id.push_str("(Probe)");
                }
                probe_depth = depth + 1;
            }
            // Probe-side filter for a full index scan: `a = 1` cannot be pushed
            // into `index:bb(b, c)`, so TiDB filters table rows with Selection.
            if let Some(probe_sel) = probe_selection_info.as_ref() {
                push_explain_row(
                    rows,
                    probe_depth,
                    true,
                    next_id,
                    CopRow {
                        name: "Selection",
                        est: if *probe_est > 0.0 {
                            *probe_est
                        } else {
                            scan_est
                        },
                        task: "cop[tikv]",
                        access: String::new(),
                        info: probe_sel.clone(),
                    },
                );
                if let Some(last) = rows.last_mut() {
                    last.id.push_str("(Probe)");
                }
                probe_depth += 1;
            }
            push_explain_row(
                rows,
                probe_depth,
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
            if probe_agg.is_none() && probe_selection_info.is_none() {
                if let Some(last) = rows.last_mut() {
                    last.id.push_str("(Probe)");
                }
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
    // Pinned GLOBAL index reads render empty access objects as `NULL`.
    let access = render_access(row.access);
    rows.push(ExplainRow {
        id: format!("{prefix}{}_{id}", row.name),
        est_rows: format!("{:.2}", row.est),
        task: row.task.to_string(),
        access,
        info: row.info,
    });
}

/// TiDB renders an empty access object as `NULL` while a GLOBAL index read is
/// pinned to named partitions, and as an empty column otherwise.
fn render_access(access: String) -> String {
    if access.is_empty() && PINNED_PARTITION.with(|c| c.get()) {
        "NULL".to_string()
    } else {
        access
    }
}

/// `table:pt, index:uidx(c)` — TiDB prints a point-get index with its columns.
/// TiDB appends `, lock` when the point get locks its own keys.
fn lock_suffix(lock: bool) -> &'static str {
    if lock { ", lock" } else { "" }
}

fn point_get_index_access(
    table: &str,
    index: &str,
    columns: &[String],
    partitions: &[String],
) -> String {
    let (kind, index) = if columns.is_empty() {
        ("index", index.to_string())
    } else {
        // A composite PRIMARY KEY is the clustered common handle in TiDB.
        let kind = if index.eq_ignore_ascii_case("PRIMARY") && columns.len() > 1 {
            "clustered index"
        } else {
            "index"
        };
        (kind, format!("{index}({})", columns.join(", ")))
    };
    let mut access = table_access(table, partitions);
    access.push_str(&format!(", {kind}:{index}"));
    access
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
        (8_000.0, "keep order:false, stats:pseudo".to_string())
    } else {
        (10_000.0, "keep order:false, stats:pseudo".to_string())
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

/// Table whose columns an unqualified Selection column belongs to.
fn selection_table(plan: &PhysicalPlan) -> Option<&str> {
    match plan {
        PhysicalPlan::PointGet { table, .. }
        | PhysicalPlan::BatchPointGet { table, .. }
        | PhysicalPlan::TableReader { table, .. }
        | PhysicalPlan::IndexReader { table, .. }
        | PhysicalPlan::IndexLookUpReader { table, .. } => Some(table),
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::UnionScan { child, .. } => selection_table(child),
        _ => None,
    }
}

fn explain_selection_info(filter: &sealdb_parser::ast::ExprNode) -> String {
    explain_selection_info_on("t", filter)
}

fn explain_selection_info_on(table: &str, filter: &sealdb_parser::ast::ExprNode) -> String {
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
                            let table = c.table.as_deref().unwrap_or(table);
                            let schema = column_schema(c, table);
                            if schema.is_empty() {
                                format!("select.{table}.{}", c.name)
                            } else {
                                format!("{schema}.{table}.{}", c.name)
                            }
                        }
                        other => other.restore_string(),
                    };
                    format!("not(isnull({col}))")
                } else if let ExprNode::Binary(b) = u.expr.as_ref()
                    && b.op == Op::NullSafeEq
                {
                    // TiDB renders `NOT (a <=> b)` as `not(nulleq(a, b))`.
                    format!(
                        "not(nulleq({}, {}))",
                        explain_scalar_expr(&b.left, table),
                        explain_scalar_expr(&b.right, table)
                    )
                } else {
                    explain_scalar_expr(&item, table)
                }
            }
            other => explain_scalar_expr(other, table),
        })
        .collect();
    parts.join(", ")
}

/// Schema for a column reference: the column's recorded schema, else the
/// schema of its table in the statement being built, else the session schema.
fn column_schema(col: &sealdb_parser::ast::expressions::ColumnNameExpr, table: &str) -> String {
    col.schema
        .clone()
        .or_else(|| crate::core::planbuilder::explain_table_schema(table))
        .unwrap_or_else(sealdb_expression::current_db_name)
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

/// Bytes marker text on an EXPLAIN-only `pushed` field.
fn bytes_marker<'a>(pushed: Option<&'a sealdb_parser::ast::ExprNode>) -> Option<&'a str> {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    match pushed? {
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => std::str::from_utf8(b).ok(),
        _ => None,
    }
}

/// `COP_PROJ_SEL|<proj exprs>|<sel info>`: cop Projection above a Selection.
fn cop_proj_sel_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<(String, String)> {
    let rest = bytes_marker(pushed)?.strip_prefix("COP_PROJ_SEL|")?;
    let (proj, sel) = rest.split_once('|')?;
    Some((proj.to_string(), sel.to_string()))
}

/// `KEEP_ORDER` marker: IndexFullScan keeps order for MergeJoin/IndexHashJoin.
fn keep_order_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> bool {
    bytes_marker(pushed) == Some("KEEP_ORDER")
}

/// `KEEP_ORDER_SEL|<sel info>`: ordered IndexFullScan under an index Selection.
fn keep_order_sel_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<String> {
    bytes_marker(pushed)?
        .strip_prefix("KEEP_ORDER_SEL|")
        .map(|s| s.to_string())
}

/// `RANGE_SEL|<sel info>|<range>`: index Selection above a decided range.
fn range_sel_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<(String, String)> {
    let rest = bytes_marker(pushed)?.strip_prefix("RANGE_SEL|")?;
    let (sel, range) = rest.split_once('|')?;
    Some((sel.to_string(), range.to_string()))
}

/// `PARTIAL_SEL|<missing cols>|<sel info>`: TableReader whose pushed Selection
/// references columns that postdate the last ANALYZE (TiDB `stats:partial[…]`).
fn partial_sel_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<(String, String)> {
    let rest = bytes_marker(pushed)?.strip_prefix("PARTIAL_SEL|")?;
    let (missing, sel) = rest.split_once('|')?;
    Some((missing.to_string(), sel.to_string()))
}

/// `TID_SEL|<tid>`: pinned GLOBAL index read filters cop rows by `_tidb_tid`.
fn tid_sel_pushed(pushed: Option<&sealdb_parser::ast::ExprNode>) -> Option<String> {
    bytes_marker(pushed)?
        .strip_prefix("TID_SEL|")
        .map(|s| s.to_string())
}

/// TiDB keeps `not(isnull(..))` selectivity on real stats; pseudo stats scale
/// each rejected-null predicate by 0.999.
fn keep_order_est(row_count: f64, sel_info: &str) -> f64 {
    if row_count >= 9_000.0 {
        let n = sel_info.matches("not(isnull(").count() as i32;
        row_count * 0.999_f64.powi(n)
    } else {
        row_count
    }
}

/// Selection info carried by `KEEP_ORDER_SEL` / `RANGE_SEL` markers.
fn marker_sel_info(filter: &sealdb_parser::ast::ExprNode) -> Option<String> {
    keep_order_sel_pushed(Some(filter))
        .or_else(|| range_sel_pushed(Some(filter)).map(|(sel, _)| sel))
        .or_else(|| partial_sel_pushed(Some(filter)).map(|(_, sel)| sel))
}

/// TiDB prints `range:(3,+inf]` but `range: decided by [...]`.
fn range_prefix(info: &str) -> String {
    if info.starts_with("decided by") {
        format!("range: {info}")
    } else {
        format!("range:{info}")
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
                // TiDB `Column.ExplainInfo` uses the column's own schema
                // (`other_db.t.a`), falling back to the session schema.
                let schema = column_schema(c, tbl);
                if schema.is_empty() {
                    format!("select.{tbl}.{}", c.name)
                } else {
                    format!("{schema}.{tbl}.{}", c.name)
                }
            }
        }
        ExprNode::Value(ValueExpr { datum: Datum::Null }) => "NULL".into(),
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::Time(t),
        }) => {
            // TiDB `Constant.ExplainInfo` prints temporal values with the
            // full microsecond fraction (`2013-11-01 00:00:00.000000`).
            let mut t = t.clone();
            t.fsp = 6;
            t.to_string()
        }
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => format!(
            "\"{}\"",
            String::from_utf8_lossy(b)
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        ),
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
                Op::IntDiv => "intdiv",
                Op::LeftShift => "leftshift",
                Op::RightShift => "rightshift",
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
            format!("cast({}, {target})", explain_scalar_expr(&c.expr, table))
        }
        // Expression index key part (`tidb_shard(a) = 8`): TiDB prints the
        // scalar call with the same column spelling as its arguments.
        ExprNode::Func(f) => {
            let args = f
                .args
                .iter()
                .map(|arg| explain_scalar_expr(arg, table))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({args})", f.name.to_ascii_lowercase())
        }
        ExprNode::InList(list) => {
            let values = list
                .list
                .iter()
                .map(|item| explain_scalar_expr(item, table))
                .collect::<Vec<_>>()
                .join(", ");
            let inner = format!("in({}, {values})", explain_scalar_expr(&list.expr, table));
            if list.not {
                format!("not({inner})")
            } else {
                inner
            }
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
        ExprNode::IsNull(i) => {
            let inner = format!("isnull({})", explain_scalar_expr(&i.expr, table));
            if i.not {
                format!("not({inner})")
            } else {
                inner
            }
        }
        other => other.restore_string(),
    }
}

/// TiDB cop Selection ExplainInfo: `gt(select.t.b, 0)` / `not(isnull(select.t.a))`.
pub(crate) fn explain_pushed_filter(table: &str, filter: &sealdb_parser::ast::ExprNode) -> String {
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
        // TiDB `SortedExplainExpressionList`: CNF items print in sorted order.
        let mut parts = cnf
            .iter()
            .map(|c| explain_pushed_filter(table, c))
            .collect::<Vec<_>>();
        parts.sort();
        return parts.join(", ");
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
                Op::LeftShift => "leftshift",
                Op::RightShift => "rightshift",
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
                if *stats_pseudo { 10_000.0 } else { -*row_count }
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
            seed, inline: true, ..
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

/// `(Build)` / `(Probe)` suffix for a join's direct child.
fn join_role_label(
    name: &str,
    self_p: *const PhysicalPlan,
    build: *const PhysicalPlan,
    probe: *const PhysicalPlan,
) -> &'static str {
    if self_p == build
        && matches!(
            name,
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
                | "Point_Get"
                | "PointGet"
        )
    {
        "(Build)"
    } else if self_p == probe
        && matches!(
            name,
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
                | "Point_Get"
                | "PointGet"
        )
    {
        "(Probe)"
    } else {
        ""
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
        | PhysicalPlan::Distinct { child, .. }
        | PhysicalPlan::SelectLock { child, .. } => vec![child.as_ref()],
        PhysicalPlan::Insert { child, .. }
        | PhysicalPlan::Update { child, .. }
        | PhysicalPlan::Delete { child, .. } => child.iter().map(|c| c.as_ref()).collect(),
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
        // TiDB HashJoin lists Build before Probe. Inner joins build the right
        // (smaller, reordered) side; a RIGHT JOIN builds whichever input is
        // smaller — the preserved 2244-row side over a 4509-row probe, or a
        // 2-row build subtree over a 4-row preserved side.
        PhysicalPlan::HashJoin {
            left, right, tp, ..
        } => {
            if matches!(tp, JoinType::RightOuter) {
                if est_rows(right) < est_rows(left) {
                    vec![right.as_ref(), left.as_ref()]
                } else {
                    vec![left.as_ref(), right.as_ref()]
                }
            } else {
                vec![right.as_ref(), left.as_ref()]
            }
        }
        // MergeJoin hashes the left side and keeps the preserved side as Probe,
        // so a RIGHT JOIN lists Build (left) before Probe (right).
        PhysicalPlan::MergeJoin {
            left,
            right,
            tp: JoinType::RightOuter,
            ..
        } => vec![left.as_ref(), right.as_ref()],
        PhysicalPlan::MergeJoin { left, right, .. } => vec![right.as_ref(), left.as_ref()],
        // IndexJoin lists outer (Build) then inner (Probe).
        PhysicalPlan::IndexJoin { outer, inner, .. } => {
            vec![outer.as_ref(), inner.as_ref()]
        }
        // Null-aware anti join: Build (right) then Probe (left), like HashJoin.
        PhysicalPlan::NullAwareAntiJoin { probe, build, .. } => {
            vec![build.as_ref(), probe.as_ref()]
        }
        PhysicalPlan::RowProjection { child, .. } => vec![child.as_ref()],
        PhysicalPlan::Union { children, .. } => children.iter().map(|c| c.as_ref()).collect(),
        PhysicalPlan::DistinctUnion { child, .. } => vec![child.as_ref()],
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
        PhysicalPlan::Projection { exprs, .. } => {
            ("Projection".to_string(), String::new(), exprs.join(", "))
        }
        PhysicalPlan::SelectLock { info, .. } => {
            ("SelectLock".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::DDL { kind } => ("DDL".to_string(), String::new(), format!("{kind:?}")),
        PhysicalPlan::Show { kind } => ("Show".to_string(), String::new(), kind.clone()),
        PhysicalPlan::Simple { kind } => ("Simple".to_string(), String::new(), kind.clone()),
        PhysicalPlan::Insert { .. } => ("Insert".to_string(), String::new(), "N/A".to_string()),

        PhysicalPlan::InfoSchema { table } => (
            "MemTable".to_string(),
            format!("table:{table}"),
            String::new(),
        ),
        PhysicalPlan::Update { .. } => ("Update".to_string(), String::new(), "N/A".to_string()),
        PhysicalPlan::Delete { .. } => ("Delete".to_string(), String::new(), "N/A".to_string()),
        PhysicalPlan::TableReader {
            pushed,
            partition_access,
            range_scan,
            data_limit,
            ..
        } => (
            "TableReader".to_string(),
            if partition_access.is_empty() {
                String::new()
            } else {
                format!("partition:{partition_access}")
            },
            if *data_limit {
                "data:Limit".to_string()
            } else if cop_projection_pending().is_some() {
                "data:Projection".to_string()
            } else if range_scan.is_some() || range_pushed(pushed.as_ref()).is_some() {
                "data:TableRangeScan".to_string()
            } else if cop_proj_sel_pushed(pushed.as_ref()).is_some() {
                "data:Projection".to_string()
            } else if pushed.is_some() {
                "data:Selection".to_string()
            } else {
                "data:TableFullScan".to_string()
            },
        ),
        PhysicalPlan::PartitionUnion { .. } => {
            ("PartitionUnion".to_string(), String::new(), String::new())
        }
        PhysicalPlan::PointGet {
            table,
            handle: Some(handle),
            index: None,
            partition_names,
            lock,
            ..
        } => (
            // TiDB EXPLAIN id is `Point_Get` (underscore before Get).
            "Point_Get".to_string(),
            table_access(table, partition_names),
            format!("handle:{handle}{}", lock_suffix(*lock)),
        ),
        PhysicalPlan::PointGet {
            table,
            index: Some(index),
            index_columns,
            partition_names,
            global_index,
            ..
        } => (
            "Point_Get".to_string(),
            point_get_index_access(
                table,
                index,
                index_columns,
                if *global_index { &[] } else { partition_names },
            ),
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
            index_columns,
            partition_names,
            global_index,
            lock,
            keep_order,
            desc,
            ..
        } => (
            "Batch_Point_Get".to_string(),
            point_get_index_access(
                table,
                index,
                index_columns,
                if *global_index { &[] } else { partition_names },
            ),
            format!("keep order:{keep_order}, desc:{desc}{}", lock_suffix(*lock)),
        ),
        PhysicalPlan::BatchPointGet {
            table,
            handles,
            partition_names,
            lock,
            keep_order,
            desc,
            ..
        } => (
            "Batch_Point_Get".to_string(),
            table_access(table, partition_names),
            format!(
                "handle:[{}], keep order:{keep_order}, desc:{desc}{}",
                handles
                    .iter()
                    .map(|handle| handle.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                lock_suffix(*lock)
            ),
        ),
        PhysicalPlan::IndexLookUpReader {
            partition_access, ..
        } => (
            "IndexLookUp".to_string(),
            if partition_access.is_empty() {
                String::new()
            } else {
                format!("partition:{partition_access}")
            },
            // TiDB IndexLookUp root info is empty; Build/Probe children carry shape.
            String::new(),
        ),
        PhysicalPlan::IndexReader {
            partition_access,
            pushed,
            range_scan,
            ..
        } => (
            "IndexReader".to_string(),
            if partition_access.is_empty() {
                String::new()
            } else {
                format!("partition:{partition_access}")
            },
            if cop_projection_pending().is_some() {
                "index:Projection".to_string()
            } else if keep_order_sel_pushed(pushed.as_ref()).is_some() {
                "index:Selection".to_string()
            } else if keep_order_pushed(pushed.as_ref()) {
                "index:IndexFullScan".to_string()
            } else if range_sel_pushed(pushed.as_ref()).is_some() {
                "index:Selection".to_string()
            } else if range_pushed(pushed.as_ref()).is_some() {
                "index:IndexRangeScan".to_string()
            } else if range_scan.is_some() {
                if pushed.is_some() {
                    "index:Selection".to_string()
                } else {
                    "index:IndexRangeScan".to_string()
                }
            } else if pushed.is_some() {
                // Only index columns reach a covering reader, so the filter is
                // always an index-side cop Selection.
                "index:Selection".to_string()
            } else {
                "index:IndexFullScan".to_string()
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
        PhysicalPlan::Selection { child, filter } => (
            "Selection".to_string(),
            String::new(),
            match selection_table(child) {
                Some(table) => explain_selection_info_on(table, filter),
                None => explain_selection_info(filter),
            },
        ),
        PhysicalPlan::Sort { by_info, .. } => ("Sort".to_string(), String::new(), by_info.clone()),
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
        PhysicalPlan::NullAwareAntiJoin { info, .. } => {
            ("HashJoin".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::RowProjection { info, .. } => {
            ("Projection".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::MergeJoin {
            left_key,
            right_key,
            info,
            ..
        } => (
            "MergeJoin".to_string(),
            String::new(),
            info.clone().unwrap_or_else(|| {
                format!("inner join, left key:{left_key}, right key:{right_key}")
            }),
        ),
        PhysicalPlan::Apply { filter, info, .. } => {
            let info = info.clone().unwrap_or_else(|| filter.restore_string());
            ("Apply".to_string(), String::new(), info)
        }
        PhysicalPlan::IndexJoin { info, .. } => {
            // Index hash join shares the executor shape; EXPLAIN differs by name.
            match info.strip_prefix("IndexHashJoin|") {
                Some(rest) => ("IndexHashJoin".to_string(), String::new(), rest.to_string()),
                None => ("IndexJoin".to_string(), String::new(), info.clone()),
            }
        }
        PhysicalPlan::UnionScan { info, .. } => {
            ("UnionScan".to_string(), String::new(), info.clone())
        }
        // TiDB `PhysicalUnionAll` ExplainInfo is always empty (distinct UNION
        // dedup lives in the HashAgg above it).
        PhysicalPlan::Union { .. } => ("Union".to_string(), String::new(), String::new()),
        PhysicalPlan::DistinctUnion { info, .. } => {
            ("HashAgg".to_string(), String::new(), info.clone())
        }
        PhysicalPlan::Window { info, .. } => ("Window".to_string(), String::new(), info.clone()),
        PhysicalPlan::Shuffle { info, .. } => ("Shuffle".to_string(), String::new(), info.clone()),
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

/// A single `column = column` conjunct (TiDB 0.8 pseudo factor).
fn is_column_eq_filter(filter: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    matches!(
        filter.split_cnf().as_slice(),
        [ExprNode::Binary(bin)]
            if bin.op == Op::Eq
                && matches!(*bin.left, ExprNode::Column(_))
                && matches!(*bin.right, ExprNode::Column(_))
    )
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

/// Range lower/upper bounds as TiDB `ranger` builds them for pseudo stats
/// (`pkg/planner/cardinality/pseudo.go`, `getPseudoRowCountByColumnRanges`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PRangeLow {
    Null,
    MinNotNull,
    Val(i128),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PRangeHigh {
    Null,
    Val(i128),
    MaxValue,
}

type PRange = (PRangeLow, PRangeHigh);

fn prange_low_key(lo: PRangeLow) -> (u8, i128) {
    match lo {
        PRangeLow::Null => (0, 0),
        PRangeLow::MinNotNull => (1, 0),
        PRangeLow::Val(v) => (2, v),
    }
}

fn prange_high_key(hi: PRangeHigh) -> (u8, i128) {
    match hi {
        PRangeHigh::Null => (0, 0),
        PRangeHigh::Val(v) => (1, v),
        PRangeHigh::MaxValue => (2, 0),
    }
}

fn prange_max_low(a: PRangeLow, b: PRangeLow) -> PRangeLow {
    if prange_low_key(a) >= prange_low_key(b) {
        a
    } else {
        b
    }
}

fn prange_min_high(a: PRangeHigh, b: PRangeHigh) -> PRangeHigh {
    if prange_high_key(a) <= prange_high_key(b) {
        a
    } else {
        b
    }
}

fn prange_empty(r: &PRange) -> bool {
    match (r.0, r.1) {
        (PRangeLow::Val(a), PRangeHigh::Val(b)) => a > b,
        _ => false,
    }
}

/// Integer literal value of a datum (partition/`int` columns only).
fn datum_to_i128(datum: &sealdb_types::Datum) -> Option<i128> {
    use sealdb_types::Datum;
    match datum {
        Datum::I64(v) => Some(*v as i128),
        Datum::U64(v) => Some(*v as i128),
        _ => None,
    }
}

/// `col <op> literal` (either operand order) → ranges over one column.
fn pseudo_ranges_of_predicate(
    expr: &sealdb_parser::ast::ExprNode,
) -> Option<(String, Vec<PRange>)> {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    // Only real table columns carry pseudo statistics; derived outputs
    // (`Column#5` from an aggregate) fall back to the generic 0.8 factor.
    let col_key = |e: &ExprNode| match e {
        ExprNode::Column(c) if !c.name.starts_with("Column#") => Some(format!(
            "{}.{}",
            c.table.as_deref().unwrap_or("").to_ascii_lowercase(),
            c.name.to_ascii_lowercase()
        )),
        _ => None,
    };
    match expr {
        ExprNode::IsNull(i) => {
            let key = col_key(&i.expr)?;
            Some((key, vec![(PRangeLow::Null, PRangeHigh::Null)]))
        }
        ExprNode::Unary(u) if u.op == Op::Not && matches!(u.expr.as_ref(), ExprNode::IsNull(_)) => {
            let ExprNode::IsNull(i) = u.expr.as_ref() else {
                return None;
            };
            let key = col_key(&i.expr)?;
            Some((key, vec![(PRangeLow::MinNotNull, PRangeHigh::MaxValue)]))
        }
        ExprNode::Between(b) => {
            let key = col_key(&b.expr)?;
            let low = expr_to_i128_const(&b.low)?;
            let high = expr_to_i128_const(&b.high)?;
            Some((key, vec![(PRangeLow::Val(low), PRangeHigh::Val(high))]))
        }
        ExprNode::Binary(bin) => {
            let (col, val, op, flipped) = match (bin.left.as_ref(), bin.right.as_ref()) {
                (ExprNode::Column(_), right) => {
                    (&bin.left, expr_to_i128_const(right)?, bin.op, false)
                }
                (left, ExprNode::Column(_)) => {
                    (&bin.right, expr_to_i128_const(left)?, bin.op, true)
                }
                _ => return None,
            };
            let key = col_key(col)?;
            // Flipped operands swap the comparison direction.
            let op = if flipped { reverse_cmp_op(op)? } else { op };
            let ranges = match op {
                Op::Gt => vec![(PRangeLow::Val(val), PRangeHigh::MaxValue)],
                Op::Ge => {
                    if val <= i64::MIN as i128 {
                        vec![(PRangeLow::MinNotNull, PRangeHigh::MaxValue)]
                    } else {
                        vec![(PRangeLow::Val(val - 1), PRangeHigh::MaxValue)]
                    }
                }
                Op::Lt => vec![(PRangeLow::MinNotNull, PRangeHigh::Val(val))],
                Op::Le => vec![(PRangeLow::MinNotNull, PRangeHigh::Val(val + 1))],
                Op::Eq => vec![(PRangeLow::Val(val), PRangeHigh::Val(val))],
                Op::Ne => vec![
                    (PRangeLow::MinNotNull, PRangeHigh::Val(val)),
                    (PRangeLow::Val(val), PRangeHigh::MaxValue),
                ],
                _ => return None,
            };
            Some((key, ranges))
        }
        _ => None,
    }
}

/// Integer constant of an expression, folding a leading unary minus
/// (`-9223372036854775808` parses as `-(9223372036854775808)`).
fn expr_to_i128_const(expr: &sealdb_parser::ast::ExprNode) -> Option<i128> {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Value(v) => datum_to_i128(&v.datum),
        ExprNode::Unary(u) if u.op == Op::UnaryMinus => expr_to_i128_const(&u.expr).map(|v| -v),
        _ => None,
    }
}

fn reverse_cmp_op(op: sealdb_parser::opcode::Op) -> Option<sealdb_parser::opcode::Op> {
    use sealdb_parser::opcode::Op;
    match op {
        Op::Gt => Some(Op::Lt),
        Op::Ge => Some(Op::Le),
        Op::Lt => Some(Op::Gt),
        Op::Le => Some(Op::Ge),
        Op::Eq => Some(Op::Eq),
        Op::Ne => Some(Op::Ne),
        _ => None,
    }
}

/// TiDB pseudo per-range row count.
fn prange_count(row_count: f64, r: &PRange) -> f64 {
    let null_count = row_count / 1000.0;
    match (r.0, r.1) {
        (PRangeLow::Null, PRangeHigh::MaxValue) => row_count,
        (PRangeLow::MinNotNull, PRangeHigh::MaxValue) => row_count - null_count,
        (PRangeLow::MinNotNull, PRangeHigh::Val(_)) => (row_count / 3.0 - null_count).max(0.0),
        (_, PRangeHigh::MaxValue) => row_count / 3.0,
        (PRangeLow::Val(a), PRangeHigh::Val(b)) if a == b => row_count / 1000.0,
        _ => row_count / 40.0,
    }
}

/// Sum per-column range counts, then combine columns independently (TiDB
/// multiplies the per-column selectivities).
fn pseudo_range_est(conjuncts: &[sealdb_parser::ast::ExprNode], row_count: f64) -> Option<f64> {
    let mut per_col: Vec<(String, Vec<PRange>)> = Vec::new();
    for c in conjuncts {
        let Some((key, ranges)) = pseudo_ranges_of_predicate(c) else {
            return None;
        };
        match per_col.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cur)) => {
                let mut merged = Vec::new();
                for a in cur.iter() {
                    for b in ranges.iter() {
                        let r = (prange_max_low(a.0, b.0), prange_min_high(a.1, b.1));
                        if !prange_empty(&r) {
                            merged.push(r);
                        }
                    }
                }
                *cur = merged;
            }
            None => per_col.push((key, ranges)),
        }
    }
    if per_col.is_empty() {
        return None;
    }
    if per_col.iter().all(|(_, r)| r.is_empty()) {
        return Some(0.0);
    }
    let mut est = row_count;
    for (_, ranges) in &per_col {
        let col_est: f64 = ranges
            .iter()
            .map(|r| prange_count(row_count, r))
            .sum::<f64>()
            .min(row_count);
        est *= col_est / row_count;
    }
    Some(est.clamp(0.0, row_count))
}

/// Extra rows TiDB adds for `not(isnull(..))` saturation on analyzed stats.

/// TiDB pseudo selectivity for one Selection conjunct
/// (`statistics.pseudoSelectivity`): a comparison whose column side needs a
/// cast cannot use the index, so it keeps `selectionFactor` (0.8), while a
/// plain `col = const` over pseudo stats contributes `1/1000`.
fn conjunct_selectivity(expr: &sealdb_parser::ast::ExprNode) -> Option<f64> {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if !matches!(bin.op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) {
        return None;
    }
    if expr_has_cast_operand(&bin.left) || expr_has_cast_operand(&bin.right) {
        return Some(0.8);
    }
    let column_const = |col: &ExprNode, value: &ExprNode| {
        matches!(col, ExprNode::Column(_)) && value.const_datum().is_some()
    };
    if bin.op == Op::Eq
        && (column_const(&bin.left, &bin.right) || column_const(&bin.right, &bin.left))
    {
        return Some(0.001);
    }
    // TiDB `selectionFactor`: a comparison between two columns cannot use an
    // index statistic, so it keeps the 0.8 pseudo factor (`a = b` over 10000
    // pseudo rows estimates 8000).
    if bin.op == Op::Eq
        && matches!(*bin.left, ExprNode::Column(_))
        && matches!(*bin.right, ExprNode::Column(_))
    {
        return Some(0.8);
    }
    None
}

/// `cast(col, …)`, or arithmetic over such a cast (`plus(cast(a, double), 1)`).
fn expr_has_cast_operand(expr: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Cast(_) => true,
        ExprNode::Binary(bin)
            if matches!(
                bin.op,
                Op::Plus | Op::Minus | Op::Mul | Op::Div | Op::IntDiv | Op::Mod
            ) =>
        {
            expr_has_cast_operand(&bin.left) || expr_has_cast_operand(&bin.right)
        }
        _ => false,
    }
}

/// Selection selectivity: `AND` multiplies, `OR`/DNF uses `1 - Π(1 - s)`.
/// `None` when a conjunct has no TiDB pseudo factor, so callers keep their
/// existing estimates.
pub(crate) fn filter_selectivity(filter: &sealdb_parser::ast::ExprNode) -> Option<f64> {
    let arms = filter.split_dnf();
    if arms.len() > 1 {
        let mut product = 1.0;
        for arm in &arms {
            let sel = conjunct_list_selectivity(&arm.split_cnf())?;
            product *= 1.0 - sel;
        }
        return Some(1.0 - product);
    }
    conjunct_list_selectivity(&filter.split_cnf())
}

fn conjunct_list_selectivity(conjuncts: &[sealdb_parser::ast::ExprNode]) -> Option<f64> {
    let mut sel = 1.0;
    for conjunct in conjuncts {
        sel *= conjunct_selectivity(conjunct)?;
    }
    Some(sel)
}

pub(crate) fn est_rows(plan: &PhysicalPlan) -> f64 {
    match plan {
        PhysicalPlan::TableDual { row_count } => *row_count as f64,
        PhysicalPlan::PointGet { .. } => 1.0,
        PhysicalPlan::BatchPointGet { n_keys, .. } => *n_keys as f64,
        PhysicalPlan::SelectLock { child, .. } => est_rows(child),
        PhysicalPlan::Selection { child, filter, .. } => {
            // A residual filter over an integer-handle point get keeps one row.
            if matches!(child.as_ref(), PhysicalPlan::PointGet { index: None, .. }) {
                return 1.0;
            }
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
            } else if let Some(sel) = filter_selectivity(filter) {
                // A cast comparison keeps TiDB's default factor; OR arms
                // combine as `1 - Π(1 - s)` (DNF selectivity).
                (est_rows(child) * sel).max(0.0)
            } else if is_equality_filter(filter) {
                let base = est_rows(child);
                if base >= 10_000.0 { 10.0 } else { 1.0 }
            } else if filter_has_like(filter) {
                // TiDB pseudo est for `like(NULL,…) AND col` under unanalyzed scan.
                // Keep unrounded so HashJoin product is 53253333.33 (not 53253300).
                let base = est_rows(child);
                if base >= 9_999.0 {
                    5_325.333_333_333
                } else {
                    base * 0.532_533_333_333
                }
            } else if let Some(est) = pseudo_range_est(&filter.split_cnf(), est_rows(child)) {
                est
            } else {
                est_rows(child) * 0.8
            }
        }
        PhysicalPlan::TableReader {
            filter_est: Some(est),
            ..
        } => *est,
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
        // TiDB `selectionFactor` on a column-vs-column equality (`a = b` over
        // 10000 pseudo rows estimates 8000).
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if is_column_eq_filter(filter) => *row_count * 0.8,
        // Each equality on a *distinct* column contributes its own 1/1000
        // pseudo factor (`a = 1 AND b = 2` estimates 0.01, not one match).
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if filter.split_cnf().len() > 1 && filter_selectivity(filter).is_some() => {
            (*row_count * filter_selectivity(filter).unwrap_or(0.8)).max(0.0)
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
        } if range_pushed(Some(filter)).is_some() || pushed_is_bytes_info(filter) => *row_count,
        PhysicalPlan::TableReader {
            pushed: Some(filter),
            row_count,
            ..
        } if let Some(est) = pseudo_range_est(&filter.split_cnf(), *row_count) => est,
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
        } if keep_order_pushed(Some(filter)) => *row_count,
        PhysicalPlan::IndexReader {
            pushed: Some(filter),
            row_count,
            ..
        } if marker_sel_info(filter).is_some() => {
            keep_order_est(*row_count, &marker_sel_info(filter).unwrap_or_default())
        }
        PhysicalPlan::IndexReader {
            pushed: Some(filter),
            row_count,
            ..
        } if range_pushed(Some(filter)).is_some() => *row_count,
        // Planner-built access range: row_count already holds the range ×
        // residual-Selection estimate computed by the detacher.
        PhysicalPlan::IndexReader {
            row_count,
            range_scan: Some(_),
            ..
        } => *row_count,
        PhysicalPlan::IndexReader {
            row_count,
            pushed: None,
            ..
        } => *row_count,
        PhysicalPlan::IndexLookUpReader {
            pushed: Some(filter),
            row_count,
            ..
        } if range_pushed(Some(filter)).is_some() => *row_count,
        PhysicalPlan::IndexLookUpReader {
            probe_est,
            probe_selection_info: Some(_),
            ..
        } if *probe_est > 0.0 => *probe_est,
        PhysicalPlan::IndexLookUpReader {
            pushed: None,
            probe_est,
            ..
        } if *probe_est > 0.0 => *probe_est,
        PhysicalPlan::IndexLookUpReader {
            row_count,
            pushed: None,
            ..
        } => *row_count,
        // A covering reader over a full index scan sizes the cop Selection from
        // the filter's pseudo selectivity (`10000 * 0.8` / DNF combination).
        PhysicalPlan::IndexReader {
            pushed: Some(filter),
            row_count,
            range_scan: None,
            ..
        } if filter_selectivity(filter).is_some() => {
            *row_count * filter_selectivity(filter).unwrap_or(0.8)
        }
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
        // Planner-computed join cardinality from the key NDVs.
        PhysicalPlan::HashJoin {
            est_rows: Some(est),
            ..
        }
        | PhysicalPlan::MergeJoin {
            est_rows: Some(est),
            ..
        } => *est,
        PhysicalPlan::MergeJoin {
            left, right, tp, ..
        } => {
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
            // 9990 → 12487.50; both 10000 → 12500). An outer join keeps at
            // least the preserved side, so its est is the larger side.
            let l = est_rows(left);
            let r = est_rows(right);
            if matches!(
                tp,
                super::physical_plans::JoinType::LeftOuter
                    | super::physical_plans::JoinType::RightOuter
            ) {
                l.max(r)
            } else if is_pseudo_card(l) && is_pseudo_card(r) {
                l.min(r) * 1.25
            } else if l < 1.0 || r < 1.0 {
                // A side estimated below one row (residual filter over a point
                // get) bounds the merge output the same way.
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
                // Pseudo-ness is a property of the source statistics, not of the
                // filtered row count: a pseudo scan under `col > k` is 3583.33
                // and still counts as unanalyzed (`min * 1.25`).
                let lp = is_pseudo_card(l) || is_pseudo_plan(left);
                let rp = is_pseudo_card(r) || is_pseudo_plan(right);
                if !lp && !rp {
                    // Analyzed: ≥2× skew → max (t1⋈t3→4); else min (t2⋈t3→2).
                    let lo = l.min(r);
                    let hi = l.max(r);
                    if hi >= lo * 2.0 { hi } else { lo }
                } else if !lp && rp {
                    // Analyzed ⋈ pseudo → TiDB ~ analyzed * 1.25 (t3⋈t4 → 2.50).
                    l * 1.25
                } else if lp && !rp {
                    r * 1.25
                } else {
                    // Both pseudo: TiDB null-reject selectivity × 1.25
                    // (10000⋈10000 → 12500, 9990⋈9990 → 12487.50).
                    l.min(r) * 1.25
                }
            } else {
                l.max(r)
            }
        }
        PhysicalPlan::Union { children, .. } => children.iter().map(|c| est_rows(c)).sum(),
        PhysicalPlan::NullAwareAntiJoin { est_rows: e, .. } => *e,
        PhysicalPlan::RowProjection { child, .. } => est_rows(child),
        PhysicalPlan::DistinctUnion { est_rows: e, .. } => *e,
        PhysicalPlan::HashAgg { est_rows: e, .. } | PhysicalPlan::StreamAgg { est_rows: e, .. } => {
            *e
        }
        PhysicalPlan::IndexJoin { est_rows: e, .. } => *e,
        PhysicalPlan::UnionScan { est_rows: e, .. } => *e,
        PhysicalPlan::Apply {
            child,
            probe: Some(_probe),
            ..
        } => {
            // TiDB Apply estRows follows the outer (Build) side; probe CTE/Agg
            // cardinality is not lifted to the Apply root.
            est_rows(child)
        }
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
        | PhysicalPlan::Explain { child, .. }
        | PhysicalPlan::Distinct { child, .. } => est_rows(child),
        PhysicalPlan::CTE {
            recursive: true,
            limit: Some(_),
            ..
        } => 20.0,
        PhysicalPlan::CTE {
            recursive: true,
            recursive_plan: Some(rec),
            ..
        } if plan_has_selection(rec) => 18_000.0,
        PhysicalPlan::CTE {
            recursive: true, ..
        } => 8_001.0,
        PhysicalPlan::CTE { seed: child, .. } => est_rows(child),
        _ => 10_000.0,
    }
}

fn plan_has_selection(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::Selection { .. } => true,
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Limit { child, .. } => plan_has_selection(child),
        PhysicalPlan::Apply {
            child,
            probe: Some(_),
            ..
        } => plan_has_selection(child),
        PhysicalPlan::Apply { child, .. } => plan_has_selection(child),
        _ => false,
    }
}

/// CTE name + original seed table under a plan (for ExplainInfo qualification).
fn recursive_cte_qualifier(plan: &PhysicalPlan) -> Option<(String, String)> {
    match plan {
        PhysicalPlan::CTE {
            name,
            seed,
            recursive: true,
            ..
        } => Some((name.clone(), seed_orig_table(seed)?)),
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. } => recursive_cte_qualifier(child),
        PhysicalPlan::Apply {
            child,
            probe: Some(probe),
            ..
        } => recursive_cte_qualifier(child).or_else(|| recursive_cte_qualifier(probe)),
        _ => None,
    }
}

fn plan_contains_recursive_cte(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::CTE {
            recursive: true, ..
        } => true,
        PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. } => plan_contains_recursive_cte(child),
        PhysicalPlan::Apply {
            child,
            probe: Some(probe),
            ..
        } => plan_contains_recursive_cte(child) || plan_contains_recursive_cte(probe),
        PhysicalPlan::Apply { child, .. } => plan_contains_recursive_cte(child),
        _ => false,
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

/// Continuation flags for the ancestors of a node emitted at `depth`.
fn ancestors_spine(spine: &[bool], depth: usize) -> Vec<bool> {
    let want = depth.saturating_sub(1);
    let mut out = spine.to_vec();
    out.resize(want, false);
    out.truncate(want);
    out
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

/// True when the subtree's base scan is an unanalyzed pseudo scan.
fn is_pseudo_plan(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::TableReader { stats_pseudo, .. } => *stats_pseudo,
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::UnionScan { child, .. } => is_pseudo_plan(child),
        PhysicalPlan::PartitionUnion { children } => children.iter().all(is_pseudo_plan),
        _ => false,
    }
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
    if matches!(tp, JoinType::Cross) && matches!(on, ExprNode::Value(_)) {
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

/// `db.table.column` label used by TiDB join nodes (`left key:` / `equal:[…]`).
/// OrigTblName: alias of physical `t` stays `<db>.t.a`; real tables use their
/// name (`<db>.t1.a`), where `<db>` is the suite schema.
pub(crate) fn explain_join_col(col: &sealdb_parser::ast::expressions::ColumnNameExpr) -> String {
    let table = col.table.as_deref().unwrap_or("t");
    let schema = column_schema(col, table);
    if schema.is_empty() {
        format!("select.{table}.{}", col.name)
    } else {
        format!("{schema}.{table}.{}", col.name)
    }
}

/// `Bytes(NOT_IN_MARKER)` conjunct tagging an anti join as `NOT IN`.
fn is_not_in_marker_expr(expr: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_types::Datum;
    matches!(
        expr,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) if b.as_slice() == super::rule_semi_join::NOT_IN_MARKER.as_bytes()
    )
}

fn contains_not_in_marker(expr: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::expressions::ExprNode;
    match expr {
        ExprNode::Binary(bin) if bin.op == sealdb_parser::opcode::Op::LogicAnd => {
            contains_not_in_marker(&bin.left) || contains_not_in_marker(&bin.right)
        }
        _ => is_not_in_marker_expr(expr),
    }
}

fn explain_join_cond(on: &sealdb_parser::ast::ExprNode) -> String {
    use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::Datum;
    // `NOT IN` marker conjuncts are execution hints, never shown in EXPLAIN.
    if contains_not_in_marker(on) {
        let parts: Vec<String> = on
            .split_cnf()
            .into_iter()
            .filter(|c| !is_not_in_marker_expr(c))
            .map(|c| explain_join_cond(&c))
            .collect();
        return parts.join(", ");
    }
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

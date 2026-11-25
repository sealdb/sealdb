/// predicate pushdown (TiDB `rule_predicate_push_down.go`).
/// CNF conjuncts without subqueries attach to TableReader / IndexLookUp /
/// IndexReader (coprocessor-filter analog). Join-local predicates push to each side;
/// outer-join inner-side filters stay residual. Completeness wave: TiKV
/// Coprocessor DAG / CopClient.
use super::physical_plans::{JoinType, PhysicalPlan};
use crate::core::planbuilder::unique_eq_rows;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::ExprNode;
use sealdb_parser::ast::expressions::BinaryOpExpr;
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

pub fn predicate_push_down(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    // TiDB `mysql.opt_rule_blacklist('predicate_push_down')`: the rule never
    // runs, so the filter stays a root `Selection` and the scan below it keeps
    // the table's full row count. `buildSelection` sizes that `Selection` with
    // the 0.8 `selectionFactor` because no range was derived for it.
    if super::blacklist::opt_rule_disabled("predicate_push_down") {
        let Some(filter) = ExprNode::compose_cnf(filter.split_cnf()) else {
            return plan;
        };
        let est_rows = super::explain::est_rows(&plan) * 0.8;
        return if filter.has_subquery() {
            PhysicalPlan::Apply {
                child: Box::new(plan),
                filter,
                probe: None,
                info: None,
            }
        } else {
            PhysicalPlan::Selection {
                child: Box::new(plan),
                filter,
                est_rows: Some(est_rows),
            }
        };
    }
    push_into(plan, filter.split_cnf(), is, current_db)
}

pub fn eq_column_consts(selection: Option<&ExprNode>) -> Vec<(String, Datum)> {
    selection
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.eq_column_const())
                .collect()
        })
        .unwrap_or_default()
}

/// Like [`eq_column_consts`], but an equality on an expression reports the
/// expression's canonical text (`tidb_shard(a) = 8` → `tidb_shard(a)`), which
/// is how an expression index part is pinned.
pub fn eq_key_consts(selection: Option<&ExprNode>) -> Vec<(String, Datum)> {
    selection
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.eq_key_const())
                .collect()
        })
        .unwrap_or_default()
}

fn push_into(
    plan: PhysicalPlan,
    conjuncts: Vec<ExprNode>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if conjuncts.is_empty() {
        return plan;
    }
    match plan {
        // An MVIndex merge keeps its `JSON_OVERLAPS` re-check at the merge root
        // (TiDB cannot push it into the cop task). The predicate pushdown has
        // already folded the WHERE conjuncts into the merge arms and the probe
        // `Selection`; wrapping another `Selection` would double-count them.
        PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        } if matches!(child.as_ref(), PhysicalPlan::IndexMerge { .. }) => PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        },
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            filter_est,
            range_scan,
            pushed,
            ..
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr, &table));
            // `c BETWEEN T::MIN AND T::MAX` covers the whole domain of a NOT
            // NULL column: it filters nothing, so TiDB folds it away and the
            // scan stays a bare TableFullScan.
            let table_info = statement_table_info(is, current_db, &table);
            let pushable: Vec<_> = pushable
                .into_iter()
                .filter(|expr| {
                    !table_info
                        .as_ref()
                        .is_some_and(|info| full_domain_conjunct(info, expr))
                })
                .map(|expr| expr.rewrite_row_in_dnf())
                .collect();
            // `pk = const` → PointGet (TiDB cte HASH_JOIN on PK CTE).
            if residual.is_empty()
                && pushed.is_none()
                && let Some(handle) = pk_eq_handle(is, current_db, &table, &pushable)
            {
                return PhysicalPlan::PointGet {
                    table,
                    handle: Some(handle),
                    handle_unsigned: false,
                    index: None,
                    index_columns: Vec::new(),
                    partition_names,
                    global_index: false,
                    lock: false,
                    common_handle: false,
                };
            }
            // Filters that arrive after the access path was decided (join-side
            // derived predicates, null rejects) ride *above* a handle range:
            // the `TableRangeScan` keeps the access cardinality and the cop
            // `Selection` filters it. Fold them into the EXPLAIN-only range
            // marker so the scan shape matches TiDB and re-derive the reader
            // estimate from the accumulated cop filters.
            let mut filter_est = filter_est;
            let mut range_scan = range_scan;
            if let Some(raw) = range_scan.as_deref() {
                // A marker with an access-time `SEL:` already describes its
                // residual filter; leave it alone. Any other marker (a bare
                // handle range, or our own `SCAN:` one) folds in the cop
                // filters that were pushed after the access path was decided
                // (join-side derived predicates, null rejects), so they render
                // as the cop `Selection` above the range.
                let mut parts = raw.split('\u{1}');
                let base = parts.next().unwrap_or(raw).to_string();
                // Keep the access-time markers the recompute does not own
                // (`PROJ:` above a rowid range).
                let mut preserved: Vec<&str> = Vec::new();
                let mut access_sel: Option<&str> = None;
                let mut consumed: Option<&str> = None;
                for part in parts {
                    if let Some(sel) = part.strip_prefix("SEL:") {
                        access_sel = Some(sel);
                    } else if let Some(range) = part.strip_prefix("CONSUMED:") {
                        consumed = Some(range);
                    } else if !part.starts_with("SCAN:") {
                        preserved.push(part);
                    }
                }
                let access_rows = raw
                    .split('\u{1}')
                    .find_map(|part| {
                        part.strip_prefix("SCAN:")
                            .and_then(|rows| rows.parse::<f64>().ok())
                    })
                    .or(filter_est)
                    .unwrap_or(row_count);
                let filters = and_opt(
                    ExprNode::compose_cnf(pushable.clone()).map(simplify_or_arms),
                    pushed.clone(),
                );
                if let Some(filters) = filters.as_ref() {
                    // The `TableRangeScan` handle is the clustered primary
                    // key, or the implicit `_tidb_rowid` of a heap table —
                    // `handle_column` falls back to the first integer column
                    // for the latter, which is *not* the seek key.
                    let handle = table_info.as_ref().map(|info| {
                        if info.pk_is_handle {
                            info.handle_column()
                                .map(|column| column.name.o().to_string())
                                .unwrap_or_else(|| "_tidb_rowid".to_string())
                        } else {
                            "_tidb_rowid".to_string()
                        }
                    });
                    // The `TableRangeScan` already encodes the handle
                    // conjuncts (`id BETWEEN …`); only the other cop
                    // filters belong to the `Selection` above it.
                    let mut above: Vec<ExprNode> = Vec::new();
                    let mut fresh = false;
                    for conjunct in filters.split_cnf() {
                        let rendered = super::explain::explain_pushed_filter(&table, &conjunct);
                        // The access range already enforces this conjunct.
                        if consumed.is_some_and(|range| range.contains(&rendered)) {
                            continue;
                        }
                        if handle.as_ref().is_some_and(|handle| {
                            sealdb_statistics::pseudo::conjunct_bound_sets(&conjunct)
                                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(handle))
                        }) {
                            continue;
                        }
                        if above
                            .iter()
                            .any(|seen| seen.restore_string() == conjunct.restore_string())
                        {
                            continue;
                        }
                        if !access_sel.is_some_and(|sel| sel.contains(&rendered)) {
                            fresh = true;
                        }
                        above.push(conjunct);
                    }
                    if fresh {
                        let sel = sealdb_statistics::pseudo::selectivity(&above, 1.0, None)
                            .unwrap_or(1.0);
                        filter_est = Some(access_rows * sel);
                        let mut info: Vec<String> = above
                            .iter()
                            .map(|expr| super::explain::explain_pushed_filter(&table, expr))
                            .collect();
                        info.sort();
                        let preserved = if preserved.is_empty() {
                            String::new()
                        } else {
                            format!("\u{1}{}", preserved.join("\u{1}"))
                        };
                        range_scan = Some(format!(
                            "{base}{preserved}\u{1}SCAN:{access_rows}\u{1}SEL:{}",
                            info.join(", ")
                        ));
                    }
                }
            }
            let pushed = and_opt(
                ExprNode::compose_cnf(pushable).map(simplify_or_arms),
                pushed,
            );
            // Fresh push onto a full scan: derive filter_est from the combined
            // cop filters (`or(eq(a, 1), 0)` → eq NDV = 10, not selectionFactor).
            // Prefer `explain::filter_selectivity` so builtins with a known
            // TiDB factor (`json_memberof` → 0.001) win over the generic
            // pseudo `selectionFactor` (0.8).
            let filter_est = if range_scan.is_none()
                && stats_pseudo
                && let Some(filter) = pushed.as_ref()
            {
                let rows = row_count.abs().max(1.0);
                // Recompute from the combined pushed filter so conflicting
                // `isnull ∧ ¬isnull` becomes 0 after a second push. Keep a
                // unique-eq `filter_est` (≤1) over pseudo NDV 10. Skip LIKE
                // so `est_rows` can use the 5325.33 like+col factor.
                if crate::core::explain::filter_has_like(filter) {
                    None
                } else if let Some(sel) =
                    crate::core::explain::filter_selectivity(filter).or_else(|| {
                        sealdb_statistics::pseudo::selectivity(&filter.split_cnf(), rows, None)
                    })
                {
                    let computed = rows * sel;
                    if computed < 1e-12 {
                        // `isnull ∧ ¬isnull` (and other empty ranges) win over
                        // a stale single-conjunct est.
                        Some(0.0)
                    } else if let Some(prev) = filter_est.filter(|p| *p > 0.0 && *p <= 100.0) {
                        // Unique / IN-list ranger est (1, 2, …) stays ahead of
                        // pseudo NDV products (0.01, 20, …).
                        Some(prev)
                    } else {
                        // Refresh partial pushes (`not(isnull)` → 9990) once
                        // richer filters arrive (`gt`+`ne` → 3583.33).
                        Some(computed)
                    }
                } else {
                    filter_est
                }
            } else {
                filter_est
            };
            wrap_residual(
                PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    data_limit,
                    filter_est,
                    // Newer conjuncts first (TiDB: pushed ON/like before view WHERE).
                    range_scan,
                    keep_order: false,
                    pushed,
                },
                residual,
            )
        }
        PhysicalPlan::PartitionUnion { children } => PhysicalPlan::PartitionUnion {
            children: children
                .into_iter()
                .map(|c| push_into(c, conjuncts.clone(), is, current_db))
                .collect(),
        },
        // TiDB builds `PhysicalUnionScan` after predicate pushdown, so the
        // table's conjuncts still reach the access path below it (the merge's
        // probe `Selection`); the UnionScan keeps re-applying them to the
        // uncommitted rows it merges in.
        PhysicalPlan::UnionScan { info, child, .. } => {
            // TiDB builds `PhysicalUnionScan` *after* predicate pushdown, so
            // the scan's filter shows the conjuncts pushed into this side
            // (including the join's `not(isnull(key))` on the null-supplying
            // build side). Keep the original `NOT BETWEEN` rendering.
            let mut info = info;
            let extra: Vec<String> = conjuncts
                .iter()
                .filter(|conjunct| {
                    matches!(
                        conjunct,
                        ExprNode::Unary(unary)
                            if unary.op == Op::Not
                                && matches!(
                                    unary.expr.as_ref(),
                                    ExprNode::IsNull(isnull) if !isnull.not
                                )
                    )
                })
                .map(|conjunct| super::explain::explain_union_scan_filter("t", conjunct))
                .collect();
            if !extra.is_empty() {
                // `info` is a comma-joined expression list whose items may
                // themselves contain `, ` (`not(and(ge(col, 1), le(col, 2)))`),
                // so never split it; append the new conjuncts instead.
                for filter in extra {
                    if info.is_empty() {
                        info = filter;
                    } else if !info.contains(&filter) {
                        info.push_str(", ");
                        info.push_str(&filter);
                    }
                }
            }
            let child = push_into(*child, conjuncts, is, current_db);
            let est_rows = super::explain::est_rows(&child);
            PhysicalPlan::UnionScan {
                info,
                est_rows,
                child: Box::new(child),
            }
        }
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            partition_names,
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            push_down,
            handle_offsets,
            pushed,
            range_scan,
            ..
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr, &table));
            // `c BETWEEN T::MIN AND T::MAX` covers the whole domain of a NOT
            // NULL column: it filters nothing, so TiDB folds it away and the
            // scan stays a bare TableFullScan.
            let table_info = statement_table_info(is, current_db, &table);
            // Every secondary index stores the row handle, so predicates on the
            // handle (or on a common-handle key column) are evaluated in the
            // index cop task and do not need the table probe.
            let handle_cols: Vec<String> = table_info
                .as_ref()
                .map(|info| {
                    if info.pk_is_handle {
                        info.handle_column()
                            .map(|c| vec![c.name.o().to_string()])
                            .unwrap_or_default()
                    } else if info.common_handle {
                        info.indices
                            .iter()
                            .find(|idx| idx.primary)
                            .map(|idx| idx.columns.iter().map(|c| c.name.o().to_string()).collect())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                })
                .unwrap_or_default();
            let pushable: Vec<_> = pushable
                .into_iter()
                .filter(|expr| {
                    !table_info
                        .as_ref()
                        .is_some_and(|info| full_domain_conjunct(info, expr))
                })
                .map(|expr| expr.rewrite_row_in_dnf())
                .collect();
            // A predicate TiDB cannot push into cop (`tidb_shard(a) = 8`) can
            // still become the index access range: the ranger detaches it from
            // the filter and the scan seeks.
            let mut residual = residual;
            let range_candidates: Vec<ExprNode> =
                pushable.iter().chain(residual.iter()).cloned().collect();
            // TiDB ranger: predicates on the index prefix become the access
            // range of the Build side, so `IndexLookUp` seeks instead of
            // scanning the whole index. The range keeps its own estRows while
            // leftover index predicates stay a `Selection(Build)`.
            let mut selection_info = selection_info;
            let mut row_count = row_count;
            let mut pushable = pushable;
            // A second pushdown must not drop the range the ranger already
            // picked (`not(isnull(key))` added by the join null rejection).
            let mut access_range = range_scan;
            if access_range.is_none()
                && pushed.is_none()
                && selection_info.is_none()
                && let Some(info) = table_info.as_ref()
                && let Some(index_info) = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(&index))
                && let Some(access) = super::access_range::index_access_range(
                    info,
                    index_info,
                    &range_candidates,
                    row_count,
                    super::planbuilder::statement_stats(&table).as_ref(),
                )
            {
                let consumed = |expr: &ExprNode| {
                    access
                        .consumed
                        .iter()
                        .any(|c| c.restore_string() == expr.restore_string())
                };
                pushable.retain(|expr| !consumed(expr));
                residual.retain(|expr| !consumed(expr));
                selection_info = None;
                row_count = access.range.rows;
                access_range = Some(access.range);
            }
            // A full index scan can only push predicates whose columns live in
            // the index (plus the row handle it stores); the rest filter the
            // probed table rows (Selection(Probe)).
            // A prefix index stores only `name(2)`, so a residual filter on the
            // prefix column (a `like` that merely seeded the access range) can
            // only be answered from the probed table row: `Selection(Probe)`.
            let prefix_cols: Vec<String> = table_info
                .as_ref()
                .and_then(|info| {
                    info.indices
                        .iter()
                        .find(|idx| idx.name.o().eq_ignore_ascii_case(&index))
                })
                .map(|idx| {
                    idx.columns
                        .iter()
                        .filter(|c| c.length != -1)
                        .map(|c| c.name.o().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let on_probe = |expr: &ExprNode| {
                let cols = expr.referenced_columns();
                cols.iter()
                    .any(|(_, col)| prefix_cols.iter().any(|p| p.eq_ignore_ascii_case(col)))
                    || !cols.iter().all(|(_, col)| {
                        index_columns.iter().any(|c| c.eq_ignore_ascii_case(col))
                            || handle_cols.iter().any(|c| c.eq_ignore_ascii_case(col))
                    })
            };
            let (index_side, probe_side): (Vec<_>, Vec<_>) =
                if pushed.is_some() && access_range.is_none() {
                    (pushable, Vec::new())
                } else {
                    pushable.into_iter().partition(|expr| !on_probe(expr))
                };
            // Build-side residual predicates (columns in the index but not
            // encoded by the range) stay a cop `Selection(Build)`. A full index
            // scan keeps its table cardinality on `IndexFullScan` and only the
            // Selection shrinks.
            if (access_range.is_some() || pushed.is_none())
                && let Some(filter) = ExprNode::compose_cnf(index_side.clone())
            {
                if access_range.is_some()
                    && let Some(sel) = crate::core::explain::filter_selectivity(&filter)
                {
                    row_count *= sel;
                }
                selection_info = Some(crate::core::explain::explain_pushed_filter(&table, &filter));
            }
            // The index scan feeds the build side, so its output cardinality is
            // what remains after the *index-side* residual only; probe-side
            // predicates must not shrink it. `IndexScanRange::filter_rows` was
            // filled by the ranger from every unused predicate, so drop the
            // probe share it folded in.
            if let Some(range) = access_range.as_mut() {
                range.filter_rows = if selection_info.is_some() {
                    row_count
                } else {
                    range.rows
                };
            }
            let probe_filter = if probe_side.is_empty() {
                None
            } else {
                ExprNode::compose_cnf(probe_side.clone())
            };
            let probe_base = access_range.as_ref().map_or(row_count, |range| range.rows);
            // A prefix-column predicate seeded the index range (`like(d, "ab%")`
            // → `["ab","ac")`), so `range.filter_rows` already accounts for it;
            // it must not shrink the probe estimate a second time.
            let probe_sel_side: Vec<ExprNode> = probe_side
                .iter()
                .filter(|expr| {
                    !expr
                        .referenced_columns()
                        .iter()
                        .any(|(_, col)| prefix_cols.iter().any(|p| p.eq_ignore_ascii_case(col)))
                })
                .cloned()
                .collect();
            let probe_est = probe_filter.as_ref().map_or(0.0, |filter| {
                if probe_sel_side.is_empty() {
                    return probe_base;
                }
                let probe_sel_filter = if probe_sel_side.len() == probe_side.len() {
                    filter.clone()
                } else {
                    ExprNode::compose_cnf(probe_sel_side.clone()).unwrap_or_else(|| filter.clone())
                };
                table_info
                    .as_ref()
                    .and_then(|info| unique_eq_rows(info, &probe_sel_filter))
                    .unwrap_or_else(|| {
                        // TiDB sizes the `TableRowIDScan`'s filter from the
                        // table's column statistics (`cardinality.Selectivity`),
                        // so `c3 < 1` keeps its range rate over the rows the
                        // index range produced. Analyzed statistics win over the
                        // pseudo rates (`not(isnull(not_null_col))` is 1.0, not
                        // the `9990/10000` MinNotNull pseudo estimate).
                        let sel = super::planbuilder::stats_filter_selectivity(
                            table_info,
                            super::planbuilder::statement_stats(&table).as_ref(),
                            &probe_sel_filter,
                        )
                        .or_else(|| {
                            sealdb_statistics::pseudo::selectivity(&probe_sel_side, 10_000.0, None)
                        })
                        .unwrap_or(1.0);
                        probe_base * sel
                    })
            });
            let probe_selection_info = probe_filter
                .as_ref()
                .map(|filter| crate::core::explain::explain_pushed_filter(&table, filter))
                .or(probe_selection_info);
            wrap_residual(
                PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: access_range,
                    push_down,
                    handle_offsets,
                    pushed: and_opt(
                        pushed,
                        ExprNode::compose_cnf(index_side.into_iter().chain(probe_side).collect()),
                    ),
                },
                residual,
            )
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            partition_names,
            partition_access,
            row_count,
            pushed,
            range_scan,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr, &table));
            // `c BETWEEN T::MIN AND T::MAX` covers the whole domain of a NOT
            // NULL column: it filters nothing, so TiDB folds it away and the
            // scan stays a bare TableFullScan.
            let table_info = statement_table_info(is, current_db, &table);
            let pushable: Vec<_> = pushable
                .into_iter()
                .filter(|expr| {
                    !table_info
                        .as_ref()
                        .is_some_and(|info| full_domain_conjunct(info, expr))
                })
                .collect();
            // TiDB ranger: predicates on the index prefix become the access
            // range, so they leave the cop Selection and drive estRows. An
            // expression key part (`tidb_shard(a) = 8`) is not cop-pushable but
            // still seeks.
            let mut range_scan = range_scan;
            let mut row_count = row_count;
            let mut pushable = pushable;
            let mut residual = residual;
            if range_scan.is_none()
                && pushed.is_none()
                && let Some(info) = table_info.as_ref()
            {
                let index_info = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(&index));
                let range_candidates: Vec<ExprNode> =
                    pushable.iter().chain(residual.iter()).cloned().collect();
                if let Some(index_info) = index_info
                    && let Some(access) = super::access_range::index_access_range(
                        info,
                        index_info,
                        &range_candidates,
                        row_count,
                        super::planbuilder::statement_stats(&table).as_ref(),
                    )
                {
                    let consumed = |expr: &ExprNode| {
                        access
                            .consumed
                            .iter()
                            .any(|c| c.restore_string() == expr.restore_string())
                    };
                    pushable.retain(|expr| !consumed(expr));
                    residual.retain(|expr| !consumed(expr));
                    row_count = access.range.filter_rows;
                    range_scan = Some(access.range);
                }
            }
            wrap_residual(
                PhysicalPlan::IndexReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    pushed: and_opt(
                        pushed,
                        ExprNode::compose_cnf(
                            pushable
                                .into_iter()
                                .map(|expr| expr.rewrite_row_in_dnf())
                                .collect(),
                        ),
                    ),
                    range_scan,
                },
                residual,
            )
        }
        PhysicalPlan::IndexMerge {
            table,
            indexes,
            partitions,
            pushed,
            intersection,
            paths,
            est_rows,
            probe_est,
            probe_selection,
            limit,
            keep_order,
            order_column,
            order_desc,
            partition_access,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr, &table));
            // `c BETWEEN T::MIN AND T::MAX` covers the whole domain of a NOT
            // NULL column: it filters nothing, so TiDB folds it away and the
            // scan stays a bare TableFullScan.
            let table_info = statement_table_info(is, current_db, &table);
            let pushable: Vec<_> = pushable
                .into_iter()
                .filter(|expr| {
                    !table_info
                        .as_ref()
                        .is_some_and(|info| full_domain_conjunct(info, expr))
                })
                .collect();
            wrap_residual(
                PhysicalPlan::IndexMerge {
                    table,
                    indexes,
                    partitions,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                    intersection,
                    paths,
                    est_rows,
                    probe_est,
                    probe_selection,
                    limit,
                    keep_order,
                    order_column,
                    order_desc,
                    partition_access,
                },
                residual,
            )
        }
        PhysicalPlan::PointGet {
            table,
            handle,
            handle_unsigned,
            index,
            index_columns,
            partition_names,
            global_index,
            lock,
            common_handle,
        } => {
            // Only the equality on the handle column itself is consumed by a
            // handle point get (`a = 1 AND b = 1` keeps `b = 1` as a filter).
            // A heap table's handle is the hidden `_tidb_rowid`; `handle_column()`
            // returns its first integer column instead, so name the rowid.
            let handle_column = (handle.is_some() && index.is_none())
                .then(|| {
                    statement_table_info(is, current_db, &table).and_then(|info| {
                        if info.pk_is_handle {
                            info.handle_column().map(|c| c.name.o().to_string())
                        } else if info.common_handle {
                            None
                        } else {
                            Some("_tidb_rowid".to_string())
                        }
                    })
                })
                .flatten();
            let key_columns = point_get_key_columns(
                is,
                current_db,
                &table,
                index.as_deref(),
                handle_column.as_deref(),
            );
            let mut residual: Vec<ExprNode> = {
                // Key values the point get already pins, used to fold the
                // remaining filters (`b + 1 = 3` with `b = 2` disappears).
                let pinned: Vec<(String, Datum)> = conjuncts
                    .iter()
                    .filter_map(|expr| expr.eq_key_const())
                    .filter(|(col, _)| key_columns.iter().any(|c| c.eq_ignore_ascii_case(col)))
                    .collect();
                conjuncts
                    .into_iter()
                    .filter_map(|expr| {
                        // The key equality is already the point get lookup.
                        if is_point_get_key(&expr, &key_columns) {
                            return None;
                        }
                        if let Some(reduced) = reduce_pinned_dnf(&expr, &key_columns) {
                            return reduced;
                        }
                        match (&handle_column, handle) {
                            (Some(col), Some(value)) => {
                                if is_handle_eq_on(&expr, value, col) {
                                    return None;
                                }
                            }
                            _ => {
                                if handle.is_some_and(|value| is_handle_eq(&expr, value)) {
                                    return None;
                                }
                            }
                        }
                        // A filter that holds for every pinned value is a no-op.
                        if matches!(
                            fold_with_pinned(&expr, &table, &pinned),
                            Some(Datum::I64(1)) | Some(Datum::U64(1))
                        ) {
                            return None;
                        }
                        Some(expr)
                    })
                    .collect()
            };
            // TiDB renders point-get table filters in sorted expression order.
            residual.sort_by_key(|expr| expr.restore_string());
            wrap_residual(
                PhysicalPlan::PointGet {
                    table,
                    handle,
                    handle_unsigned,
                    index,
                    index_columns,
                    partition_names,
                    global_index,
                    lock,
                    common_handle,
                },
                residual,
            )
        }
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            handles,
            index_columns,
            tuple_columns,
            partition_names,
            global_index,
            lock,
            keep_order,
            desc,
            common_handle,
        } => {
            let key_columns = point_get_key_columns(is, current_db, &table, index.as_deref(), None);
            let mut residual: Vec<ExprNode> = conjuncts
                .into_iter()
                .filter_map(|expr| {
                    if is_point_get_key(&expr, &key_columns) {
                        return None;
                    }
                    let consumed = if tuple_columns.is_empty() {
                        is_in_list_on(&expr, &column)
                    } else {
                        is_row_in_list_on(&expr, &tuple_columns)
                            || is_dnf_point_rows(&expr, &tuple_columns)
                    };
                    if consumed {
                        return None;
                    }
                    reduce_pinned_dnf(&expr, &key_columns).unwrap_or(Some(expr))
                })
                .collect();
            // TiDB renders point-get table filters in sorted expression order.
            residual.sort_by_key(|expr| expr.restore_string());
            wrap_residual(
                PhysicalPlan::BatchPointGet {
                    table,
                    column,
                    index,
                    n_keys,
                    handles,
                    index_columns,
                    tuple_columns,
                    partition_names,
                    global_index,
                    lock,
                    keep_order,
                    desc,
                    common_handle,
                },
                residual,
            )
        }
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
            est_rows: None,
        },
        // The star-order projection only restores column order; filters still
        // belong to the join below it.
        PhysicalPlan::StarOrderProjection { exprs, child } => PhysicalPlan::StarOrderProjection {
            exprs,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::Distinct {
            keys,
            fields,
            names,
            child,
        } => PhysicalPlan::Distinct {
            keys,
            fields,
            names,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::HashJoin {
            est_rows: _,
            mut tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left_cols = table_columns(is, current_db, &left_table);
            let right_cols = table_columns(is, current_db, &right_table);
            let left_nulls = matches!(tp, JoinType::RightOuter);
            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut residual = Vec::new();
            let mut null_supplying = Vec::new();
            for expr in conjuncts {
                match classify(
                    &expr,
                    &left_table,
                    &right_table,
                    &left_cols,
                    &right_cols,
                    tp,
                ) {
                    Side::Left => left_preds.push(expr),
                    Side::Right => right_preds.push(expr),
                    Side::NullSupplying => null_supplying.push(expr),
                    Side::Residual => residual.push(expr),
                }
            }
            // TiDB `PredicatePushDown`: a null-rejecting predicate on the
            // null-supplying side of an outer join simplifies it to an inner
            // join; anything else must stay above the join (pushing it into the
            // null-padded side would drop matched rows).
            if matches!(tp, JoinType::LeftOuter | JoinType::RightOuter)
                && !null_supplying.is_empty()
            {
                for expr in std::mem::take(&mut null_supplying) {
                    if null_rejecting(&expr) {
                        tp = JoinType::Inner;
                        if left_nulls {
                            left_preds.push(expr);
                        } else {
                            right_preds.push(expr);
                        }
                    } else {
                        residual.push(expr);
                    }
                }
            } else if !null_supplying.is_empty() {
                // Not reachable today (only outer joins classify a side this
                // way); keep the predicates above the join to stay correct.
                residual.extend(std::mem::take(&mut null_supplying));
            }
            // Comma joins (`FROM a, b WHERE a.x = b.x`) plan as CARTESIAN until
            // the WHERE equality becomes the join key. Split remaining
            // conjuncts by the tables owned by each subtree: cross-side column
            // equalities attach to this join (`inner join, equal:[…]`), and
            // single-side conjuncts descend to where they belong.
            let mut tp = tp;
            let mut on = on;
            if matches!(tp, JoinType::Inner | JoinType::Cross) {
                let mut left_tables = std::collections::HashSet::new();
                left_tables.insert(left_table.to_ascii_lowercase());
                collect_join_tables(&left, &mut left_tables);
                let mut right_tables = std::collections::HashSet::new();
                right_tables.insert(right_table.to_ascii_lowercase());
                collect_join_tables(&right, &mut right_tables);
                let left_map = column_owners(&left_tables, is, current_db);
                let right_map = column_owners(&right_tables, is, current_db);
                let mut keep = Vec::new();
                // TiDB `pushDownPredicatesForJoin` promotes *every* cross-table
                // equality into the join keys (`HashJoin equal:[eq(…)
                // eq(…)]`); inner joins are filter-equivalent, so promoting
                // more keys only tightens the estimate. The executor hashes on
                // the first key and applies the rest as residual conditions.
                let mut on_keys: Vec<ExprNode> = if is_true_expr(&on) {
                    Vec::new()
                } else {
                    on.split_cnf()
                };
                for expr in residual {
                    match resolve_conjunct_side(
                        &expr,
                        &left_map,
                        &right_map,
                        &left_tables,
                        &right_tables,
                    ) {
                        Some(Side::Left) => left_preds.push(expr),
                        Some(Side::Right) => right_preds.push(expr),
                        Some(Side::Residual) | Some(Side::NullSupplying) | None => {
                            if let Some(key) = qualify_cross_equality(
                                &expr,
                                &left_map,
                                &right_map,
                                &left_tables,
                                &right_tables,
                            ) {
                                // The hash key compares raw datums, while a
                                // residual `Selection` applies MySQL coercion
                                // (`int = bit`): only promote a type-identical
                                // key, else the join would drop matching rows.
                                if !same_join_key_type(&key, is, current_db) {
                                    keep.push(key);
                                } else {
                                    // CROSS JOIN's placeholder `ON TRUE` gives
                                    // way to the equality keys.
                                    on_keys.push(key);
                                    tp = JoinType::Inner;
                                }
                            } else {
                                keep.push(expr);
                            }
                        }
                    }
                }
                on = ExprNode::compose_cnf(on_keys).unwrap_or(on);
                // TiDB `pushDownPredicatesForJoin`: an inner-join equality key
                // makes single-side predicates derivable on the other side
                // (`t1.c1 > k` ⇒ `t2.c1 > k` when `t1.c1 = t2.c1`).
                if matches!(tp, JoinType::Inner) && !is_true_expr(&on) {
                    let (dl, dr) = derive_equal_cond_preds(
                        &on,
                        &left_map,
                        &right_map,
                        &left_table,
                        &right_table,
                        is_single_table_subtree(&left),
                        is_single_table_subtree(&right),
                        &left_preds,
                        &right_preds,
                    );
                    left_preds.extend(dl);
                    right_preds.extend(dr);
                }
                // TiDB `DeriveOtherConditions`: a cross-side comparison in the
                // ON / residual (`lt(t1.c_str, t2.c_str)`) rejects NULLs on each
                // side, so push `not(isnull(col))` into the children.
                if matches!(tp, JoinType::Inner) {
                    let mut others = keep.clone();
                    for item in on.split_cnf() {
                        if let ExprNode::Binary(bin) = &item
                            && bin.op == Op::Eq
                            && matches!(
                                (bin.left.as_ref(), bin.right.as_ref()),
                                (ExprNode::Column(_), ExprNode::Column(_))
                            )
                        {
                            continue;
                        }
                        if !is_true_expr(&item) {
                            others.push(item);
                        }
                    }
                    let (dl, dr) = derive_other_not_null(
                        &others,
                        &left_map,
                        &right_map,
                        &left_table,
                        &right_table,
                        is,
                        current_db,
                    );
                    left_preds.extend(dl);
                    right_preds.extend(dr);
                }
                residual = keep;
            }
            wrap_residual(
                PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp,
                    left: Box::new(push_into(*left, left_preds, is, current_db)),
                    right: Box::new(push_into(*right, right_preds, is, current_db)),
                    left_table,
                    right_table,
                    on,
                },
                residual,
            )
        }
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline,
            recursive,
            limit,
            offset,
        } if inline => {
            // Empty columns: seed names match CTE outputs — push through.
            // Named outputs may differ from seed; keep Selection above unless
            // caller rewrote the filter onto seed columns (HASH_JOIN path).
            if columns.is_empty() {
                PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed: Box::new(push_into(*seed, conjuncts, is, current_db)),
                    recursive_plan,
                    inline,
                    recursive,
                    limit,
                    offset,
                }
            } else {
                wrap_residual(
                    PhysicalPlan::CTE {
                        name,
                        alias,
                        columns,
                        seed,
                        recursive_plan,
                        inline,
                        recursive,
                        limit,
                        offset,
                    },
                    conjuncts,
                )
            }
        }
        // An empty plan folds away its filters (TiDB TableDual rows:0).
        PhysicalPlan::TableDual { row_count: 0 } => PhysicalPlan::TableDual { row_count: 0 },
        // TiDB pushes equality ranges into the infoschema memory table and
        // prints them on the scan (`table_name:["t1"]`); the 10000-row
        // estimate stays, and answered conjuncts leave no `Selection`.
        PhysicalPlan::InfoSchema {
            table,
            pushed,
            ranges: already,
        } => {
            // Cluster / metrics / inspection extractors fold time-range and
            // equality filters into MemTableScan operator info (TiDB).
            let extracted = if super::memtable_extractor::is_metrics_pushed(pushed.as_deref())
                || matches!(
                    table.to_ascii_lowercase().as_str(),
                    "cluster_log"
                        | "slow_query"
                        | "cluster_config"
                        | "inspection_result"
                        | "inspection_rules"
                        | "table_storage_stats"
                        | "inspection_summary"
                        | "tiflash_tables"
                        | "tiflash_segments"
                        | "up"
                        | "tidb_query_duration"
                        | "tables"
                        | "statistics"
                        | "key_column_usage"
                        | "referential_constraints"
                ) {
                let folded: Vec<_> = conjuncts
                    .iter()
                    .cloned()
                    .map(|c| super::memtable_extractor::fold_database_consts(c, current_db))
                    .collect();
                Some(super::memtable_extractor::extract_memtable_predicates(
                    &table, folded,
                ))
            } else {
                None
            };
            if let Some(extracted) = extracted {
                let mut text = if extracted.replace_pushed {
                    String::new()
                } else {
                    pushed.unwrap_or_default()
                };
                if !extracted.parts.is_empty() {
                    text = merge_memtable_pushed_parts(&text, &extracted.parts);
                }
                let mut ranges = already;
                ranges.extend(extracted.ranges);
                ranges.sort();
                ranges.dedup();
                return wrap_residual(
                    PhysicalPlan::InfoSchema {
                        table,
                        pushed: (!text.is_empty()).then_some(text),
                        ranges,
                    },
                    extracted.residual,
                );
            }
            // A range can only answer a predicate on a column the memory table
            // actually exposes; anything else stays above as a `Selection`.
            let columns = sealdb_infoschema::tables::virtual_columns(&table);
            let mut ranges: Vec<(String, String)> = already;
            let mut residual = Vec::new();
            for conjunct in conjuncts {
                match mem_table_range(&conjunct) {
                    Some((column, values))
                        if columns.iter().any(|c| c.eq_ignore_ascii_case(&column))
                            || super::explain::mem_table_column_index(&table, &column)
                                .is_some() =>
                    {
                        ranges.extend(values.into_iter().map(|v| (column.clone(), v)));
                    }
                    _ => residual.push(mark_mem_table_like_ci(conjunct, &columns)),
                }
            }
            ranges.sort();
            ranges.dedup();
            let mut text = pushed.unwrap_or_default();
            if !ranges.is_empty() {
                let mut attached: Vec<String> = Vec::new();
                let mut i = 0;
                while i < ranges.len() {
                    let column = ranges[i].0.clone();
                    let mut values = Vec::new();
                    while i < ranges.len() && ranges[i].0 == column {
                        values.push(format!("\"{}\"", ranges[i].1));
                        i += 1;
                    }
                    // STATEMENTS_SUMMARY spells its digest range `digests: […]`.
                    let (label, sep) =
                        if table.eq_ignore_ascii_case("statements_summary") && column == "digest" {
                            ("digests".to_string(), ": ")
                        } else {
                            (column, ":")
                        };
                    attached.push(format!("{label}{sep}[{}]", values.join(",")));
                }
                if !text.is_empty() {
                    text.push_str(", ");
                }
                text.push_str(&attached.join(", "));
            }
            wrap_residual(
                PhysicalPlan::InfoSchema {
                    table,
                    pushed: (!text.is_empty()).then_some(text),
                    ranges,
                },
                residual,
            )
        }
        // Inner merge join: the join key already enforces its equality, so a
        // WHERE copy of that key leaves no `Selection` (TiDB `PredicatePushDown`
        // folds it into the key). Single-side conjuncts descend when the side is
        // a base table; everything else stays above the join.
        PhysicalPlan::MergeJoin {
            est_rows: _,
            tp: JoinType::Inner,
            left,
            right,
            left_table,
            right_table,
            left_key,
            right_key,
            info,
            on,
        } => {
            let key_items: Vec<String> = on
                .split_cnf_all()
                .iter()
                .map(|item| item.restore_string())
                .collect();
            let left_cols = table_columns(is, current_db, &left_table);
            let right_cols = table_columns(is, current_db, &right_table);
            let left_single = is_single_table_subtree(&left);
            let right_single = is_single_table_subtree(&right);
            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut residual = Vec::new();
            for expr in conjuncts {
                if key_items.iter().any(|key| *key == expr.restore_string()) {
                    continue;
                }
                match classify(
                    &expr,
                    &left_table,
                    &right_table,
                    &left_cols,
                    &right_cols,
                    JoinType::Inner,
                ) {
                    Side::Left if left_single => left_preds.push(expr),
                    Side::Right if right_single => right_preds.push(expr),
                    _ => residual.push(expr),
                }
            }
            wrap_residual(
                PhysicalPlan::MergeJoin {
                    est_rows: None,
                    tp: JoinType::Inner,
                    left: Box::new(push_into(*left, left_preds, is, current_db)),
                    right: Box::new(push_into(*right, right_preds, is, current_db)),
                    left_table,
                    right_table,
                    left_key,
                    right_key,
                    info,
                    on,
                },
                residual,
            )
        }
        other => wrap_residual(other, conjuncts),
    }
}

/// Resolve the table metadata of a plan's scan node. A statement may join
/// tables from another database (`FROM other_db.t`), and the scan node only
/// keeps the bare table name, so the database comes from the statement scope.
fn statement_table_info<'a>(
    is: Option<&'a InfoSchema>,
    current_db: &str,
    table: &str,
) -> Option<&'a sealdb_meta::model::table::TableInfo> {
    let is = is?;
    let db =
        super::planbuilder::statement_table_db(table).unwrap_or_else(|| current_db.to_string());
    // The plan keeps the SQL alias (`FROM t tmp` → `table:tmp`), while the
    // ranger needs the base table definition to build the index seek.
    let physical = super::planbuilder::explain_physical_table(table);
    is.table_by_name(&db, physical.as_deref().unwrap_or(table))
}

fn pk_eq_handle(
    is: Option<&InfoSchema>,
    current_db: &str,
    table: &str,
    conjuncts: &[ExprNode],
) -> Option<i64> {
    if conjuncts.len() != 1 {
        return None;
    }
    let (col, value) = conjuncts[0].eq_column_const()?;
    let info = statement_table_info(is, current_db, table)?;
    if !info.pk_is_handle {
        return None;
    }
    let handle = info.handle_column()?;
    if !handle.name.o().eq_ignore_ascii_case(&col) {
        return None;
    }
    match value {
        Datum::I64(v) => Some(v),
        Datum::U64(v) => Some(v as i64),
        Datum::Bytes(b) => std::str::from_utf8(&b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// TiDB pushes a predicate into cop only when every function has a cop
/// implementation. `tidb_shard` is evaluated by TiDB alone, so a predicate
/// that mentions it stays a root `Selection` (`explain_shard_index`).
///
/// An `ENUM` / `SET` column reaches the coprocessor through an implicit
/// `cast_enum` (`cast(col, double)` / `cast(col, var_string)`), so a
/// blacklisted `enum` row keeps every predicate over such a column on the TiDB
/// side — even a plain `gt(b, 1)`.
pub(crate) fn cop_pushable(expr: &ExprNode, table: &str) -> bool {
    // `mysql.expr_pushdown_blacklist`: TiDB checks the expression's own
    // `ScalarFunction` name at every node of the conjunct, so a blacklisted
    // `lt` keeps the whole conjunct (and its children) on the tidb side.
    if super::blacklist::expr_node_pushdown_disabled(expr) {
        return false;
    }
    if super::blacklist::expr_pushdown_disabled("enum") && expr_references_enum_column(expr, table)
    {
        return false;
    }
    match expr {
        ExprNode::Func(func) => {
            let name = func.name.to_ascii_lowercase();
            // TiDB keeps base64 transforms on the tidb side (root Selection
            // above an IndexReader that only absorbed the LIKE / range).
            if matches!(name.as_str(), "from_base64" | "to_base64") {
                return false;
            }
            // Binary-charset columns cannot evaluate regexp in coprocessor.
            if matches!(
                name.as_str(),
                "regexp_like" | "regexp_instr" | "regexp_substr" | "regexp_replace" | "regexp"
            ) && expr_references_binary_charset(expr, table)
            {
                return false;
            }
            name != "tidb_shard" && func.args.iter().all(|arg| cop_pushable(arg, table))
        }
        ExprNode::Binary(bin) => cop_pushable(&bin.left, table) && cop_pushable(&bin.right, table),
        ExprNode::Unary(unary) => cop_pushable(&unary.expr, table),
        ExprNode::Cast(cast) => cop_pushable(&cast.expr, table),
        ExprNode::InList(list) => {
            cop_pushable(&list.expr, table)
                && list.list.iter().all(|item| cop_pushable(item, table))
        }
        ExprNode::Between(between) => {
            cop_pushable(&between.expr, table)
                && cop_pushable(&between.low, table)
                && cop_pushable(&between.high, table)
        }
        ExprNode::IsNull(is_null) => cop_pushable(&is_null.expr, table),
        ExprNode::Like(like) => {
            cop_pushable(&like.expr, table) && cop_pushable(&like.pattern, table)
        }
        // TiDB's coprocessor has no `case` scalar function: a `CASE` expression
        // keeps its whole conjunct on the tidb side (`index_advise` sizes the
        // root `Selection` with the 0.8 factor over the cop-filtered scan).
        ExprNode::Case(_) => false,
        _ => true,
    }
}

/// True when `expr` reads a binary-charset column (TiDB keeps regexp on root).
fn expr_references_binary_charset(expr: &ExprNode, table: &str) -> bool {
    expr.referenced_columns().iter().any(|(qualifier, name)| {
        let qualifier = qualifier.as_deref().unwrap_or(table);
        crate::core::planbuilder::explain_column_type(qualifier, name)
            .is_some_and(|ft| ft.charset.eq_ignore_ascii_case("binary"))
    })
}

/// Does `expr` read an `ENUM` / `SET` column of `table`?
fn expr_references_enum_column(expr: &ExprNode, table: &str) -> bool {
    expr.referenced_columns().iter().any(|(qualifier, name)| {
        let qualifier = qualifier.as_deref().unwrap_or(table);
        crate::core::planbuilder::explain_column_type(qualifier, name).is_some_and(|ft| {
            matches!(
                ft.tp,
                sealdb_types::field_type::TYPE_ENUM | sealdb_types::field_type::TYPE_SET
            )
        })
    })
}

/// Key columns a point get pins: the index key parts (an expression part is
/// keyed by its canonical text), or the handle column of a handle lookup.
fn point_get_key_columns(
    is: Option<&InfoSchema>,
    current_db: &str,
    table: &str,
    index: Option<&str>,
    handle_column: Option<&str>,
) -> Vec<String> {
    match index {
        Some(name) => statement_table_info(is, current_db, table)
            .and_then(|info| {
                let idx = info
                    .indices
                    .iter()
                    .find(|idx| idx.name.o().eq_ignore_ascii_case(name))?;
                Some(super::expr_index::part_keys(info, idx))
            })
            .unwrap_or_default(),
        None => handle_column
            .map(|c| vec![c.to_string()])
            .unwrap_or_default(),
    }
}

/// Is the conjunct an equality on one of the key columns?
fn is_point_get_key(expr: &ExprNode, key_columns: &[String]) -> bool {
    expr.eq_key_const()
        .is_some_and(|(key, _)| key_columns.iter().any(|c| c.eq_ignore_ascii_case(&key)))
}

/// Reduce a DNF whose key equalities the point get answers:
/// `a = 100 AND (b = 100 OR b = 200)` keeps `or(b = 100, b = 200)`.
///
/// `None` when the conjunct is not a DNF over key equalities; `Some(None)` when
/// every arm is fully answered by the lookup (the filter is redundant).
fn reduce_pinned_dnf(expr: &ExprNode, key_columns: &[String]) -> Option<Option<ExprNode>> {
    let arms = expr.split_dnf();
    if arms.len() < 2
        || !arms.iter().all(|arm| {
            arm.split_cnf_all()
                .iter()
                .any(|c| is_point_get_key(c, key_columns))
        })
    {
        return None;
    }
    let mut leftovers: Vec<ExprNode> = Vec::with_capacity(arms.len());
    for arm in &arms {
        let kept: Vec<ExprNode> = arm
            .split_cnf_all()
            .into_iter()
            .filter(|conjunct| !is_point_get_key(conjunct, key_columns))
            .collect();
        match ExprNode::compose_cnf(kept) {
            // The point get already answers this arm.
            None => return Some(None),
            Some(filter) => leftovers.push(filter),
        }
    }
    Some(leftovers.into_iter().rev().reduce(|right, left| {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicOr,
            left: Box::new(left),
            right: Box::new(right),
            single_cnf_item: false,
        })
    }))
}

/// Infoschema memory tables answer an equality on a string column
/// (`table_name = 'T1'` → `table_name:["t1"]`); values compare under the
/// case-insensitive column collation, so they render lower-cased.
fn mem_table_range(expr: &ExprNode) -> Option<(String, Vec<String>)> {
    // An IN list is a union of point ranges on one column
    // (`digest IN ('a','b')` → `digests: ["a","b"]`).
    if let ExprNode::InList(list) = expr {
        if list.not {
            return None;
        }
        let ExprNode::Column(col) = list.expr.as_ref() else {
            return None;
        };
        let values = list
            .list
            .iter()
            .map(|item| item.const_datum().and_then(mem_table_value))
            .collect::<Option<Vec<_>>>()?;
        return Some((col.name.to_ascii_lowercase(), values));
    }
    let (column, value) = expr.eq_column_const()?;
    Some((column.to_ascii_lowercase(), vec![mem_table_value(value)?]))
}

fn mem_table_value(value: Datum) -> Option<String> {
    let text = match value {
        Datum::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Datum::I64(v) => v.to_string(),
        Datum::U64(v) => v.to_string(),
        _ => return None,
    };
    Some(text.to_lowercase())
}

/// Infoschema string columns carry a case-insensitive collation
/// (`utf8mb4_general_ci`), but the residual `LIKE` stays above the
/// `MemTableScan`. Mark those predicates so evaluation matches TiDB
/// (`column_name LIKE 'I%'` finds `id`).
fn mark_mem_table_like_ci(mut expr: ExprNode, columns: &[String]) -> ExprNode {
    match &mut expr {
        ExprNode::Like(like) => {
            if let ExprNode::Column(col) = like.expr.as_ref()
                && columns.iter().any(|c| c.eq_ignore_ascii_case(&col.name))
            {
                like.ci = true;
            }
        }
        ExprNode::Binary(bin) if matches!(bin.op, Op::LogicAnd | Op::LogicOr | Op::LogicXor) => {
            bin.left = Box::new(mark_mem_table_like_ci((*bin.left).clone(), columns));
            bin.right = Box::new(mark_mem_table_like_ci((*bin.right).clone(), columns));
        }
        ExprNode::Unary(unary) if matches!(unary.op, Op::Not) => {
            unary.expr = Box::new(mark_mem_table_like_ci((*unary.expr).clone(), columns));
        }
        _ => {}
    }
    expr
}

/// Overlay new `col:[…]` fragments onto an existing MemTableScan pushed string,
/// replacing any prior entry for the same column (multi-step push-down).
/// Leaves `PromQL:…` prefixes intact — those contain commas that are not part
/// boundaries.
fn merge_memtable_pushed_parts(existing: &str, new_parts: &[String]) -> String {
    if new_parts.is_empty() {
        return existing.to_string();
    }
    // Metrics PromQL text is opaque; only append extractor suffixes.
    if existing.starts_with("PromQL:") || existing.is_empty() {
        let mut text = existing.to_string();
        for part in new_parts {
            if !text.is_empty() {
                text.push_str(", ");
            }
            text.push_str(part);
        }
        return text;
    }
    // Split only on `, col:` style boundaries (column fragments), preserving order.
    let mut entries: Vec<(String, String)> = Vec::new();
    for part in existing.split(", ").filter(|p| !p.is_empty()) {
        let key = part
            .split_once(':')
            .map(|(c, _)| c.to_string())
            .unwrap_or_else(|| part.to_string());
        if let Some(slot) = entries.iter_mut().find(|(k, _)| k == &key) {
            slot.1 = part.to_string();
        } else {
            entries.push((key, part.to_string()));
        }
    }
    for part in new_parts {
        let key = part
            .split_once(':')
            .map(|(c, _)| c.to_string())
            .unwrap_or_else(|| part.clone());
        if let Some(slot) = entries.iter_mut().find(|(k, _)| k == &key) {
            slot.1 = part.clone();
        } else {
            entries.push((key, part.clone()));
        }
    }
    entries
        .into_iter()
        .map(|(_, v)| v)
        .collect::<Vec<_>>()
        .join(", ")
}

fn wrap_residual(plan: PhysicalPlan, residual: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(residual.clone()) else {
        return plan;
    };
    if filter.has_subquery() {
        PhysicalPlan::Apply {
            child: Box::new(plan),
            filter,
            probe: None,
            info: None,
        }
    } else {
        // A conjunct the expression blacklist kept out of the coprocessor never
        // had a range derived for it, so TiDB sizes the `Selection` with the
        // plain 0.8 `selectionFactor` instead of the pseudo range selectivity.
        let read_table = crate::core::explain::first_read_table(&plan).unwrap_or_default();
        let est_rows = residual
            .iter()
            .any(|expr| {
                super::blacklist::expr_contains_blacklisted_pushdown(expr)
                    || (super::blacklist::expr_pushdown_disabled("enum")
                        && expr_references_enum_column(expr, read_table))
            })
            .then(|| super::explain::est_rows(&plan) * 0.8);
        PhysicalPlan::Selection {
            child: Box::new(plan),
            filter,
            est_rows,
        }
    }
}

/// Evaluate a residual filter with the point-get key values substituted:
/// `b + 1 = 3` under `b = 2` is constantly true, so TiDB drops it.
/// `None` when the expression cannot be folded (unknown column shape, error).
fn fold_with_pinned(
    expr: &ExprNode,
    table: &str,
    pinned: &[(String, Datum)],
) -> Option<sealdb_types::Datum> {
    let substituted = substitute_pinned_columns(expr, table, pinned)?;
    sealdb_expression::eval_expr(&substituted).ok()
}

fn substitute_pinned_columns(
    expr: &ExprNode,
    table: &str,
    pinned: &[(String, Datum)],
) -> Option<ExprNode> {
    use sealdb_parser::ast::expressions::{
        BinaryOpExpr, ExprNode, FuncCastExpr, UnaryOpExpr, ValueExpr,
    };
    match expr {
        ExprNode::Column(c) => {
            let same_table = c
                .table
                .as_deref()
                .is_none_or(|t| t.is_empty() || t.eq_ignore_ascii_case(table));
            if !same_table {
                return None;
            }
            pinned
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&c.name))
                .map(|(_, value)| {
                    ExprNode::Value(ValueExpr {
                        datum: value.clone(),
                    })
                })
        }
        ExprNode::Value(_) => Some(expr.clone()),
        ExprNode::Binary(bin) => Some(ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(substitute_pinned_columns(&bin.left, table, pinned)?),
            right: Box::new(substitute_pinned_columns(&bin.right, table, pinned)?),
            single_cnf_item: bin.single_cnf_item,
        })),
        ExprNode::Unary(unary) => Some(ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(substitute_pinned_columns(&unary.expr, table, pinned)?),
        })),
        ExprNode::Cast(cast) => Some(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(substitute_pinned_columns(&cast.expr, table, pinned)?),
            target: cast.target.clone(),
            json: cast.json,
        })),
        _ => None,
    }
}

/// TiDB `PredicateSimplification` (`updateOrPredicate`) on one pushed filter:
/// a scalar conjunct (`b = 300`) rules out the OR arms it contradicts
/// (`a = 100 AND b = 100`), so `(a = 100 AND b = 100) OR a = 200` collapses to
/// `a = 200`. A single surviving arm spreads into the CNF list the way TiDB
/// prints the cop Selection.
fn simplify_or_arms(filter: ExprNode) -> ExprNode {
    let conjuncts = filter.split_cnf_all();
    let top_pins: Vec<(String, Datum)> = conjuncts
        .iter()
        .filter_map(|cond| cond.eq_key_const())
        .map(|(col, value)| (col.to_ascii_lowercase(), value))
        .collect();
    let mut out: Vec<ExprNode> = Vec::new();
    let mut changed = false;
    for cond in conjuncts {
        if !matches!(&cond, ExprNode::Binary(bin) if bin.op == Op::LogicOr) {
            out.push(cond);
            continue;
        }
        let mut kept: Vec<Vec<ExprNode>> = Vec::new();
        let mut dropped_arm = false;
        for arm in cond.split_dnf() {
            let arm_conjuncts = arm.split_cnf_all();
            let mut pins = top_pins.clone();
            for (col, value) in arm_conjuncts.iter().filter_map(|cc| cc.eq_key_const()) {
                let col = col.to_ascii_lowercase();
                if !pins.iter().any(|(name, _)| *name == col) {
                    pins.push((col, value));
                }
            }
            let mut alive = true;
            let mut remaining: Vec<ExprNode> = Vec::new();
            for conjunct in arm_conjuncts {
                let own = conjunct
                    .eq_key_const()
                    .map(|(col, _)| col.to_ascii_lowercase())
                    .into_iter()
                    .collect::<Vec<_>>();
                let relevant: Vec<(String, Datum)> = pins
                    .iter()
                    .filter(|(col, _)| !own.iter().any(|own| own == col))
                    .cloned()
                    .collect();
                let rewritten = if relevant.is_empty() {
                    conjunct
                } else {
                    super::planbuilder::substitute_eq_columns(&conjunct, &relevant)
                        .unwrap_or(conjunct)
                };
                match constant_truth(&rewritten) {
                    // Only a pin makes an arm contradictory. A literal arm
                    // (`or(0, eq(a, -1))` from partition pruning) stays as
                    // TiDB prints it.
                    Some(false) if !relevant.is_empty() => {
                        alive = false;
                        break;
                    }
                    Some(false) | Some(true) if relevant.is_empty() => return filter,
                    Some(false) => {
                        alive = false;
                        break;
                    }
                    Some(true) => {
                        changed = true;
                    }
                    None => remaining.push(rewritten),
                }
            }
            if alive {
                kept.push(remaining);
            } else {
                dropped_arm = true;
            }
        }
        if !dropped_arm {
            out.push(cond);
            continue;
        }
        changed = true;
        match kept.len() {
            0 => return filter,
            1 => out.extend(kept.pop().unwrap_or_default()),
            _ => {
                let arms: Vec<ExprNode> = kept
                    .into_iter()
                    .filter_map(ExprNode::compose_cnf)
                    .map(|arm| arm.rewrite_row_in_dnf())
                    .collect();
                if let Some(arm) = arms.into_iter().reduce(|left, right| {
                    ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
                        op: Op::LogicOr,
                        left: Box::new(left),
                        right: Box::new(right),
                        single_cnf_item: false,
                    })
                }) {
                    out.push(arm);
                }
            }
        }
    }
    if !changed {
        return filter;
    }
    ExprNode::compose_cnf(out).unwrap_or(filter)
}

/// Truth value of a column-free predicate, `None` when it still reads columns.
fn constant_truth(expr: &ExprNode) -> Option<bool> {
    if !expr.referenced_columns().is_empty() {
        return None;
    }
    let value = sealdb_expression::eval_expr(expr).ok()?;
    if value.is_null() {
        return Some(false);
    }
    Some(value.as_i64().map(|v| v != 0).unwrap_or(true))
}

fn and_opt(left: Option<ExprNode>, right: Option<ExprNode>) -> Option<ExprNode> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) => {
            // A conjunct can reach the same scan twice (e.g. a WHERE predicate
            // mirrored through a join key that the child already received).
            // TiDB keeps a single copy, so drop the duplicates by their
            // rendered text.
            let existing: Vec<String> = right
                .split_cnf()
                .iter()
                .map(|c| super::explain::explain_pushed_filter("t", c))
                .collect();
            let kept: Vec<ExprNode> = left
                .split_cnf()
                .into_iter()
                .filter(|c| !existing.contains(&super::explain::explain_pushed_filter("t", c)))
                .collect();
            match ExprNode::compose_cnf(kept) {
                Some(kept) => ExprNode::compose_cnf(vec![kept, right]),
                None => Some(right),
            }
        }
    }
}

/// Integer type bounds `(min, max)` for the column's field type.
pub(crate) fn int_bounds(ft: &sealdb_types::field_type::FieldType) -> Option<(i128, i128)> {
    use sealdb_types::field_type::*;
    let (lo, hi) = if ft.is_unsigned() {
        match ft.tp {
            TYPE_TINY => (0, u8::MAX as i128),
            TYPE_SHORT => (0, u16::MAX as i128),
            TYPE_INT24 => (0, (1i128 << 24) - 1),
            TYPE_LONG => (0, u32::MAX as i128),
            TYPE_LONG_LONG => (0, u64::MAX as i128),
            _ => return None,
        }
    } else {
        match ft.tp {
            TYPE_TINY => (i8::MIN as i128, i8::MAX as i128),
            TYPE_SHORT => (i16::MIN as i128, i16::MAX as i128),
            TYPE_INT24 => (-(1i128 << 23), (1i128 << 23) - 1),
            TYPE_LONG => (i32::MIN as i128, i32::MAX as i128),
            TYPE_LONG_LONG => (i64::MIN as i128, i64::MAX as i128),
            _ => return None,
        }
    };
    Some((lo, hi))
}

/// True when the conjunct is a range bound that always holds inside the
/// column's NOT NULL type domain (`col >= T::MIN`, `col <= T::MAX`, …).
fn full_domain_conjunct(info: &sealdb_meta::TableInfo, expr: &ExprNode) -> bool {
    use sealdb_parser::opcode::Op;
    let ExprNode::Binary(bin) = expr else {
        return false;
    };
    let (col, value, flipped) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(c), other) => (c, const_i128(other), false),
        (other, ExprNode::Column(c)) => (c, const_i128(other), true),
        _ => return false,
    };
    let Some(value) = value else {
        return false;
    };
    // Dropping a bound also drops its NULL filter, so the column must be
    // NOT NULL: the PRIMARY KEY (clustered handle or index).
    let non_null = (info.pk_is_handle
        && info
            .handle_column()
            .is_some_and(|c| c.name.o().eq_ignore_ascii_case(&col.name)))
        || info.indices.iter().any(|idx| {
            idx.primary
                && idx
                    .columns
                    .iter()
                    .any(|c| c.name.o().eq_ignore_ascii_case(&col.name))
        });
    if !non_null {
        return false;
    }
    let Some(column) = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
    else {
        return false;
    };
    let Some((lo, hi)) = int_bounds(&column.field_type) else {
        return false;
    };
    let op = if flipped {
        match bin.op {
            Op::Lt => Op::Gt,
            Op::Le => Op::Ge,
            Op::Gt => Op::Lt,
            Op::Ge => Op::Le,
            other => other,
        }
    } else {
        bin.op
    };
    match op {
        Op::Ge => value <= lo,
        Op::Le => value >= hi,
        _ => false,
    }
}

pub(crate) fn const_i128(expr: &ExprNode) -> Option<i128> {
    match expr {
        ExprNode::Value(v) => match &v.datum {
            Datum::I64(v) => Some(*v as i128),
            Datum::U64(v) => Some(*v as i128),
            Datum::F64(v) if v.fract() == 0.0 => Some(*v as i128),
            Datum::Decimal(d) => d.to_string().parse().ok(),
            Datum::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            _ => None,
        },
        // `-9223372036854775808` parses as unary minus on a positive literal.
        ExprNode::Unary(u) if u.op == sealdb_parser::opcode::Op::UnaryMinus => {
            const_i128(&u.expr).map(|v| -v)
        }
        _ => None,
    }
}

fn is_handle_eq_on(expr: &ExprNode, handle: i64, column: &str) -> bool {
    expr.eq_column_const().is_some_and(|(col, value)| {
        col.eq_ignore_ascii_case(column) && datum_as_i64(&value) == Some(handle)
    })
}

fn is_handle_eq(expr: &ExprNode, handle: i64) -> bool {
    expr.eq_column_const()
        .and_then(|(_, value)| datum_as_i64(&value))
        .is_some_and(|v| v == handle)
}

fn datum_as_i64(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => Some(*v as i64),
        Datum::F64(v) if v.fract() == 0.0 && *v >= i64::MIN as f64 && *v <= i64::MAX as f64 => {
            Some(*v as i64)
        }
        Datum::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn is_in_list_on(expr: &ExprNode, column: &str) -> bool {
    expr.column_in_consts()
        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(column))
}

/// Row-IN list whose columns are exactly the batch point get key columns.
fn is_row_in_list_on(expr: &ExprNode, tuple_columns: &[String]) -> bool {
    expr.row_in_consts_with_len().is_some_and(|(names, _, _)| {
        names.len() == tuple_columns.len()
            && names.iter().all(|name| {
                tuple_columns
                    .iter()
                    .any(|col| col.eq_ignore_ascii_case(name))
            })
    })
}

/// OR of AND equalities whose arms exactly constrain the batch point get key.
fn is_dnf_point_rows(expr: &ExprNode, tuple_columns: &[String]) -> bool {
    let arms = expr.split_dnf();
    arms.len() > 1
        && arms.iter().all(|arm| {
            let eqs: Vec<(String, Datum)> = arm
                .split_cnf_all()
                .into_iter()
                .filter_map(|item| item.eq_key_const())
                .collect();
            // Every arm conjunct must be one of the key parts (a shard rewrite
            // adds a redundant `tidb_shard(a) = <bucket>` next to `a = 100`),
            // and the arm must pin them all.
            eqs.len() >= tuple_columns.len()
                && eqs.iter().all(|(name, _)| {
                    tuple_columns
                        .iter()
                        .any(|column| column.eq_ignore_ascii_case(name))
                })
                && tuple_columns.iter().all(|column| {
                    eqs.iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(column))
                })
        })
}

enum Side {
    Left,
    Right,
    /// Predicate on the null-supplying side of an outer join. Only a
    /// null-rejecting predicate may go there, and TiDB then turns the outer
    /// join into an inner one.
    NullSupplying,
    Residual,
}

/// Whether `expr` rejects a row whose side columns are all `NULL` (TiDB
/// `outerJoin` predicate simplification): `t.a > 0` yields `NULL` there, while
/// `t.a IS NULL` yields `TRUE`. Evaluate the predicate with every column
/// replaced by `NULL`; anything that is not a plain `TRUE` rejects the row.
fn null_rejecting(expr: &ExprNode) -> bool {
    if expr.has_subquery() {
        return false;
    }
    fn nullify(expr: &ExprNode) -> ExprNode {
        match expr {
            ExprNode::Column(_) => {
                ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr { datum: Datum::Null })
            }
            ExprNode::Binary(bin) => {
                ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
                    op: bin.op,
                    left: Box::new(nullify(&bin.left)),
                    right: Box::new(nullify(&bin.right)),
                    single_cnf_item: bin.single_cnf_item,
                })
            }
            ExprNode::Unary(un) => ExprNode::Unary(sealdb_parser::ast::expressions::UnaryOpExpr {
                op: un.op,
                expr: Box::new(nullify(&un.expr)),
            }),
            _ => expr.clone(),
        }
    }
    match sealdb_expression::eval_expr(&nullify(expr)) {
        Ok(datum) => !matches!(datum, Datum::I64(1) | Datum::U64(1)),
        // Unevaluable predicates stay where they are (residual), which never
        // changes the result set.
        Err(_) => false,
    }
}

fn classify(
    expr: &ExprNode,
    left_table: &str,
    right_table: &str,
    left_cols: &[String],
    right_cols: &[String],
    tp: JoinType,
) -> Side {
    if expr.has_subquery() {
        return Side::Residual;
    }
    let cols = expr.referenced_columns();
    if cols.is_empty() {
        return Side::Residual;
    }
    let mut left = true;
    let mut right = true;
    for (table, name) in cols {
        let on_left = column_on(table.as_deref(), &name, left_table, left_cols);
        let on_right = column_on(table.as_deref(), &name, right_table, right_cols);
        if !on_left {
            left = false;
        }
        if !on_right {
            right = false;
        }
        if !on_left && !on_right {
            return Side::Residual;
        }
    }
    match (left, right, tp) {
        (true, false, JoinType::RightOuter) => Side::NullSupplying,
        (true, false, _) => Side::Left,
        (false, true, JoinType::Inner | JoinType::Cross | JoinType::Semi | JoinType::Anti) => {
            Side::Right
        }
        (false, true, JoinType::RightOuter) => Side::Right,
        (false, true, JoinType::LeftOuter) => Side::NullSupplying,
        (true, true, _) => Side::Residual,
        (false, false, _) => Side::Residual,
    }
}

fn column_on(table: Option<&str>, name: &str, owner: &str, cols: &[String]) -> bool {
    if let Some(table) = table {
        return table.eq_ignore_ascii_case(owner);
    }
    if cols.is_empty() {
        return false;
    }
    cols.iter().any(|col| col.eq_ignore_ascii_case(name))
}

fn table_columns(is: Option<&InfoSchema>, current_db: &str, table: &str) -> Vec<String> {
    let Some(is) = is else {
        return Vec::new();
    };
    statement_table_info(Some(is), current_db, table)
        .or_else(|| {
            is.all_schemas()
                .into_iter()
                .find_map(|db| is.table_by_name(db.name.o(), table))
        })
        .map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Lower-cased table names owned by a join subtree (HashJoin/MergeJoin carry
/// both sides' table names at every level).
fn collect_join_tables(plan: &PhysicalPlan, out: &mut std::collections::HashSet<String>) {
    match plan {
        PhysicalPlan::HashJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        } => {
            out.insert(left_table.to_ascii_lowercase());
            out.insert(right_table.to_ascii_lowercase());
            collect_join_tables(left, out);
            collect_join_tables(right, out);
        }
        PhysicalPlan::MergeJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        } => {
            out.insert(left_table.to_ascii_lowercase());
            out.insert(right_table.to_ascii_lowercase());
            collect_join_tables(left, out);
            collect_join_tables(right, out);
        }
        _ => {}
    }
}

/// A side that holds no nested join is a single base table (or a leaf such as
/// a CTE/derived table): every column of its ON operand belongs to it.
fn is_single_table_subtree(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::HashJoin { .. } | PhysicalPlan::MergeJoin { .. } => false,
        PhysicalPlan::Projection { child, .. } | PhysicalPlan::Distinct { child, .. } => {
            is_single_table_subtree(child)
        }
        _ => true,
    }
}

fn is_true_expr(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::Value(value) if value.datum == Datum::I64(1))
}

/// Column name → owning tables for every table joined in a subtree.
fn column_owners(
    tables: &std::collections::HashSet<String>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for table in tables {
        for col in table_columns(is, current_db, table) {
            map.entry(col.to_ascii_lowercase())
                .or_default()
                .push(table.clone());
        }
    }
    map
}

/// Which table a (possibly qualified) column reference resolves to inside one
/// join side. A qualifier is only honoured for a table that this side really
/// owns: `eq(t1.id, t2.id)` must not look like "owned by the left side" just
/// because `t1` is written down, otherwise a cross-table equality can never
/// become the join key.
fn owner_of(
    table: Option<&str>,
    col: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
    tables: &std::collections::HashSet<String>,
) -> Option<String> {
    if let Some(table) = table {
        let table = table.to_ascii_lowercase();
        if !tables.contains(&table) {
            return None;
        }
        // A known column must be listed under this table; an unresolved
        // subtree (alias not in the infoschema) leaves `map` empty and relies
        // on the qualifier alone.
        return match map.get(&col.to_ascii_lowercase()) {
            Some(owners) => owners.contains(&table).then_some(table),
            None => Some(table),
        };
    }
    let owners = map.get(&col.to_ascii_lowercase())?;
    (owners.len() == 1).then(|| owners[0].clone())
}

/// The single join input that owns every column of `expr`, or `None` when the
/// expression is constant, spans inputs, or names an unknown table.
fn expr_owned_by_side(
    expr: &ExprNode,
    map: &std::collections::HashMap<String, Vec<String>>,
    tables: &std::collections::HashSet<String>,
) -> Option<String> {
    let mut owner: Option<String> = None;
    for (table, name) in expr.referenced_columns() {
        let table = table?;
        let current = owner_of(Some(&table), &name, map, tables)?;
        if owner.as_ref().is_some_and(|seen| seen != &current) {
            return None;
        }
        owner = Some(current);
    }
    owner
}

/// Which join side owns every column of `expr`; `None` when a column belongs
/// to both sides / is unknown (so the conjunct stays residual or becomes a key).
/// A cross-table equality can only replace the join filter when both columns
/// have the same declared type. Unknown columns (aliases, derived sides) keep
/// the historical behaviour and are promoted.
fn same_join_key_type(key: &ExprNode, is: Option<&InfoSchema>, current_db: &str) -> bool {
    use sealdb_parser::opcode::Op;
    let ExprNode::Binary(bin) = key else {
        return true;
    };
    if bin.op != Op::Eq {
        return true;
    }
    let (ExprNode::Column(lhs), ExprNode::Column(rhs)) = (bin.left.as_ref(), bin.right.as_ref())
    else {
        return true;
    };
    let tp = |col: &sealdb_parser::ast::expressions::ColumnNameExpr| -> Option<u8> {
        let table = col.table.as_deref()?;
        let (db, table) = match table.split_once('.') {
            Some((db, table)) => (db.to_string(), table.to_string()),
            None => (current_db.to_string(), table.to_string()),
        };
        let info = is?.table_by_name(&db, &table)?;
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
            .map(|c| c.field_type.tp)
    };
    match (tp(lhs), tp(rhs)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

fn resolve_conjunct_side(
    expr: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
    left_tables: &std::collections::HashSet<String>,
    right_tables: &std::collections::HashSet<String>,
) -> Option<Side> {
    let cols = expr.referenced_columns();
    if cols.is_empty() {
        return None;
    }
    let mut left = true;
    let mut right = true;
    for (table, name) in cols {
        let on_left = owner_of(table.as_deref(), &name, left_map, left_tables).is_some();
        let on_right = owner_of(table.as_deref(), &name, right_map, right_tables).is_some();
        if !on_left {
            left = false;
        }
        if !on_right {
            right = false;
        }
        if !on_left && !on_right {
            return None;
        }
    }
    match (left, right) {
        (true, false) => Some(Side::Left),
        (false, true) => Some(Side::Right),
        _ => None,
    }
}

/// `col = col` with one column on each side, rewritten to qualified column
/// references so the executor can build the hash key.
fn qualify_cross_equality(
    expr: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
    left_tables: &std::collections::HashSet<String>,
    right_tables: &std::collections::HashSet<String>,
) -> Option<ExprNode> {
    let ExprNode::Binary(binary) = expr else {
        return None;
    };
    if binary.op != sealdb_parser::opcode::Op::Eq {
        return None;
    }
    // Row comparisons (`(a, b) = (c, d)`) are not scalar hash keys: they must
    // stay a residual filter and be evaluated column by column.
    if matches!(binary.left.as_ref(), ExprNode::Row(_))
        || matches!(binary.right.as_ref(), ExprNode::Row(_))
    {
        return None;
    }
    // Expression keys (`t1.a + 1 = t2.a + 1`): each side must resolve wholly
    // to one join input, and the two expressions to opposite inputs. Their
    // columns are already qualified, so the predicate can become the hash key
    // unchanged.
    if !matches!(
        (binary.left.as_ref(), binary.right.as_ref()),
        (ExprNode::Column(_), ExprNode::Column(_))
    ) {
        let left_owner = expr_owned_by_side(&binary.left, left_map, left_tables)
            .or_else(|| expr_owned_by_side(&binary.left, right_map, right_tables))?;
        let right_owner = expr_owned_by_side(&binary.right, left_map, left_tables)
            .or_else(|| expr_owned_by_side(&binary.right, right_map, right_tables))?;
        let cross = (left_tables.contains(&left_owner) && right_tables.contains(&right_owner))
            || (right_tables.contains(&left_owner) && left_tables.contains(&right_owner));
        if !cross {
            return None;
        }
        return Some(expr.clone());
    }
    let (ExprNode::Column(l), ExprNode::Column(r)) = (binary.left.as_ref(), binary.right.as_ref())
    else {
        return None;
    };
    let (l_left, l_right) = (
        owner_of(l.table.as_deref(), &l.name, left_map, left_tables),
        owner_of(l.table.as_deref(), &l.name, right_map, right_tables),
    );
    let (r_left, r_right) = (
        owner_of(r.table.as_deref(), &r.name, left_map, left_tables),
        owner_of(r.table.as_deref(), &r.name, right_map, right_tables),
    );
    // Each column must be owned by exactly one side, and the two columns must
    // sit on opposite sides for this to be a usable hash key.
    let (l_owner, r_owner) = match (l_left, l_right, r_left, r_right) {
        (Some(l), None, None, Some(r)) => (Some(l), Some(r)),
        (None, Some(l), Some(r), None) => (Some(l), Some(r)),
        _ => return None,
    };
    let mut new = binary.clone();
    let mut left_col = l.clone();
    left_col.table = l_owner;
    let mut right_col = r.clone();
    right_col.table = r_owner;
    new.left = Box::new(ExprNode::Column(left_col));
    new.right = Box::new(ExprNode::Column(right_col));
    Some(ExprNode::Binary(new))
}

/// TiDB derived predicates across an inner-join equality key.
///
/// For each `a = b` in `ON` every single-side conjunct on `a` is mirrored onto
/// `b` (and vice versa), which is why unanalyzed TiDB plans show the same
/// filter on both sides of a join. The mirrored column is bound to the alias
/// that owns it inside the target subtree.
#[allow(clippy::too_many_arguments)]
/// TiDB `DeriveOtherConditions` / `deriveNotNullExpr`: a cross-side comparison
/// (`lt(t1.c_str, t2.c_str)`) rejects NULLs on each nullable column it reads.
fn derive_other_not_null(
    others: &[ExprNode],
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
    left_table: &str,
    right_table: &str,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> (Vec<ExprNode>, Vec<ExprNode>) {
    use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, IsNullExpr, UnaryOpExpr};
    use sealdb_parser::opcode::Op;

    fn not_null(table: &str, name: &str) -> ExprNode {
        ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(table.to_string()),
                    name: name.to_string(),
                })),
                not: false,
            })),
        })
    }
    fn column_nullable(is: Option<&InfoSchema>, current_db: &str, table: &str, name: &str) -> bool {
        let Some(is) = is else {
            return true;
        };
        let info = statement_table_info(Some(is), current_db, table).or_else(|| {
            is.all_schemas()
                .into_iter()
                .find_map(|db| is.table_by_name(db.name.o(), table))
        });
        let Some(info) = info else {
            return true;
        };
        let in_pk = info.indices.iter().any(|idx| {
            idx.primary
                && idx
                    .columns
                    .iter()
                    .any(|c| c.name.o().eq_ignore_ascii_case(name))
        });
        if in_pk {
            return false;
        }
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(name))
            .is_none_or(|c| c.field_type.flag & 1 == 0)
    }
    fn owns(
        map: &std::collections::HashMap<String, Vec<String>>,
        name: &str,
        fallback: &str,
    ) -> Option<String> {
        match map.get(&name.to_ascii_lowercase()) {
            Some(owners) if owners.len() == 1 => Some(owners[0].clone()),
            Some(_) => None,
            None if map.is_empty() => Some(fallback.to_string()),
            None => None,
        }
    }

    let mut dl = Vec::new();
    let mut dr = Vec::new();
    for expr in others {
        let ExprNode::Binary(bin) = expr else {
            continue;
        };
        // Only comparisons (`lt` / `gt` / …); equalities are join keys.
        if !matches!(bin.op, Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Ne) {
            continue;
        }
        let (ExprNode::Column(a), ExprNode::Column(b)) = (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        for col in [a, b] {
            let name = &col.name;
            let alias = col
                .table
                .clone()
                .or_else(|| owns(left_map, name, left_table))
                .or_else(|| owns(right_map, name, right_table));
            let Some(alias) = alias else {
                continue;
            };
            let on_left = alias.eq_ignore_ascii_case(left_table)
                || left_map
                    .get(&name.to_ascii_lowercase())
                    .is_some_and(|owners| owners.iter().any(|o| o.eq_ignore_ascii_case(&alias)));
            let on_right = alias.eq_ignore_ascii_case(right_table)
                || right_map
                    .get(&name.to_ascii_lowercase())
                    .is_some_and(|owners| owners.iter().any(|o| o.eq_ignore_ascii_case(&alias)));
            if !column_nullable(is, current_db, &alias, name) {
                continue;
            }
            let reject = not_null(&alias, name);
            if on_left
                && !dl
                    .iter()
                    .any(|e: &ExprNode| e.restore_string() == reject.restore_string())
            {
                dl.push(reject.clone());
            }
            if on_right
                && !dr
                    .iter()
                    .any(|e: &ExprNode| e.restore_string() == reject.restore_string())
            {
                dr.push(reject);
            }
        }
    }
    (dl, dr)
}

fn derive_equal_cond_preds(
    on: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
    left_table: &str,
    right_table: &str,
    left_single: bool,
    right_single: bool,
    left_preds: &[ExprNode],
    right_preds: &[ExprNode],
) -> (Vec<ExprNode>, Vec<ExprNode>) {
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ExprNode, UnaryOpExpr};

    fn single_col(e: &ExprNode) -> Option<ColumnNameExpr> {
        match e {
            ExprNode::Column(c) => Some(c.clone()),
            ExprNode::Binary(b) => match (b.left.as_ref(), b.right.as_ref()) {
                (ExprNode::Column(c), other) | (other, ExprNode::Column(c))
                    if other.referenced_columns().is_empty() =>
                {
                    Some(c.clone())
                }
                _ => None,
            },
            ExprNode::Unary(u) => single_col(&u.expr),
            _ => None,
        }
    }
    /// An alias-loaded subtree may be absent from the infoschema (the alias
    /// itself is not a table name); an empty map then means "unknown", not
    /// "not owned".
    fn owns(
        map: &std::collections::HashMap<String, Vec<String>>,
        name: &str,
        single: bool,
    ) -> bool {
        single || map.is_empty() || map.contains_key(&name.to_ascii_lowercase())
    }
    /// Alias that owns `name` in the given subtree (`None` → cannot derive).
    /// Falls back to the join side's own alias when columns are unresolvable.
    fn alias_for(
        map: &std::collections::HashMap<String, Vec<String>>,
        name: &str,
        fallback: &str,
        single: bool,
    ) -> Option<String> {
        if single {
            return Some(fallback.to_string());
        }
        match map.get(&name.to_ascii_lowercase()) {
            Some(owners) if owners.len() == 1 => Some(owners[0].clone()),
            Some(_) => None,
            None if map.is_empty() => Some(fallback.to_string()),
            None => None,
        }
    }
    fn replace(e: &ExprNode, from: &str, to: &str, alias: &str) -> Option<ExprNode> {
        if e.referenced_columns().is_empty() {
            return Some(e.clone());
        }
        match e {
            ExprNode::Column(c) if c.name.eq_ignore_ascii_case(from) => {
                Some(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(alias.to_string()),
                    name: to.to_string(),
                }))
            }
            ExprNode::Binary(b) => Some(ExprNode::Binary(BinaryOpExpr {
                op: b.op,
                left: Box::new(replace(&b.left, from, to, alias)?),
                right: Box::new(replace(&b.right, from, to, alias)?),
                single_cnf_item: false,
            })),
            ExprNode::Unary(u) => Some(ExprNode::Unary(UnaryOpExpr {
                op: u.op,
                expr: Box::new(replace(&u.expr, from, to, alias)?),
            })),
            ExprNode::IsNull(n) => Some(ExprNode::IsNull(
                sealdb_parser::ast::expressions::IsNullExpr {
                    expr: Box::new(replace(&n.expr, from, to, alias)?),
                    not: n.not,
                },
            )),
            _ => None,
        }
    }

    let mut dl = Vec::new();
    let mut dr = Vec::new();
    for item in on.split_cnf() {
        let ExprNode::Binary(bin) = &item else {
            continue;
        };
        if bin.op != sealdb_parser::opcode::Op::Eq {
            continue;
        }
        let (ExprNode::Column(a), ExprNode::Column(b)) = (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        for p in left_preds {
            let Some(c) = single_col(p) else { continue };
            if !owns(left_map, &c.name, left_single) {
                continue;
            }
            let (from, to) =
                if c.name.eq_ignore_ascii_case(&a.name) && !c.name.eq_ignore_ascii_case(&b.name) {
                    (a.name.clone(), b.name.clone())
                } else if c.name.eq_ignore_ascii_case(&b.name) {
                    (b.name.clone(), a.name.clone())
                } else {
                    continue;
                };
            let Some(alias) = alias_for(right_map, &to, right_table, right_single) else {
                continue;
            };
            if let Some(d) = replace(p, &from, &to, &alias) {
                dr.push(d);
            }
        }
        for p in right_preds {
            let Some(c) = single_col(p) else { continue };
            if !owns(right_map, &c.name, right_single) {
                continue;
            }
            let (from, to) =
                if c.name.eq_ignore_ascii_case(&b.name) && !c.name.eq_ignore_ascii_case(&a.name) {
                    (b.name.clone(), a.name.clone())
                } else if c.name.eq_ignore_ascii_case(&a.name) {
                    (a.name.clone(), b.name.clone())
                } else {
                    continue;
                };
            let Some(alias) = alias_for(left_map, &to, left_table, left_single) else {
                continue;
            };
            if let Some(d) = replace(p, &from, &to, &alias) {
                dl.push(d);
            }
        }
    }
    (dl, dr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};
    use sealdb_parser::opcode::Op;

    fn col_eq(table: &str, name: &str, value: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(table.to_string()),
                name: name.to_string(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(value),
            })),
            single_cnf_item: false,
        })
    }

    #[test]
    fn test_push_into_table_reader() {
        let plan = predicate_push_down(
            PhysicalPlan::table_reader("t", Vec::new()),
            &col_eq("t", "id", 1),
            None,
            "db1",
        );
        match plan {
            PhysicalPlan::TableReader {
                pushed: Some(filter),
                ..
            } => assert!(filter.restore_string().contains("id")),
            other => panic!("expected pushed TableReader, got {other:?}"),
        }
    }

    #[test]
    fn test_inner_join_splits_predicates() {
        let join = PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::Inner,
            left: Box::new(PhysicalPlan::table_reader("t1", Vec::new())),
            right: Box::new(PhysicalPlan::table_reader("t2", Vec::new())),
            left_table: "t1".to_string(),
            right_table: "t2".to_string(),
            on: col_eq("t1", "id", 1),
        };
        let filter =
            ExprNode::compose_cnf(vec![col_eq("t1", "id", 1), col_eq("t2", "id", 2)]).expect("and");
        let plan = predicate_push_down(join, &filter, None, "db1");
        let PhysicalPlan::HashJoin { left, right, .. } = plan else {
            panic!("expected hash join, got {plan:?}");
        };
        assert!(matches!(
            *left,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
        assert!(matches!(
            *right,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
    }

    fn col_cmp(table: &str, name: &str, op: Op, value: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(table.to_string()),
                name: name.to_string(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(value),
            })),
            single_cnf_item: false,
        })
    }

    fn col_is_null(table: &str, name: &str, not: bool) -> ExprNode {
        ExprNode::IsNull(sealdb_parser::ast::expressions::IsNullExpr {
            expr: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(table.to_string()),
                name: name.to_string(),
            })),
            not,
        })
    }

    fn right_join() -> PhysicalPlan {
        PhysicalPlan::HashJoin {
            est_rows: None,
            tp: JoinType::RightOuter,
            left: Box::new(PhysicalPlan::table_reader("t1", Vec::new())),
            right: Box::new(PhysicalPlan::table_reader("t2", Vec::new())),
            left_table: "t1".to_string(),
            right_table: "t2".to_string(),
            on: col_eq("t1", "id", 1),
        }
    }

    #[test]
    fn test_null_rejecting_on_null_supplying_side_turns_inner() {
        // `t1 RIGHT JOIN t2 … WHERE t1.id > 0`: `t1` is null-supplying, and the
        // predicate rejects the padded NULL row, so TiDB simplifies to an inner
        // join and pushes it into `t1`.
        let filter = col_cmp("t1", "id", Op::Gt, 0);
        let plan = predicate_push_down(right_join(), &filter, None, "db1");
        let PhysicalPlan::HashJoin {
            tp, left, right, ..
        } = plan
        else {
            panic!("expected hash join, got {plan:?}");
        };
        assert!(
            matches!(tp, JoinType::Inner),
            "expected inner join, got {tp:?}"
        );
        assert!(matches!(
            *left,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
        assert!(matches!(
            *right,
            PhysicalPlan::TableReader { pushed: None, .. }
        ));
    }

    #[test]
    fn test_null_accepting_predicate_stays_above_right_join() {
        // `t1 RIGHT JOIN t2 … WHERE t1.id IS NULL` keeps the padded rows, so
        // the predicate must stay above the outer join.
        let filter = col_is_null("t1", "id", false);
        let plan = predicate_push_down(right_join(), &filter, None, "db1");
        let PhysicalPlan::Selection { child, .. } = plan else {
            panic!("expected selection above the join, got {plan:?}");
        };
        let PhysicalPlan::HashJoin { tp, .. } = *child else {
            panic!("expected hash join, got {child:?}");
        };
        assert!(matches!(tp, JoinType::RightOuter), "got {tp:?}");
    }
}

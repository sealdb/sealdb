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
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr));
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
                    index: None,
                    index_columns: Vec::new(),
                    partition_names,
                    global_index: false,
                    lock: false,
                    common_handle: false,
                };
            }
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
                    pushed: and_opt(
                        ExprNode::compose_cnf(pushable).map(simplify_or_arms),
                        pushed,
                    ),
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
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            pushed,
            range_scan,
            ..
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr));
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
            // A predicate TiDB cannot push into cop (`tidb_shard(a) = 8`) can
            // still become the index access range: the ranger detaches it from
            // the filter and the scan seeks.
            let mut residual = residual;
            let range_candidates: Vec<ExprNode> = pushable
                .iter()
                .chain(residual.iter())
                .cloned()
                .collect();
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
            // the index; the rest filter the probed table rows (Selection(Probe)).
            let on_probe = |expr: &ExprNode| {
                !expr
                    .referenced_columns()
                    .iter()
                    .all(|(_, col)| index_columns.iter().any(|c| c.eq_ignore_ascii_case(col)))
            };
            let (index_side, probe_side): (Vec<_>, Vec<_>) =
                if pushed.is_some() && access_range.is_none() {
                    (pushable, Vec::new())
                } else {
                    pushable.into_iter().partition(|expr| !on_probe(expr))
                };
            // Build-side residual predicates (columns in the index but not
            // encoded by the range) stay a cop `Selection(Build)`.
            if access_range.is_some()
                && let Some(filter) = ExprNode::compose_cnf(index_side.clone())
            {
                if let Some(sel) = crate::core::explain::filter_selectivity(&filter) {
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
            let probe_est = probe_filter.as_ref().map_or(0.0, |filter| {
                table_info
                    .as_ref()
                    .and_then(|info| unique_eq_rows(info, filter))
                    .unwrap_or_else(|| {
                        // TiDB sizes the `TableRowIDScan`'s filter from the
                        // table's column statistics (`cardinality.Selectivity`),
                        // so `c3 < 1` keeps its pseudo range rate over the rows
                        // the index range produced.
                        let sel = sealdb_statistics::pseudo::selectivity(&probe_side, 10_000.0, None)
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
                    partition_access,
                    row_count,
                    selection_info,
                    probe_selection_info,
                    probe_est,
                    range_scan: access_range,
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
            partition_access,
            row_count,
            pushed,
            range_scan,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr));
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
            let pushed = pushed;
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
                let range_candidates: Vec<ExprNode> = pushable
                    .iter()
                    .chain(residual.iter())
                    .cloned()
                    .collect();
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
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) = conjuncts
                .into_iter()
                .partition(|expr| !expr.has_subquery() && cop_pushable(expr));
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
                },
                residual,
            )
        }
        PhysicalPlan::PointGet {
            table,
            handle,
            index,
            index_columns,
            partition_names,
            global_index,
            lock,
            common_handle,
        } => {
            // Only the equality on the handle column itself is consumed by a
            // handle point get (`a = 1 AND b = 1` keeps `b = 1` as a filter).
            let handle_column = (handle.is_some() && index.is_none())
                .then(|| {
                    statement_table_info(is, current_db, &table)
                        .and_then(|info| info.handle_column())
                        .map(|c| c.name.o().to_string())
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
            let key_columns = point_get_key_columns(
                is,
                current_db,
                &table,
                index.as_deref(),
                None,
            );
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
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
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
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left_cols = table_columns(is, current_db, &left_table);
            let right_cols = table_columns(is, current_db, &right_table);
            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut residual = Vec::new();
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
                    Side::Residual => residual.push(expr),
                }
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
                // HashJoin derives its hash key from the first equality in
                // `ON`; further equalities stay as a filter above the join so
                // they are still enforced (inner joins are filter-equivalent).
                let mut key_taken = !is_true_expr(&on);
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
                        Some(Side::Residual) | None => {
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
                                if key_taken || !same_join_key_type(&key, is, current_db) {
                                    keep.push(key);
                                } else {
                                    // CROSS JOIN's placeholder `ON TRUE` gives
                                    // way to the equality key.
                                    on = key;
                                    tp = JoinType::Inner;
                                    key_taken = true;
                                }
                            } else {
                                keep.push(expr);
                            }
                        }
                    }
                }
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
            // A range can only answer a predicate on a column the memory table
            // actually exposes; anything else stays above as a `Selection`.
            let columns = sealdb_infoschema::tables::virtual_columns(&table);
            let mut ranges: Vec<(String, String)> = already;
            let mut residual = Vec::new();
            for conjunct in conjuncts {
                match mem_table_range(&conjunct) {
                    Some((column, values))
                        if columns
                            .iter()
                            .any(|c| c.eq_ignore_ascii_case(&column)) =>
                    {
                        ranges.extend(values.into_iter().map(|v| (column.clone(), v)));
                    }
                    _ => residual.push(conjunct),
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
    is.table_by_name(&db, table)
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
fn cop_pushable(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(func) => {
            !func.name.eq_ignore_ascii_case("tidb_shard")
                && func.args.iter().all(cop_pushable)
        }
        ExprNode::Binary(bin) => cop_pushable(&bin.left) && cop_pushable(&bin.right),
        ExprNode::Unary(unary) => cop_pushable(&unary.expr),
        ExprNode::Cast(cast) => cop_pushable(&cast.expr),
        ExprNode::InList(list) => cop_pushable(&list.expr) && list.list.iter().all(cop_pushable),
        ExprNode::Between(between) => {
            cop_pushable(&between.expr) && cop_pushable(&between.low) && cop_pushable(&between.high)
        }
        ExprNode::IsNull(is_null) => cop_pushable(&is_null.expr),
        ExprNode::Like(like) => cop_pushable(&like.expr) && cop_pushable(&like.pattern),
        ExprNode::Case(case) => {
            case.operand.as_ref().is_none_or(|o| cop_pushable(o))
                && case
                    .when_then
                    .iter()
                    .all(|(when, then)| cop_pushable(when) && cop_pushable(then))
                && case.else_expr.as_ref().is_none_or(|e| cop_pushable(e))
        }
        _ => true,
    }
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
        None => handle_column.map(|c| vec![c.to_string()]).unwrap_or_default(),
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
    if arms.len() < 2 || !arms.iter().all(|arm| arm.split_cnf_all().iter().any(|c| is_point_get_key(c, key_columns))) {
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
    Some((
        column.to_ascii_lowercase(),
        vec![mem_table_value(value)?],
    ))
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

fn wrap_residual(plan: PhysicalPlan, residual: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(residual) else {
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
        PhysicalPlan::Selection {
            child: Box::new(plan),
            filter,
            est_rows: None,
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
        (Some(left), Some(right)) => ExprNode::compose_cnf(vec![left, right]),
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
    Residual,
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
        (true, false, _) => Side::Left,
        (false, true, JoinType::Inner | JoinType::Cross | JoinType::Semi | JoinType::Anti) => {
            Side::Right
        }
        (false, true, JoinType::RightOuter) => Side::Right,
        (false, true, JoinType::LeftOuter) => Side::Residual,
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
            Some(owners) => owners.iter().any(|owner| *owner == table).then_some(table),
            None => Some(table),
        };
    }
    let owners = map.get(&col.to_ascii_lowercase())?;
    (owners.len() == 1).then(|| owners[0].clone())
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

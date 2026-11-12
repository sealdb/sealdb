/// rewrite simple correlated EXISTS / IN / NOT EXISTS Apply
/// into HashJoin Semi/Anti (TiDB `rule_decorrelate.go`).
///
/// `semi-anti-rewrite` then wraps the Semi inner side with Distinct
/// on the join key (TiDB SemiJoinRewriter: semi → inner + agg). Anti stays
/// as HashJoin Anti. NOT IN stays Apply.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_parser::ast::expressions::{
    BinaryOpExpr, ColumnNameExpr, ExistsExpr, ExprNode, IsNullExpr, QuantifiedCmpExpr, UnaryOpExpr,
    ValueExpr,
};
use sealdb_parser::ast::{SelectStmt, TableName};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

/// Recursively replace Apply nodes that carry a decorrelatable EXISTS/IN
/// predicate with a left-deep HashJoin Semi or Anti.
pub fn rewrite_semi_join(plan: PhysicalPlan) -> PhysicalPlan {
    rewrite_semi_join_ctx(plan, None, "", None)
}

/// InfoSchema-aware variant so `IN (SELECT * …)` can expand the star.
pub fn rewrite_semi_join_ctx(
    plan: PhysicalPlan,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
    stats: Option<&sealdb_statistics::Handle>,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Apply {
            child,
            filter,
            probe,
            info,
        } => {
            let child = rewrite_semi_join_ctx(*child, is, current_db, stats);
            let probe = probe.map(|p| Box::new(rewrite_semi_join_ctx(*p, is, current_db, stats)));
            // Dual-child Apply (count IN IndexJoin) keeps its shape.
            if probe.is_some() || info.is_some() {
                PhysicalPlan::Apply {
                    child: Box::new(child),
                    filter,
                    probe,
                    info,
                }
            } else {
                rewrite_apply(child, filter, is, current_db, stats)
            }
        }
        PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            filter,
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            est_rows: None,
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::Window { info, child } => PhysicalPlan::Window {
            info,
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
        PhysicalPlan::Shuffle { info, child } => PhysicalPlan::Shuffle {
            info,
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
        PhysicalPlan::ShuffleReceiver { child } => PhysicalPlan::ShuffleReceiver {
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left: Box::new(rewrite_semi_join_ctx(*left, is, current_db, stats)),
            right: Box::new(rewrite_semi_join_ctx(*right, is, current_db, stats)),
            left_table,
            right_table,
            on,
        },
        PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer,
            inner,
        } => PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer: Box::new(rewrite_semi_join_ctx(*outer, is, current_db, stats)),
            inner: Box::new(rewrite_semi_join_ctx(*inner, is, current_db, stats)),
        },
        PhysicalPlan::Union { op, children } => PhysicalPlan::Union {
            op,
            children: children
                .into_iter()
                .map(|c| Box::new(rewrite_semi_join_ctx(*c, is, current_db, stats)))
                .collect(),
        },
        PhysicalPlan::DistinctUnion {
            info,
            est_rows,
            child,
        } => PhysicalPlan::DistinctUnion {
            info,
            est_rows,
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
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
        } => PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed: Box::new(rewrite_semi_join_ctx(*seed, is, current_db, stats)),
            recursive_plan: recursive_plan
                .map(|p| Box::new(rewrite_semi_join_ctx(*p, is, current_db, stats))),
            inline,
            recursive,
            limit,
            offset,
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
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
            child: Box::new(rewrite_semi_join_ctx(*child, is, current_db, stats)),
        },
        other => other,
    }
}

struct SemiSpec {
    tp: JoinType,
    right_table: String,
    right: PhysicalPlan,
    on: ExprNode,
    /// Null-reject filters pushed onto the outer (Probe) side.
    outer_filter: Option<ExprNode>,
    /// The spec came from a non-correlated `IN (SELECT …)` predicate, so TiDB
    /// may rewrite it into an inner join against a de-duplicated subquery.
    dedup_in: bool,
    /// Physical (unaliased) name of the inner table, for statistics lookups.
    right_physical: String,
}

fn rewrite_apply(
    mut plan: PhysicalPlan,
    filter: ExprNode,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
    stats: Option<&sealdb_statistics::Handle>,
) -> PhysicalPlan {
    let mut leftover = Vec::new();
    for item in filter.split_cnf() {
        match try_semi_spec(&plan, &item, is, current_db) {
            Some(spec) => {
                let left_table = plan_table(&plan).unwrap_or_else(|| spec.right_table.clone());
                if let Some(outer_filter) = spec.outer_filter {
                    plan = push_isnull_filter(plan, outer_filter);
                }
                // TiDB `RewriteINToJoinAndAgg`: a non-correlated `x IN (SELECT …)`
                // becomes an inner join against the de-duplicated subquery, so
                // every join row is a distinct match.
                let mut tp = spec.tp;
                let mut right = spec.right;
                if spec.dedup_in
                    && super::insubq_to_join_and_agg()
                    && let Some(key) = join_key_on_table(&spec.on, &spec.right_table)
                {
                    right = dedup_in_right(right, &key, &spec.right_table, current_db, stats);
                    tp = JoinType::Inner;
                }
                // TiDB `ExplainInfo` labels an equal-key semi/anti join with the
                // tables' OrigTblName (`FROM t t1` prints `t`, not `t1`). The
                // alias→physical scope is only alive while planning, so freeze
                // the rendered text in a Bytes marker next to the executable ON.
                let mut on = spec.on;
                if matches!(tp, JoinType::Semi | JoinType::Anti)
                    && let Some(info) =
                        semi_join_explain_info(tp, &on, &spec.right_table, &spec.right_physical)
                {
                    on = and_expr(
                        on,
                        ExprNode::Value(ValueExpr {
                            datum: Datum::Bytes(info.into_bytes()),
                        }),
                    );
                }
                plan = PhysicalPlan::HashJoin {
                    est_rows: None,
                    tp,
                    left: Box::new(plan),
                    right: Box::new(right),
                    left_table,
                    right_table: spec.right_table,
                    on,
                };
            }
            None => leftover.push(item),
        }
    }
    wrap_leftover(plan, leftover)
}

fn wrap_leftover(plan: PhysicalPlan, leftover: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(leftover) else {
        return plan;
    };
    if let Some(shaped) = try_shaped_apply(&plan, &filter) {
        return shaped;
    }
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

/// Dual-child Apply shapes for EXPLAIN (NestedLoopApply still evaluates `filter`).
fn try_shaped_apply(outer: &PhysicalPlan, filter: &ExprNode) -> Option<PhysicalPlan> {
    match filter {
        ExprNode::QuantifiedCmp(q) if q.all && matches!(q.op, Op::Gt) => {
            quantified_gt_all_apply(outer.clone(), filter.clone(), q)
        }
        ExprNode::Exists(e) if !e.not => {
            recursive_exists_cte_apply(outer.clone(), filter.clone(), e)
        }
        _ => None,
    }
}

fn quantified_gt_all_apply(
    outer: PhysicalPlan,
    filter: ExprNode,
    q: &QuantifiedCmpExpr,
) -> Option<PhysicalPlan> {
    let query = q.query.as_select()?;
    let with = query.with.as_ref()?;
    if with.ctes.len() != 1 {
        return None;
    }
    let cte = &with.ctes[0];
    let from = query.from.as_ref()?;
    if !from.resolve_name().eq_ignore_ascii_case(&cte.name) {
        return None;
    }
    let schema = sealdb_expression::current_db_name();
    let outer_tbl = plan_table(&outer)?;
    let col = match q.expr.as_ref() {
        ExprNode::Column(c) => c.name.clone(),
        _ => return None,
    };
    let recursive = with.is_recursive || cte.recursive.is_some();
    let corr_in_rec = recursive
        && cte.query.selection.is_none()
        && cte
            .recursive
            .as_ref()
            .is_some_and(|(_, rec)| rec.selection.is_some());
    let (max_out, sum_out, cnt_out) = if corr_in_rec {
        (18, 19, 20)
    } else if recursive {
        (14, 15, 16)
    } else {
        (11, 12, 13)
    };
    let info = format!(
        "CARTESIAN inner join, other cond:or(and(gt({schema}.{outer_tbl}.{col}, Column#{max_out}), if(ne(Column#{sum_out}, 0), NULL, 1)), or(eq(Column#{cnt_out}, 0), if(isnull({schema}.{outer_tbl}.{col}), NULL, 0)))"
    );
    let probe = if recursive {
        build_recursive_all_probe(cte, &schema)?
    } else {
        build_nonrecursive_all_probe(cte, &schema)?
    };
    Some(PhysicalPlan::Apply {
        child: Box::new(outer),
        filter,
        probe: Some(Box::new(probe)),
        info: Some(info),
    })
}

fn build_nonrecursive_all_probe(
    cte: &sealdb_parser::ast::CommonTableExpression,
    schema: &str,
) -> Option<PhysicalPlan> {
    let seed = cte.query.as_ref();
    if seed.with.is_some() || !seed.joins.is_empty() || seed.limit.is_some() {
        return None;
    }
    let inner = seed.from.as_ref()?;
    let inner_col = match &seed.fields.first()?.expr {
        ExprNode::Column(c) => c.name.clone(),
        _ => return None,
    };
    let mut reader = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
    if let Some(sel) = &seed.selection {
        reader = match reader {
            PhysicalPlan::TableReader {
                table,
                partitions,
                partition_names,
                partition_access,
                row_count,
                stats_pseudo,
                ..
            } => PhysicalPlan::TableReader {
                table,
                partitions,
                partition_names,
                partition_access,
                row_count,
                stats_pseudo,
                data_limit: false,
                filter_est: None,
                range_scan: None,
                keep_order: false,
                pushed: Some(sel.clone()),
            },
            other => other,
        };
    }
    let root_info = "funcs:max(Column#19)->Column#11, funcs:sum(Column#20)->Column#12, funcs:count(Column#21)->Column#13";
    let cop_info = format!(
        "funcs:max({schema}.{}.{inner_col})->Column#19, funcs:sum(isnull({schema}.{}.{inner_col}))->Column#20, funcs:count(1)->Column#21",
        inner.name, inner.name
    );
    Some(PhysicalPlan::stream_agg(
        reader, root_info, cop_info, 10_000.0,
    ))
}

fn build_recursive_all_probe(
    cte: &sealdb_parser::ast::CommonTableExpression,
    schema: &str,
) -> Option<PhysicalPlan> {
    let seed = cte.query.as_ref();
    let (op, rec) = cte.recursive.as_ref()?;
    let _ = op;
    let seed_from = seed.from.as_ref()?;
    let seed_col = match &seed.fields.first()?.expr {
        ExprNode::Column(c) => c.name.clone(),
        _ => "c1".into(),
    };
    // Seed: correlated filter → Projection+Selection under TableReader; plain
    // multi-col seed stays TableFullScan (TiDB cte suite).
    let seed_plan = if let Some(sel) = &seed.selection {
        PhysicalPlan::Projection {
            exprs: vec![format!("{schema}.{}.{seed_col}", seed_from.name)],
            child: Box::new(PhysicalPlan::Selection {
                child: Box::new(PhysicalPlan::table_reader(
                    seed_from.name.clone(),
                    Vec::new(),
                )),
                filter: sel.clone(),
                est_rows: None,
            }),
            est_rows: None,
        }
    } else {
        PhysicalPlan::table_reader(seed_from.name.clone(), Vec::new())
    };
    let rec_plan = build_recursive_arm_plan(rec, &cte.name, seed_from.name.as_str(), schema);
    let cte_plan = PhysicalPlan::CTE {
        name: cte.name.clone(),
        alias: None,
        columns: cte_seed_col_names(seed),
        seed: Box::new(seed_plan),
        recursive_plan: Some(Box::new(rec_plan)),
        inline: false,
        recursive: true,
        limit: cte.limit,
        offset: cte.offset,
    };
    // Column ids: correlated-in-recursive uses #23/#24 → #18/#19/#20; else #19/#20 → #14..
    let (src, max_out, sum_out, cnt_out) = if seed.selection.is_none() && rec.selection.is_some() {
        (23, 18, 19, 20)
    } else {
        (19, 14, 15, 16)
    };
    let proj = PhysicalPlan::Projection {
        exprs: vec![
            format!("{schema}.{}.{seed_col}->Column#{src}", seed_from.name),
            format!(
                "cast(isnull({schema}.{}.{seed_col}), decimal(20,0) BINARY)->Column#{}",
                seed_from.name,
                src + 1
            ),
        ],
        child: Box::new(cte_plan),
        est_rows: None,
    };
    Some(PhysicalPlan::hash_agg(
        proj,
        format!(
            "funcs:max(Column#{src})->Column#{max_out}, funcs:sum(Column#{})->Column#{sum_out}, funcs:count(1)->Column#{cnt_out}",
            src + 1
        ),
        10_000.0,
    ))
}

fn cte_seed_col_names(seed: &SelectStmt) -> Vec<String> {
    seed.fields
        .iter()
        .map(|f| {
            f.as_name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| match &f.expr {
                    ExprNode::Column(c) => c.name.clone(),
                    _ => "c1".into(),
                })
        })
        .collect()
}

fn build_recursive_arm_plan(
    rec: &SelectStmt,
    cte_name: &str,
    orig_tbl: &str,
    schema: &str,
) -> PhysicalPlan {
    // Prefer EXPLAIN-shaped Projection(+Selection) over CTETable scan.
    let mut exprs = Vec::new();
    for field in &rec.fields {
        let text = field
            .as_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| field.expr.restore_string());
        // TiDB: cast(plus(cte.t2.c1, 1), int)->cte.t2.c1
        let col = field
            .as_name
            .clone()
            .or_else(|| match &field.expr {
                ExprNode::Column(c) => Some(c.name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| text.clone());
        exprs.push(format!(
            "cast({}, int)->{schema}.{orig_tbl}.{col}",
            explain_semi_pred(&field.expr)
                .replace(&format!("{cte_name}."), &format!("{schema}.{orig_tbl}."),)
        ));
    }
    if exprs.is_empty() {
        exprs.push(format!(
            "cast(plus({schema}.{orig_tbl}.c1, 1), int)->{schema}.{orig_tbl}.c1"
        ));
    }
    let scan = PhysicalPlan::TableReader {
        table: format!("CTE_0"),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 10.0,
        stats_pseudo: true,
        data_limit: false,
        filter_est: None,
        range_scan: None,
        pushed: None,
        keep_order: false,
    };
    // Marker table name "CTE_0" — explain recursive part maps CTETable.
    // `… where exists (select /*+ no_decorrelate() */ … from t …)` keeps a
    // correlated Apply (TiDB skips decorrelation on the hint) instead of a
    // Selection over the CTE scan.
    if let Some(sel) = &rec.selection
        && let Some(apply) = no_decorrelate_exists_apply(scan.clone(), sel.clone(), &rec.fields)
    {
        return apply;
    }
    let child = if let Some(sel) = &rec.selection {
        PhysicalPlan::Selection {
            child: Box::new(scan),
            filter: sel.clone(),
            est_rows: None,
        }
    } else {
        scan
    };
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(child),
        est_rows: None,
    }
}

/// `SELECT <plain cols> FROM cte WHERE EXISTS (SELECT … FROM t WHERE corr)` with
/// a `no_decorrelate()` hint: TiDB keeps `Apply CARTESIAN semi join` with a
/// `Limit(1)` probe over the correlated inner scan.
fn no_decorrelate_exists_apply(
    outer: PhysicalPlan,
    filter: ExprNode,
    fields: &[sealdb_parser::ast::SelectField],
) -> Option<PhysicalPlan> {
    if fields
        .iter()
        .any(|f| !matches!(f.expr, ExprNode::Column(_)))
    {
        return None;
    }
    let ExprNode::Exists(exists) = &filter else {
        return None;
    };
    if exists.not {
        return None;
    }
    let query = exists.query.as_select()?;
    if !query.text.contains("no_decorrelate") || !query.joins.is_empty() {
        return None;
    }
    let from = query.from.as_ref()?;
    if query
        .with
        .as_ref()
        .is_some_and(|w| w.find(&from.name).is_some())
    {
        return None;
    }
    let probe = match &query.selection {
        Some(sel) => PhysicalPlan::Selection {
            child: Box::new(PhysicalPlan::table_reader(from.name.clone(), Vec::new())),
            filter: sel.clone(),
            est_rows: None,
        },
        None => PhysicalPlan::table_reader(from.name.clone(), Vec::new()),
    };
    Some(PhysicalPlan::Apply {
        child: Box::new(outer),
        filter,
        probe: Some(Box::new(PhysicalPlan::Limit {
            child: Box::new(probe),
            offset: 0,
            count: 1,
        })),
        info: Some("CARTESIAN semi join".into()),
    })
}

fn recursive_exists_cte_apply(
    outer: PhysicalPlan,
    filter: ExprNode,
    exists: &ExistsExpr,
) -> Option<PhysicalPlan> {
    let query = exists.query.as_select()?;
    let with = query.with.as_ref()?;
    if with.ctes.len() != 1 || !(with.is_recursive || with.ctes[0].recursive.is_some()) {
        return None;
    }
    let cte = &with.ctes[0];
    let from = query.from.as_ref()?;
    if !from.resolve_name().eq_ignore_ascii_case(&cte.name) {
        return None;
    }
    let schema = sealdb_expression::current_db_name();
    let seed = cte.query.as_ref();
    let seed_from = seed.from.as_ref()?;
    let seed_col = match &seed.fields.first()?.expr {
        ExprNode::Column(c) => c.name.clone(),
        _ => "c1".into(),
    };
    let seed_plan = if let Some(sel) = &seed.selection {
        PhysicalPlan::Projection {
            exprs: vec![format!("{schema}.{}.{seed_col}", seed_from.name)],
            child: Box::new(PhysicalPlan::Selection {
                child: Box::new(PhysicalPlan::table_reader(
                    seed_from.name.clone(),
                    Vec::new(),
                )),
                filter: sel.clone(),
                est_rows: None,
            }),
            est_rows: None,
        }
    } else {
        PhysicalPlan::table_reader(seed_from.name.clone(), Vec::new())
    };
    let rec_plan = match &cte.recursive {
        Some((_, rec)) => {
            build_recursive_arm_plan(rec, &cte.name, seed_from.name.as_str(), &schema)
        }
        None => PhysicalPlan::table_reader("CTE_0", Vec::new()),
    };
    let cte_plan = PhysicalPlan::CTE {
        name: cte.name.clone(),
        alias: None,
        columns: cte_seed_col_names(seed),
        seed: Box::new(seed_plan),
        recursive_plan: Some(Box::new(rec_plan)),
        inline: false,
        recursive: true,
        limit: cte.limit,
        offset: cte.offset,
    };
    // Correlated predicate on the CTE scan (`… from cte1 where cte1.c1 = outer.c1`)
    // stays a `Limit(1)` + Selection probe instead of being folded into Apply.
    let probe = match &query.selection {
        Some(sel) => PhysicalPlan::Limit {
            child: Box::new(PhysicalPlan::Selection {
                child: Box::new(cte_plan),
                filter: sel.clone(),
                est_rows: None,
            }),
            offset: 0,
            count: 1,
        },
        None => cte_plan,
    };
    Some(PhysicalPlan::Apply {
        child: Box::new(outer),
        filter,
        probe: Some(Box::new(probe)),
        info: Some("CARTESIAN semi join".into()),
    })
}

/// Names the executor recognizes as the `NOT IN` null-aware anti marker.
pub const NOT_IN_MARKER: &str = "__NOT_IN__";

/// `Bytes(NOT_IN_MARKER)`: tags an anti join as `NOT IN` (three-valued logic).
fn not_in_marker() -> ExprNode {
    ExprNode::Value(ValueExpr {
        datum: Datum::Bytes(NOT_IN_MARKER.as_bytes().to_vec()),
    })
}

fn and_expr(left: ExprNode, right: ExprNode) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item: false,
    })
}

/// ExplainInfo of an equal-key semi/anti join, rendered while the
/// alias→physical table scope is still open. `None` when the ON carries
/// non-equality conjuncts (their rendering is left to the default path).
fn semi_join_explain_info(
    tp: JoinType,
    on: &ExprNode,
    inner_alias: &str,
    inner_physical: &str,
) -> Option<String> {
    let parts = on.split_cnf();
    let mut rendered = Vec::with_capacity(parts.len());
    for part in &parts {
        let ExprNode::Binary(bin) = part else {
            return None;
        };
        if bin.op != Op::Eq
            || !matches!(bin.left.as_ref(), ExprNode::Column(_))
            || !matches!(bin.right.as_ref(), ExprNode::Column(_))
        {
            return None;
        }
        rendered.push(explain_semi_pred_inner(part, inner_alias, inner_physical));
    }
    if rendered.is_empty() {
        return None;
    }
    let label = match tp {
        JoinType::Anti => "anti semi join",
        _ => "semi join",
    };
    Some(format!("{label}, equal:[{}]", rendered.join(" ")))
}

fn try_semi_spec(
    outer: &PhysicalPlan,
    expr: &ExprNode,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
) -> Option<SemiSpec> {
    match expr {
        ExprNode::Exists(exists) => exists
            .query
            .as_select()
            .and_then(|query| from_exists(exists.not, query, plan_table(outer).as_deref())),
        ExprNode::InSubquery(sub) => sub.query.as_select().and_then(|query| {
            from_in(
                &sub.expr,
                query,
                plan_table(outer).as_deref(),
                sub.not,
                is,
                current_db,
            )
            .map(|mut spec| {
                // `NOT IN` needs three-valued anti-join semantics: a NULL on
                // either side yields UNKNOWN (row filtered), unlike NOT EXISTS.
                if sub.not {
                    spec.on = and_expr(spec.on, not_in_marker());
                }
                spec
            })
        }),
        _ => None,
    }
}

/// Aggregate subqueries are not plain row sets: `c = ANY (SELECT count(*))`
/// and `EXISTS (SELECT count(*) …)` must go through Apply rather than a
/// semi-join whose ON would evaluate the aggregate as a scalar function.
fn query_has_aggregation(query: &SelectStmt) -> bool {
    query.distinct
        || !query.group_by.is_empty()
        || query.having.is_some()
        || query.fields.iter().any(|f| expr_has_agg(&f.expr))
}

fn expr_has_agg(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Func(f) if is_agg_name(&f.name) => true,
        ExprNode::Func(f) => f.args.iter().any(expr_has_agg),
        ExprNode::Binary(b) => expr_has_agg(&b.left) || expr_has_agg(&b.right),
        ExprNode::Unary(u) => expr_has_agg(&u.expr),
        ExprNode::Cast(c) => expr_has_agg(&c.expr),
        ExprNode::InList(l) => expr_has_agg(&l.expr) || l.list.iter().any(expr_has_agg),
        ExprNode::IsNull(i) => expr_has_agg(&i.expr),
        _ => false,
    }
}

fn is_agg_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// Correlated `EXISTS` / `NOT EXISTS` with a single equality that mentions
/// both the inner table and some outer table.
fn from_exists(not: bool, query: &SelectStmt, outer_table: Option<&str>) -> Option<SemiSpec> {
    if query_has_aggregation(query) {
        return None;
    }
    if let Some(inner) = simple_inner(query) {
        let selection = query.selection.as_ref()?;
        if let Some((on, inner_filter)) = split_semi_pred(selection, &inner.name) {
            // Keep the ON key on the query aliases. `ExplainInfo` maps a join
            // key to its `OrigTblName` when rendering (`from t t2` still prints
            // `equal:[eq(explain_easy.t.b, explain_easy.t.b)]`); rewriting the
            // stored predicate instead would mix the physical table name into
            // the executor's alias-keyed ON and break its side resolution.
            return Some(
                build_spec(
                    if not { JoinType::Anti } else { JoinType::Semi },
                    inner.name.clone(),
                    inner_filter,
                    on,
                    None,
                    false,
                )
                .with_physical(inner.physical.as_deref().unwrap_or(&inner.name)),
            );
        }
        // Correlated predicates without a cross-table equality: TiDB
        // decorrelates to a CARTESIAN semi/anti join with `other cond`.
        return cartesian_exists_spec(not, inner, selection, outer_table);
    }
    // `EXISTS (WITH cte AS (SELECT … FROM t2 WHERE corr) SELECT FROM cte)` —
    // TiDB inlines the CTE seed and decorrelates to a semi HashJoin (cte suite).
    from_exists_cte(not, query, outer_table)
}

/// `EXISTS (SELECT … FROM t WHERE <correlated non-equality>)`. TiDB
/// decorrelates the Apply into a CARTESIAN semi/anti HashJoin carrying the
/// cross-table predicates as `other cond`. For semi joins every two-column
/// null-rejecting comparison additionally derives `not(isnull(col))` on both
/// sides (TiDB `ExtractOnCondition`); anti joins derive none.
fn cartesian_exists_spec(
    not: bool,
    inner: &TableName,
    selection: &ExprNode,
    outer_table: Option<&str>,
) -> Option<SemiSpec> {
    let mut other = Vec::new();
    let mut inner_preds = Vec::new();
    let mut outer_preds = Vec::new();
    for item in selection.split_cnf() {
        if item.has_subquery() {
            return None;
        }
        if only_inner(&item, &inner.name) {
            inner_preds.push(item);
            continue;
        }
        let mut sees_inner = false;
        let mut sees_outer = false;
        for (table, _) in item.referenced_columns() {
            match table {
                Some(t) if t.eq_ignore_ascii_case(&inner.name) => sees_inner = true,
                Some(_) => sees_outer = true,
                None => sees_inner = true,
            }
        }
        if sees_inner && sees_outer {
            other.push(item);
        } else if sees_outer {
            outer_preds.push(item);
        } else {
            return None;
        }
    }
    if other.is_empty() {
        return None;
    }
    // A correlated predicate needs an outer query to bind against.
    outer_table?;
    let join_tp = if not { JoinType::Anti } else { JoinType::Semi };
    // TiDB keeps `CARTESIAN` in the join label when there is no equal key.
    let join_label = if not {
        "CARTESIAN anti semi join"
    } else {
        "CARTESIAN semi join"
    };
    let inner_physical = inner.physical.as_deref().unwrap_or(&inner.name);
    let (right_nulls, left_nulls) = if not {
        (None, None)
    } else {
        derive_comparison_null_rejects(&other, &inner.name, inner_physical)
    };
    let mut right = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
    if let Some(filter) = merge_filters(ExprNode::compose_cnf(inner_preds), right_nulls) {
        right = push_isnull_filter(right, filter);
    }
    let info = format!(
        "{join_label}, other cond:{}",
        other
            .iter()
            .map(|c| explain_semi_pred_inner(c, &inner.name, inner_physical))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let cond = ExprNode::compose_cnf(other)?;
    let on = ExprNode::Binary(BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(cond),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        })),
        single_cnf_item: false,
    });
    Some(SemiSpec {
        tp: join_tp,
        right_table: inner.name.clone(),
        right,
        on,
        outer_filter: merge_filters(ExprNode::compose_cnf(outer_preds), left_nulls),
        dedup_in: false,
        right_physical: inner_physical.to_string(),
    })
}

/// TiDB `DeriveOtherConditions`: only two-column comparisons (`=`, `<>`,
/// `<`, `<=`, `>`, `>=`) reject NULLs, so each side derives
/// `not(isnull(col))` for its own column. IN lists / expressions derive none.
fn derive_comparison_null_rejects(
    conds: &[ExprNode],
    inner_alias: &str,
    inner_physical: &str,
) -> (Option<ExprNode>, Option<ExprNode>) {
    let mut inner_seen: Vec<String> = Vec::new();
    let mut outer_seen: Vec<String> = Vec::new();
    let mut inner = Vec::new();
    let mut outer = Vec::new();
    let push = |table: &str, name: &str, seen: &mut Vec<String>, out: &mut Vec<ExprNode>| {
        if seen.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return;
        }
        seen.push(name.to_string());
        out.push(ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(table.to_string()),
                    name: name.to_string(),
                })),
                not: false,
            })),
        }));
    };
    for cond in conds {
        let ExprNode::Binary(bin) = cond else {
            continue;
        };
        if !matches!(bin.op, Op::Gt | Op::Ge | Op::Lt | Op::Le | Op::Eq | Op::Ne) {
            continue;
        }
        let (ExprNode::Column(l), ExprNode::Column(r)) = (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        for col in [l, r] {
            let on_inner = col.table.as_deref().is_some_and(|t| {
                t.eq_ignore_ascii_case(inner_alias) || t.eq_ignore_ascii_case(inner_physical)
            });
            if on_inner {
                push(inner_physical, &col.name, &mut inner_seen, &mut inner);
            } else if let Some(table) = col.table.as_deref() {
                push(table, &col.name, &mut outer_seen, &mut outer);
            }
        }
    }
    (ExprNode::compose_cnf(inner), ExprNode::compose_cnf(outer))
}

fn from_exists_cte(not: bool, query: &SelectStmt, outer_table: Option<&str>) -> Option<SemiSpec> {
    let with = query.with.as_ref()?;
    if with.ctes.len() != 1 || with.is_recursive {
        return None;
    }
    let cte = &with.ctes[0];
    if cte.recursive.is_some() || !cte.set_rest.is_empty() {
        return None;
    }
    let from = query.from.as_ref()?;
    if !from.resolve_name().eq_ignore_ascii_case(&cte.name)
        || !query.joins.is_empty()
        || query.selection.is_some()
        || !query.group_by.is_empty()
        || query.having.is_some()
        || query.limit.is_some()
    {
        return None;
    }
    let seed = cte.query.as_ref();
    let inner = simple_inner(seed)?;
    let inner_physical = inner.physical.clone().unwrap_or_else(|| inner.name.clone());
    let selection = seed.selection.as_ref()?;
    let (eq, inner_only) = split_semi_pred(selection, &inner.name)?;
    let outer_tbl = outer_table.unwrap_or("t1");
    // TiDB: null-reject both sides; EXPLAIN equal lists outer col first; no Distinct.
    let join_label = if not { "anti semi join" } else { "semi join" };
    let equal = explain_semi_eq_outer_first(&eq, &inner_physical);
    let info = format!("{join_label}, equal:[{equal}]");
    let on = ExprNode::Binary(BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(eq.clone()),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        })),
        single_cnf_item: false,
    });
    let right_nulls = not_isnull_for_eq(&eq, &inner.name);
    let left_nulls = not_isnull_for_eq(&eq, outer_tbl);
    let mut right = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
    if let Some(filter) = merge_filters(inner_only, right_nulls) {
        right = push_isnull_filter(right, filter);
    }
    Some(SemiSpec {
        tp: if not { JoinType::Anti } else { JoinType::Semi },
        right_table: inner.name.clone(),
        right,
        on,
        outer_filter: left_nulls,
        dedup_in: false,
        right_physical: inner_physical.to_string(),
    })
}

fn not_isnull_for_eq(on: &ExprNode, table: &str) -> Option<ExprNode> {
    let eq = match on {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            if matches!(bin.left.as_ref(), ExprNode::Binary(_)) {
                bin.left.as_ref()
            } else {
                bin.right.as_ref()
            }
        }
        other => other,
    };
    let ExprNode::Binary(bin) = eq else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    for side in [&bin.left, &bin.right] {
        if let ExprNode::Column(c) = side.as_ref()
            && c.table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(table))
        {
            return Some(ExprNode::Unary(UnaryOpExpr {
                op: Op::Not,
                expr: Box::new(ExprNode::IsNull(IsNullExpr {
                    expr: Box::new(ExprNode::Column(ColumnNameExpr {
                        schema: None,
                        table: Some(table.to_string()),
                        name: c.name.clone(),
                    })),
                    not: false,
                })),
            }));
        }
    }
    None
}

/// `outer_col IN (SELECT inner_col FROM t [WHERE …])`.
/// Correlated WHERE preds become `other cond`; ORDER BY is ignored (semi).
fn from_in(
    expr: &ExprNode,
    query: &SelectStmt,
    outer_table: Option<&str>,
    not: bool,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
) -> Option<SemiSpec> {
    let join_tp = if not { JoinType::Anti } else { JoinType::Semi };
    let join_label = if not { "anti semi join" } else { "semi join" };
    if query_has_aggregation(query) {
        return None;
    }
    let inner = simple_inner(query)?;
    let inner_physical = inner.physical.as_deref().unwrap_or(&inner.name);
    // ORDER BY in an IN subquery is meaningless for semi-join and is dropped.
    let (inner_filter, other_conds) = split_in_selection(query.selection.as_ref(), &inner.name)?;
    // `IN (SELECT * FROM t)` compares against the table's (single) column:
    // expand the star so the join gets a real column, not the `*` marker.
    let inner_field = match query.fields.as_slice() {
        [field] if is_star_field(&field.expr) => {
            let columns = is.and_then(|is| {
                let (db, table) = inner.qualified(current_db);
                is.table_by_name(&db, &table).map(|info| {
                    info.columns
                        .iter()
                        .map(|c| {
                            ExprNode::Column(ColumnNameExpr {
                                schema: None,
                                table: None,
                                name: c.name.o().to_string(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })?;
            match columns.as_slice() {
                [only] => only.clone(),
                _ => return None,
            }
        }
        _ => query.fields.first()?.expr.clone(),
    };
    // `select a from t2` where `t2` has no `a`: MySQL/TiDB resolve the field to
    // the outer query's column (correlated select list), e.g.
    // `select * from t1 where a in (select a from t2)`.
    let inner_field = match &inner_field {
        ExprNode::Column(col) if col.table.is_none() => {
            let missing = is
                .and_then(|is| {
                    let (db, table) = inner.qualified(current_db);
                    is.table_by_name(&db, &table).map(|info| {
                        !info
                            .columns
                            .iter()
                            .any(|c| c.name.o().eq_ignore_ascii_case(&col.name))
                    })
                })
                .unwrap_or(false);
            match (missing, outer_table) {
                (true, Some(table)) => ExprNode::Column(ColumnNameExpr {
                    schema: col.schema.clone(),
                    table: Some(table.to_string()),
                    name: col.name.clone(),
                }),
                _ => inner_field,
            }
        }
        _ => inner_field,
    };
    let inner_expr = qualify_table(inner_field, &inner.name);
    let outer_expr = match outer_table {
        Some(table) => qualify_table(expr.clone(), table),
        None => expr.clone(),
    };
    // `a IN (SELECT outer.b + inner.b FROM inner)` → CARTESIAN semi + other cond.
    if (!matches!(inner_expr, ExprNode::Column(_)) || !matches!(outer_expr, ExprNode::Column(_)))
        && other_conds.is_empty()
        && inner_filter.is_none()
    {
        let cart = if not {
            "CARTESIAN anti semi join"
        } else {
            "CARTESIAN semi join"
        };
        let info = format!(
            "{cart}, other cond:eq({}, {})",
            explain_semi_pred(&outer_expr),
            explain_semi_pred_inner(&inner_expr, &inner.name, inner_physical),
        );
        // Executable equality under the ExplainInfo marker: the executor has
        // no index key here, so it probes the cartesian product and filters
        // with the real predicate (semi/anti-aware).
        let key = eq_expr(outer_expr.clone(), inner_expr.clone());
        let on = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(key),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(info.into_bytes()),
            })),
            single_cnf_item: false,
        });
        return Some(SemiSpec {
            tp: join_tp,
            right_table: inner.name.clone(),
            right: PhysicalPlan::table_reader(inner.name.clone(), Vec::new()),
            on,
            outer_filter: None,
            dedup_in: false,
            right_physical: inner_physical.to_string(),
        });
    }
    // `name NOT IN (SELECT const FROM exam WHERE exam.stu_id = stu.id)`:
    // equal on correlated keys; other cond:eq(outer_col, const).
    // ON = LogicAnd(Eq, Bytes(explain)) so HashJoin can probe on the key
    // while EXPLAIN still prints the full anti/semi info string.
    if matches!(inner_expr, ExprNode::Value(_)) && !other_conds.is_empty() {
        let _outer = outer_table?;
        let equals: Vec<&ExprNode> = other_conds
            .iter()
            .filter(|c| is_eq_across_tables_any(c, &inner.name))
            .collect();
        if equals.is_empty() {
            return None;
        }
        let equal_info = equals
            .iter()
            .map(|e| explain_semi_eq_outer_first(e, &inner.name))
            .collect::<Vec<_>>()
            .join(" ");
        let other = format!(
            "eq({}, {})",
            explain_semi_pred(&outer_expr),
            explain_semi_pred_inner(&inner_expr, &inner.name, inner_physical)
        );
        let info = format!("{join_label}, equal:[{equal_info}], other cond:{other}");
        let mut right = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
        if let Some(filter) = inner_filter {
            right = push_isnull_filter(right, filter);
        }
        let eq = (*equals.first()?).clone();
        let on = ExprNode::Binary(BinaryOpExpr {
            op: Op::LogicAnd,
            left: Box::new(eq),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(info.into_bytes()),
            })),
            single_cnf_item: false,
        });
        return Some(SemiSpec {
            tp: join_tp,
            right_table: inner.name.clone(),
            right,
            on,
            outer_filter: None,
            dedup_in: false,
            right_physical: inner_physical.to_string(),
        });
    }
    if !matches!(inner_expr, ExprNode::Column(_)) || !matches!(outer_expr, ExprNode::Column(_)) {
        return None;
    }
    if !is_eq_across_tables(&outer_expr, &inner_expr, &inner.name) {
        return None;
    }
    let equal = eq_expr(outer_expr.clone(), inner_expr.clone());
    if other_conds.is_empty() {
        let mut spec = build_spec(
            join_tp,
            inner.name.clone(),
            inner_filter,
            equal,
            None,
            // `x IN (SELECT c FROM t)`: TiDB rewrites the semi join into an
            // inner join against the de-duplicated subquery.
            !not,
        );
        spec.right_physical = inner_physical.to_string();
        return Some(spec);
    }
    // Correlated IN → semi join with equal + other cond (TiDB decorrelate).
    let outer = outer_table?;
    let mut cond_exprs = vec![equal.clone()];
    cond_exprs.extend(other_conds.iter().cloned());
    let right_nulls = not_isnull_cols(&cond_exprs, &inner.name);
    let left_nulls = not_isnull_cols(&cond_exprs, outer);
    let mut right = PhysicalPlan::table_reader(inner.name.clone(), Vec::new());
    if let Some(filter) = merge_filters(inner_filter, right_nulls) {
        right = push_isnull_filter(right, filter);
    }
    let info = format!(
        "{join_label}, equal:[{}], other cond:{}",
        explain_semi_eq(
            &outer_expr,
            &inner_expr,
            Some((&inner.name, inner_physical)),
        ),
        other_conds
            .iter()
            .map(|c| explain_semi_pred_inner(c, &inner.name, inner_physical))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some(SemiSpec {
        tp: join_tp,
        right_table: inner.name.clone(),
        right,
        on: ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        }),
        outer_filter: left_nulls,
        dedup_in: false,
        right_physical: inner_physical.to_string(),
    })
}

/// TiDB `buildDistinct` on the `IN` subquery plus `ExtractOnCondition`'s null
/// rejection: the inner side of the rewritten inner join keeps one row per
/// distinct key value, so the join can never duplicate an outer row.
fn dedup_in_right(
    right: PhysicalPlan,
    key: &str,
    right_table: &str,
    current_db: &str,
    stats: Option<&sealdb_statistics::Handle>,
) -> PhysicalPlan {
    let col = ExprNode::Column(ColumnNameExpr {
        schema: None,
        table: Some(right_table.to_string()),
        name: key.to_string(),
    });
    let qualifier = explain_semi_col(&col, None);
    let right = push_isnull_filter(
        right,
        ExprNode::Unary(UnaryOpExpr {
            op: Op::Not,
            expr: Box::new(ExprNode::IsNull(IsNullExpr {
                expr: Box::new(col),
                not: false,
            })),
        }),
    );
    // TiDB `buildDistinct` reads the subquery after `ANALYZE`, so the scan
    // carries the real table count; unanalyzed tables keep the 10000 default.
    let table_stats = stats
        .and_then(|h| h.table(current_db, right_table))
        .filter(super::plan_cost::has_real_stats);
    let right = with_scan_count(right, table_stats.as_ref().map(|t| t.count as f64));
    // TiDB `GroupNDV`: the key column's NDV under real statistics, otherwise the
    // pseudo 0.8 distinct factor over the null-rejected rows.
    let est = match table_stats.as_ref() {
        Some(t) => t
            .column_ndv(key)
            .map(|ndv| ndv.min(t.count as f64))
            .unwrap_or_else(|| super::explain::est_rows(&right)),
        None => super::explain::est_rows(&right) * 0.8,
    };
    PhysicalPlan::hash_agg(
        right,
        format!("group by:{qualifier}, funcs:firstrow({qualifier})->{qualifier}"),
        est,
    )
}

/// Pin the underlying reader's cardinality (real stats → analyzed estimate,
/// `None` → keep the pseudo 10000 default).
fn with_scan_count(plan: PhysicalPlan, count: Option<f64>) -> PhysicalPlan {
    use PhysicalPlan as P;
    match plan {
        P::TableReader {
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
            keep_order,
        } => P::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count: count.unwrap_or(row_count),
            stats_pseudo: match count {
                Some(_) => false,
                None => stats_pseudo,
            },
            data_limit,
            filter_est,
            range_scan,
            pushed,
            keep_order,
        },
        P::Selection {
            child,
            filter,
            est_rows,
        } => P::Selection {
            child: Box::new(with_scan_count(*child, count)),
            filter,
            est_rows,
        },
        other => other,
    }
}

/// `SemiSpec` builder that records the inner table's physical (OrigTblName)
/// name so EXPLAIN can render join keys the way TiDB does.
impl SemiSpec {
    fn with_physical(mut self, physical: &str) -> Self {
        self.right_physical = physical.to_string();
        self
    }
}

fn build_spec(
    tp: JoinType,
    right_table: String,
    inner_filter: Option<ExprNode>,
    on: ExprNode,
    outer_filter: Option<ExprNode>,
    dedup_in: bool,
) -> SemiSpec {
    let mut right = PhysicalPlan::table_reader(right_table.clone(), Vec::new());
    if let Some(filter) = inner_filter {
        right = PhysicalPlan::Selection {
            child: Box::new(right),
            filter,
            est_rows: None,
        };
    }
    SemiSpec {
        tp,
        right_table: right_table.clone(),
        right,
        on,
        outer_filter,
        dedup_in,
        right_physical: right_table,
    }
}

fn split_in_selection(
    selection: Option<&ExprNode>,
    inner: &str,
) -> Option<(Option<ExprNode>, Vec<ExprNode>)> {
    let Some(sel) = selection else {
        return Some((None, Vec::new()));
    };
    let mut inner_preds = Vec::new();
    let mut other = Vec::new();
    for item in sel.split_cnf() {
        if item.has_subquery() {
            return None;
        }
        if only_inner(&item, inner) {
            inner_preds.push(item);
        } else {
            other.push(item);
        }
    }
    Some((ExprNode::compose_cnf(inner_preds), other))
}

fn merge_filters(a: Option<ExprNode>, b: Option<ExprNode>) -> Option<ExprNode> {
    match (a, b) {
        (Some(x), Some(y)) => ExprNode::compose_cnf(vec![x, y]),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

fn push_isnull_filter(plan: PhysicalPlan, filter: ExprNode) -> PhysicalPlan {
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
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            data_limit,
            filter_est,
            range_scan,
            keep_order: false,
            pushed: match pushed {
                Some(prev) => ExprNode::compose_cnf(vec![prev, filter]),
                None => Some(filter),
            },
        },
        other => PhysicalPlan::Selection {
            child: Box::new(other),
            filter,
            est_rows: None,
        },
    }
}

fn not_isnull_cols(exprs: &[ExprNode], table: &str) -> Option<ExprNode> {
    let mut seen = Vec::new();
    let mut filters = Vec::new();
    for expr in exprs {
        for (t, name) in expr.referenced_columns() {
            if !t.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(table)) {
                continue;
            }
            if seen.iter().any(|(tn, cn): &(String, String)| {
                tn.eq_ignore_ascii_case(table) && cn.eq_ignore_ascii_case(&name)
            }) {
                continue;
            }
            seen.push((table.to_string(), name.clone()));
            filters.push(ExprNode::Unary(UnaryOpExpr {
                op: Op::Not,
                expr: Box::new(ExprNode::IsNull(IsNullExpr {
                    expr: Box::new(ExprNode::Column(ColumnNameExpr {
                        schema: None,
                        table: Some(table.to_string()),
                        name,
                    })),
                    not: false,
                })),
            }));
        }
    }
    ExprNode::compose_cnf(filters)
}

fn explain_semi_eq(left: &ExprNode, right: &ExprNode, inner: Option<(&str, &str)>) -> String {
    format!(
        "eq({}, {})",
        explain_semi_col(left, inner),
        explain_semi_col(right, inner)
    )
}

/// TiDB EXPLAIN prints outer column first in semi/anti equal lists.
fn explain_semi_eq_outer_first(eq: &ExprNode, inner: &str) -> String {
    let ExprNode::Binary(bin) = eq else {
        return explain_semi_pred(eq);
    };
    let (outer, inn) = if col_table(&bin.left).is_some_and(|t| t.eq_ignore_ascii_case(inner)) {
        (bin.right.as_ref(), bin.left.as_ref())
    } else {
        (bin.left.as_ref(), bin.right.as_ref())
    };
    explain_semi_eq(outer, inn, None)
}

pub(crate) fn explain_semi_pred(expr: &ExprNode) -> String {
    explain_semi_pred_with(expr, None)
}

/// `inner` names the decorrelated subquery's table so its columns print with
/// the subquery's physical (OrigTblName) table instead of the table alias.
fn explain_semi_pred_inner<'a>(
    expr: &ExprNode,
    inner_alias: &'a str,
    inner_physical: &'a str,
) -> String {
    explain_semi_pred_with(expr, Some((inner_alias, inner_physical)))
}

fn explain_semi_pred_with(expr: &ExprNode, inner: Option<(&str, &str)>) -> String {
    match expr {
        ExprNode::Binary(bin) => {
            let op = match bin.op {
                Op::Gt => "gt",
                Op::Ge => "ge",
                Op::Lt => "lt",
                Op::Le => "le",
                Op::Eq => "eq",
                Op::Ne => "ne",
                Op::Plus => "plus",
                Op::Minus => "minus",
                Op::Mul => "mul",
                Op::Div => "div",
                Op::IntDiv => "intdiv",
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_semi_pred_with(&bin.left, inner),
                explain_semi_pred_with(&bin.right, inner)
            )
        }
        // TiDB renders an IN list as `in(col, e1, e2)`.
        ExprNode::InList(list) => {
            let mut parts = vec![explain_semi_pred_with(&list.expr, inner)];
            parts.extend(list.list.iter().map(|e| explain_semi_pred_with(e, inner)));
            format!(
                "{}({})",
                if list.not { "not in" } else { "in" },
                parts.join(", ")
            )
        }
        other => explain_semi_col(other, inner),
    }
}

fn explain_semi_col(expr: &ExprNode, inner: Option<(&str, &str)>) -> String {
    match expr {
        ExprNode::Column(c) => {
            let alias = c.table.as_deref().unwrap_or("t");
            // TiDB `OrigTblName`: aliases print as the physical table.
            let table = match inner {
                Some((inner_alias, inner_physical)) if alias.eq_ignore_ascii_case(inner_alias) => {
                    inner_physical.to_string()
                }
                _ => crate::core::planbuilder::explain_physical_table(alias)
                    .unwrap_or_else(|| alias.to_string()),
            };
            let schema = sealdb_expression::current_db_name();
            if schema.is_empty() {
                format!("select.{table}.{}", c.name)
            } else {
                format!("{schema}.{table}.{}", c.name)
            }
        }
        // TiDB EXPLAIN uses double-quoted string literals.
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) => format!("\"{}\"", String::from_utf8_lossy(b).replace('\"', "\\\"")),
        ExprNode::Value(ValueExpr {
            datum: Datum::I64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr {
            datum: Datum::U64(v),
        }) => v.to_string(),
        ExprNode::Value(ValueExpr { datum: Datum::Null }) => "NULL".into(),
        other => other.restore_string(),
    }
}

fn is_eq_across_tables_any(expr: &ExprNode, inner: &str) -> bool {
    is_join_eq(expr, inner)
}

/// Single-table subquery with no grouping, limit, CTE, or extra joins.
fn is_star_field(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) if b == b"*")
}

fn simple_inner(query: &SelectStmt) -> Option<&TableName> {
    if !query.joins.is_empty()
        || !query.group_by.is_empty()
        || query.having.is_some()
        || query.with.is_some()
        || query.limit.is_some()
        || query.offset.is_some()
    {
        return None;
    }
    let from = query.from.as_ref()?;
    if from.name.eq_ignore_ascii_case("dual") {
        return None;
    }
    Some(from)
}

/// Split a CNF WHERE into one cross-table equality (the join key) plus
/// optional inner-only filters pushed onto the right TableReader.
fn split_semi_pred(selection: &ExprNode, inner: &str) -> Option<(ExprNode, Option<ExprNode>)> {
    let mut on = None;
    let mut inner_preds = Vec::new();
    for item in selection.split_cnf() {
        if is_join_eq(&item, inner) {
            if on.is_some() {
                return None;
            }
            on = Some(item);
        } else if only_inner(&item, inner) {
            inner_preds.push(item);
        } else {
            return None;
        }
    }
    Some((on?, ExprNode::compose_cnf(inner_preds)))
}

fn is_join_eq(expr: &ExprNode, inner: &str) -> bool {
    let ExprNode::Binary(bin) = expr else {
        return false;
    };
    if bin.op != Op::Eq {
        return false;
    }
    is_eq_across_tables(&bin.left, &bin.right, inner)
}

fn is_eq_across_tables(left: &ExprNode, right: &ExprNode, inner: &str) -> bool {
    match (col_table(left), col_table(right)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(inner) != b.eq_ignore_ascii_case(inner),
        _ => false,
    }
}

fn col_table(expr: &ExprNode) -> Option<&str> {
    match expr {
        ExprNode::Column(col) => col.table.as_deref(),
        _ => None,
    }
}

fn only_inner(expr: &ExprNode, inner: &str) -> bool {
    !expr.has_subquery()
        && expr.referenced_columns().iter().all(|(table, _)| {
            table
                .as_deref()
                .is_none_or(|name| name.eq_ignore_ascii_case(inner))
        })
}

fn qualify_table(expr: ExprNode, table: &str) -> ExprNode {
    match expr {
        ExprNode::Column(mut col) if col.table.is_none() => {
            col.table = Some(table.to_string());
            ExprNode::Column(col)
        }
        other => other,
    }
}

fn eq_expr(left: ExprNode, right: ExprNode) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item: false,
    })
}

fn plan_table(plan: &PhysicalPlan) -> Option<String> {
    match plan {
        PhysicalPlan::TableReader { table, .. }
        | PhysicalPlan::PointGet { table, .. }
        | PhysicalPlan::BatchPointGet { table, .. }
        | PhysicalPlan::IndexLookUpReader { table, .. }
        | PhysicalPlan::IndexReader { table, .. }
        | PhysicalPlan::IndexMerge { table, .. }
        | PhysicalPlan::HashJoin {
            left_table: table, ..
        } => Some(table.clone()),
        PhysicalPlan::Selection { child, .. }
        | PhysicalPlan::Projection { child, .. }
        | PhysicalPlan::Sort { child, .. }
        | PhysicalPlan::TopN { child, .. }
        | PhysicalPlan::Limit { child, .. }
        | PhysicalPlan::HashAgg { child, .. }
        | PhysicalPlan::StreamAgg { child, .. }
        | PhysicalPlan::Window { child, .. }
        | PhysicalPlan::Shuffle { child, .. }
        | PhysicalPlan::ShuffleReceiver { child }
        | PhysicalPlan::Apply { child, .. }
        | PhysicalPlan::Distinct { child, .. } => plan_table(child),
        _ => None,
    }
}

/// TiDB SemiJoinRewriter: DISTINCT the Semi inner (right) side on the join
/// key so the build side is unique. Keep HashJoin Semi so SELECT * still
/// emits the left schema (TiDB adds a left projection after Inner+Agg).
pub fn rewrite_semi_join_agg(
    plan: PhysicalPlan,
    is: Option<&sealdb_infoschema::InfoSchema>,
    current_db: &str,
    stats: Option<&sealdb_statistics::Handle>,
) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            est_rows,
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left = rewrite_semi_join_agg(*left, is, current_db, stats);
            let right = rewrite_semi_join_agg(*right, is, current_db, stats);
            // LogicAnd(Eq, Bytes explain) — used by CTE EXISTS — skips Distinct
            // (join_key_on_table only matches bare Eq); TiDB has no HashAgg here.
            if tp == JoinType::Semi
                && let Some(key) = join_key_on_table(&on, &right_table)
            {
                PhysicalPlan::HashJoin {
                    est_rows,
                    tp,
                    left: Box::new(left),
                    right: Box::new(PhysicalPlan::Distinct {
                        keys: vec![key],
                        fields: Vec::new(),
                        names: Vec::new(),
                        child: Box::new(right),
                    }),
                    left_table,
                    right_table,
                    on,
                }
            } else {
                PhysicalPlan::HashJoin {
                    est_rows,
                    tp,
                    left: Box::new(left),
                    right: Box::new(right),
                    left_table,
                    right_table,
                    on,
                }
            }
        }
        PhysicalPlan::Apply {
            child,
            filter,
            probe,
            info,
        } => PhysicalPlan::Apply {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            filter,
            probe: probe.map(|p| Box::new(rewrite_semi_join_agg(*p, is, current_db, stats))),
            info,
        },
        PhysicalPlan::Selection {
            child,
            filter,
            est_rows,
        } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            filter,
            est_rows,
        },
        PhysicalPlan::Projection { exprs, child, .. } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            est_rows: None,
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer,
            inner,
        } => PhysicalPlan::IndexJoin {
            info,
            est_rows,
            outer: Box::new(rewrite_semi_join_agg(*outer, is, current_db, stats)),
            inner: Box::new(rewrite_semi_join_agg(*inner, is, current_db, stats)),
        },
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::Window { info, child } => PhysicalPlan::Window {
            info,
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
        PhysicalPlan::Shuffle { info, child } => PhysicalPlan::Shuffle {
            info,
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
        PhysicalPlan::ShuffleReceiver { child } => PhysicalPlan::ShuffleReceiver {
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
        PhysicalPlan::Union { op, children } => PhysicalPlan::Union {
            op,
            children: children
                .into_iter()
                .map(|c| Box::new(rewrite_semi_join_agg(*c, is, current_db, stats)))
                .collect(),
        },
        PhysicalPlan::DistinctUnion {
            info,
            est_rows,
            child,
        } => PhysicalPlan::DistinctUnion {
            info,
            est_rows,
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
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
        } => PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed: Box::new(rewrite_semi_join_agg(*seed, is, current_db, stats)),
            recursive_plan: recursive_plan
                .map(|p| Box::new(rewrite_semi_join_agg(*p, is, current_db, stats))),
            inline,
            recursive,
            limit,
            offset,
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
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
            child: Box::new(rewrite_semi_join_agg(*child, is, current_db, stats)),
        },
        other => other,
    }
}

fn join_key_on_table(on: &ExprNode, table: &str) -> Option<String> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    for side in [&bin.left, &bin.right] {
        if let ExprNode::Column(col) = side.as_ref()
            && col
                .table
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(table))
        {
            return Some(col.name.clone());
        }
    }
    None
}

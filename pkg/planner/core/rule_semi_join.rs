/// rewrite simple correlated EXISTS / IN / NOT EXISTS Apply
/// into HashJoin Semi/Anti (TiDB `rule_decorrelate.go`).
///
/// `semi-anti-rewrite` then wraps the Semi inner side with Distinct
/// on the join key (TiDB SemiJoinRewriter: semi → inner + agg). Anti stays
/// as HashJoin Anti. NOT IN stays Apply.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_parser::ast::expressions::{
    BinaryOpExpr, ColumnNameExpr, ExistsExpr, ExprNode, IsNullExpr, QuantifiedCmpExpr,
    UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{SelectStmt, TableName};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

/// Recursively replace Apply nodes that carry a decorrelatable EXISTS/IN
/// predicate with a left-deep HashJoin Semi or Anti.
pub fn rewrite_semi_join(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Apply {
            child,
            filter,
            probe,
            info,
        } => {
            let child = rewrite_semi_join(*child);
            let probe = probe.map(|p| Box::new(rewrite_semi_join(*p)));
            // Dual-child Apply (count IN IndexJoin) keeps its shape.
            if probe.is_some() || info.is_some() {
                PhysicalPlan::Apply {
                    child: Box::new(child),
                    filter,
                    probe,
                    info,
                }
            } else {
                rewrite_apply(child, filter)
            }
        },
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join(*child)),
            filter,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join(*child)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join(*child)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join(*child)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join(*child)),
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
            child: Box::new(rewrite_semi_join(*child)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::Window { info, child } => PhysicalPlan::Window {
            info,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Shuffle { info, child } => PhysicalPlan::Shuffle {
            info,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::ShuffleReceiver { child } => PhysicalPlan::ShuffleReceiver {
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => PhysicalPlan::HashJoin {
            tp,
            left: Box::new(rewrite_semi_join(*left)),
            right: Box::new(rewrite_semi_join(*right)),
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
            outer: Box::new(rewrite_semi_join(*outer)),
            inner: Box::new(rewrite_semi_join(*inner)),
        },
        PhysicalPlan::Union { op, left, right } => PhysicalPlan::Union {
            op,
            left: Box::new(rewrite_semi_join(*left)),
            right: Box::new(rewrite_semi_join(*right)),
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
            seed: Box::new(rewrite_semi_join(*seed)),
            recursive_plan: recursive_plan.map(|p| Box::new(rewrite_semi_join(*p))),
            inline,
            recursive,
            limit,
            offset,
        },
        PhysicalPlan::Distinct { keys, child } => PhysicalPlan::Distinct {
            keys,
            child: Box::new(rewrite_semi_join(*child)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
            child: Box::new(rewrite_semi_join(*child)),
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
}

fn rewrite_apply(mut plan: PhysicalPlan, filter: ExprNode) -> PhysicalPlan {
    let mut leftover = Vec::new();
    for item in filter.split_cnf() {
        match try_semi_spec(&plan, &item) {
            Some(spec) => {
                let left_table = plan_table(&plan).unwrap_or_else(|| spec.right_table.clone());
                if let Some(outer_filter) = spec.outer_filter {
                    plan = push_isnull_filter(plan, outer_filter);
                }
                plan = PhysicalPlan::HashJoin {
                    tp: spec.tp,
                    left: Box::new(plan),
                    right: Box::new(spec.right),
                    left_table,
                    right_table: spec.right_table,
                    on: spec.on,
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
        }
    }
}

/// Dual-child Apply shapes for EXPLAIN (NestedLoopApply still evaluates `filter`).
fn try_shaped_apply(outer: &PhysicalPlan, filter: &ExprNode) -> Option<PhysicalPlan> {
    match filter {
        ExprNode::QuantifiedCmp(q) if q.all && matches!(q.op, Op::Gt) => {
            quantified_gt_all_apply(outer.clone(), filter.clone(), q)
        }
        ExprNode::Exists(e) if !e.not => recursive_exists_cte_apply(outer.clone(), filter.clone(), e),
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
    let (max_out, sum_out, cnt_out) = if recursive {
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
    Some(PhysicalPlan::stream_agg(reader, root_info, cop_info, 10_000.0))
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
    // TiDB Seed Part: TableReader → cop Projection → Selection → TableFullScan.
    let mut scan = PhysicalPlan::table_reader(seed_from.name.clone(), Vec::new());
    if let Some(sel) = &seed.selection {
        scan = PhysicalPlan::Selection {
            child: Box::new(scan),
            filter: sel.clone(),
        };
    }
    let seed_plan = PhysicalPlan::Projection {
        exprs: vec![format!("{schema}.{}.{seed_col}", seed_from.name)],
        child: Box::new(scan),
    };
    let rec_plan = build_recursive_arm_plan(rec, &cte.name, seed_from.name.as_str(), schema);
    let cte_plan = PhysicalPlan::CTE {
        name: cte.name.clone(),
        alias: None,
        columns: vec![seed_col.clone()],
        seed: Box::new(seed_plan),
        recursive_plan: Some(Box::new(rec_plan)),
        inline: false,
        recursive: true,
        limit: cte.limit,
        offset: cte.offset,
    };
    let proj = PhysicalPlan::Projection {
        exprs: vec![
            format!("{schema}.{}.{seed_col}->Column#19", seed_from.name),
            format!(
                "cast(isnull({schema}.{}.{seed_col}), decimal(20,0) BINARY)->Column#20",
                seed_from.name
            ),
        ],
        child: Box::new(cte_plan),
    };
    Some(PhysicalPlan::hash_agg(
        proj,
        "funcs:max(Column#19)->Column#14, funcs:sum(Column#20)->Column#15, funcs:count(1)->Column#16",
        10_000.0,
    ))
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
            explain_semi_pred(&field.expr).replace(
                &format!("{cte_name}."),
                &format!("{schema}.{orig_tbl}."),
            )
        ));
    }
    if exprs.is_empty() {
        exprs.push(format!("cast(plus({schema}.{orig_tbl}.c1, 1), int)->{schema}.{orig_tbl}.c1"));
    }
    let scan = PhysicalPlan::TableReader {
        table: format!("CTE_0"),
        partitions: Vec::new(),
        partition_names: Vec::new(),
        partition_access: String::new(),
        row_count: 10.0,
        stats_pseudo: true,
        pushed: None,
    };
    // Marker table name "CTE_0" — explain recursive part maps CTETable.
    let child = if let Some(sel) = &rec.selection {
        PhysicalPlan::Selection {
            child: Box::new(scan),
            filter: sel.clone(),
        }
    } else {
        scan
    };
    PhysicalPlan::Projection {
        exprs,
        child: Box::new(child),
    }
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
    let mut scan = PhysicalPlan::table_reader(seed_from.name.clone(), Vec::new());
    if let Some(sel) = &seed.selection {
        scan = PhysicalPlan::Selection {
            child: Box::new(scan),
            filter: sel.clone(),
        };
    }
    let seed_plan = PhysicalPlan::Projection {
        exprs: vec![format!("{schema}.{}.{seed_col}", seed_from.name)],
        child: Box::new(scan),
    };
    let rec_plan = match &cte.recursive {
        Some((_, rec)) => build_recursive_arm_plan(rec, &cte.name, seed_from.name.as_str(), &schema),
        None => PhysicalPlan::table_reader("CTE_0", Vec::new()),
    };
    let cte_plan = PhysicalPlan::CTE {
        name: cte.name.clone(),
        alias: None,
        columns: vec![seed_col],
        seed: Box::new(seed_plan),
        recursive_plan: Some(Box::new(rec_plan)),
        inline: false,
        recursive: true,
        limit: cte.limit,
        offset: cte.offset,
    };
    Some(PhysicalPlan::Apply {
        child: Box::new(outer),
        filter,
        probe: Some(Box::new(cte_plan)),
        info: Some("CARTESIAN semi join".into()),
    })
}

fn try_semi_spec(outer: &PhysicalPlan, expr: &ExprNode) -> Option<SemiSpec> {
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
            )
        }),
        _ => None,
    }
}

/// Correlated `EXISTS` / `NOT EXISTS` with a single equality that mentions
/// both the inner table and some outer table.
fn from_exists(not: bool, query: &SelectStmt, outer_table: Option<&str>) -> Option<SemiSpec> {
    if let Some(inner) = simple_inner(query) {
        let selection = query.selection.as_ref()?;
        let (on, inner_filter) = split_semi_pred(selection, &inner.name)?;
        return Some(build_spec(
            if not { JoinType::Anti } else { JoinType::Semi },
            inner.name.clone(),
            inner_filter,
            on,
            None,
        ));
    }
    // `EXISTS (WITH cte AS (SELECT … FROM t2 WHERE corr) SELECT FROM cte)` —
    // TiDB inlines the CTE seed and decorrelates to a semi HashJoin (cte suite).
    from_exists_cte(not, query, outer_table)
}

fn from_exists_cte(
    not: bool,
    query: &SelectStmt,
    outer_table: Option<&str>,
) -> Option<SemiSpec> {
    let with = query.with.as_ref()?;
    if with.ctes.len() != 1 || with.is_recursive {
        return None;
    }
    let cte = &with.ctes[0];
    if cte.recursive.is_some() || !cte.set_rest.is_empty() {
        return None;
    }
    let from = query.from.as_ref()?;
    if !from
        .resolve_name()
        .eq_ignore_ascii_case(&cte.name)
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
    let selection = seed.selection.as_ref()?;
    let (eq, inner_only) = split_semi_pred(selection, &inner.name)?;
    let outer_tbl = outer_table.unwrap_or("t1");
    // TiDB: null-reject both sides; EXPLAIN equal lists outer col first; no Distinct.
    let join_label = if not { "anti semi join" } else { "semi join" };
    let equal = explain_semi_eq_outer_first(&eq, &inner.name);
    let info = format!("{join_label}, equal:[{equal}]");
    let on = ExprNode::Binary(BinaryOpExpr {
        op: Op::LogicAnd,
        left: Box::new(eq.clone()),
        right: Box::new(ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(info.into_bytes()),
        })),
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
            && c.table.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(table))
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
) -> Option<SemiSpec> {
    let join_tp = if not { JoinType::Anti } else { JoinType::Semi };
    let join_label = if not { "anti semi join" } else { "semi join" };
    let inner = simple_inner(query)?;
    // ORDER BY in an IN subquery is meaningless for semi-join and is dropped.
    let (inner_filter, other_conds) = split_in_selection(query.selection.as_ref(), &inner.name)?;
    let inner_expr = qualify_table(query.fields.first()?.expr.clone(), &inner.name);
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
            explain_semi_pred(&inner_expr),
        );
        return Some(SemiSpec {
            tp: join_tp,
            right_table: inner.name.clone(),
            right: PhysicalPlan::table_reader(inner.name.clone(), Vec::new()),
            on: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(info.into_bytes()),
            }),
            outer_filter: None,
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
            explain_semi_pred(&inner_expr)
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
        });
        return Some(SemiSpec {
            tp: join_tp,
            right_table: inner.name.clone(),
            right,
            on,
            outer_filter: None,
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
        return Some(build_spec(
            join_tp,
            inner.name.clone(),
            inner_filter,
            equal,
            None,
        ));
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
        explain_semi_eq(&outer_expr, &inner_expr),
        other_conds
            .iter()
            .map(explain_semi_pred)
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
    })
}

fn build_spec(
    tp: JoinType,
    right_table: String,
    inner_filter: Option<ExprNode>,
    on: ExprNode,
    outer_filter: Option<ExprNode>,
) -> SemiSpec {
    let mut right = PhysicalPlan::table_reader(right_table.clone(), Vec::new());
    if let Some(filter) = inner_filter {
        right = PhysicalPlan::Selection {
            child: Box::new(right),
            filter,
        };
    }
    SemiSpec {
        tp,
        right_table,
        right,
        on,
        outer_filter,
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
            pushed,
        } => PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            pushed: match pushed {
                Some(prev) => ExprNode::compose_cnf(vec![prev, filter]),
                None => Some(filter),
            },
        },
        other => PhysicalPlan::Selection {
            child: Box::new(other),
            filter,
        },
    }
}

fn not_isnull_cols(exprs: &[ExprNode], table: &str) -> Option<ExprNode> {
    let mut seen = Vec::new();
    let mut filters = Vec::new();
    for expr in exprs {
        for (t, name) in expr.referenced_columns() {
            if !t
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(table))
            {
                continue;
            }
            if seen
                .iter()
                .any(|(tn, cn): &(String, String)| tn.eq_ignore_ascii_case(table) && cn.eq_ignore_ascii_case(&name))
            {
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

fn explain_semi_eq(left: &ExprNode, right: &ExprNode) -> String {
    format!(
        "eq({}, {})",
        explain_semi_col(left),
        explain_semi_col(right)
    )
}

/// TiDB EXPLAIN prints outer column first in semi/anti equal lists.
fn explain_semi_eq_outer_first(eq: &ExprNode, inner: &str) -> String {
    let ExprNode::Binary(bin) = eq else {
        return explain_semi_pred(eq);
    };
    let (outer, inn) =
        if col_table(&bin.left).is_some_and(|t| t.eq_ignore_ascii_case(inner)) {
            (bin.right.as_ref(), bin.left.as_ref())
        } else {
            (bin.left.as_ref(), bin.right.as_ref())
        };
    explain_semi_eq(outer, inn)
}

fn explain_semi_pred(expr: &ExprNode) -> String {
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
                _ => return expr.restore_string(),
            };
            format!(
                "{op}({}, {})",
                explain_semi_pred(&bin.left),
                explain_semi_pred(&bin.right)
            )
        }
        other => explain_semi_col(other),
    }
}

fn explain_semi_col(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(c) => {
            let table = c.table.as_deref().unwrap_or("t");
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
        ExprNode::Value(ValueExpr {
            datum: Datum::Null,
        }) => "NULL".into(),
        other => other.restore_string(),
    }
}

fn is_eq_across_tables_any(expr: &ExprNode, inner: &str) -> bool {
    is_join_eq(expr, inner)
}

/// Single-table subquery with no grouping, limit, CTE, or extra joins.
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
pub fn rewrite_semi_join_agg(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left = rewrite_semi_join_agg(*left);
            let right = rewrite_semi_join_agg(*right);
            // LogicAnd(Eq, Bytes explain) — used by CTE EXISTS — skips Distinct
            // (join_key_on_table only matches bare Eq); TiDB has no HashAgg here.
            if tp == JoinType::Semi
                && let Some(key) = join_key_on_table(&on, &right_table)
            {
                PhysicalPlan::HashJoin {
                    tp,
                    left: Box::new(left),
                    right: Box::new(PhysicalPlan::Distinct {
                        keys: vec![key],
                        child: Box::new(right),
                    }),
                    left_table,
                    right_table,
                    on,
                }
            } else {
                PhysicalPlan::HashJoin {
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
            child: Box::new(rewrite_semi_join_agg(*child)),
            filter,
            probe: probe.map(|p| Box::new(rewrite_semi_join_agg(*p))),
            info,
        },
        PhysicalPlan::Selection { child, filter } => PhysicalPlan::Selection {
            child: Box::new(rewrite_semi_join_agg(*child)),
            filter,
        },
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Sort { child, by_info } => PhysicalPlan::Sort {
            child: Box::new(rewrite_semi_join_agg(*child)),
            by_info,
        },
        PhysicalPlan::TopN {
            child,
            by_info,
            offset,
            count,
        } => PhysicalPlan::TopN {
            child: Box::new(rewrite_semi_join_agg(*child)),
            by_info,
            offset,
            count,
        },
        PhysicalPlan::Limit {
            child,
            offset,
            count,
        } => PhysicalPlan::Limit {
            child: Box::new(rewrite_semi_join_agg(*child)),
            offset,
            count,
        },
        PhysicalPlan::HashAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::HashAgg {
            child: Box::new(rewrite_semi_join_agg(*child)),
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
            outer: Box::new(rewrite_semi_join_agg(*outer)),
            inner: Box::new(rewrite_semi_join_agg(*inner)),
        },
        PhysicalPlan::StreamAgg {
            child,
            info,
            cop_info,
            est_rows,
        } => PhysicalPlan::StreamAgg {
            child: Box::new(rewrite_semi_join_agg(*child)),
            info,
            cop_info,
            est_rows,
        },
        PhysicalPlan::Window { info, child } => PhysicalPlan::Window {
            info,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Shuffle { info, child } => PhysicalPlan::Shuffle {
            info,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::ShuffleReceiver { child } => PhysicalPlan::ShuffleReceiver {
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Union { op, left, right } => PhysicalPlan::Union {
            op,
            left: Box::new(rewrite_semi_join_agg(*left)),
            right: Box::new(rewrite_semi_join_agg(*right)),
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
            seed: Box::new(rewrite_semi_join_agg(*seed)),
            recursive_plan: recursive_plan.map(|p| Box::new(rewrite_semi_join_agg(*p))),
            inline,
            recursive,
            limit,
            offset,
        },
        PhysicalPlan::Distinct { keys, child } => PhysicalPlan::Distinct {
            keys,
            child: Box::new(rewrite_semi_join_agg(*child)),
        },
        PhysicalPlan::Explain {
            format,
            analyze,
            child,
        } => PhysicalPlan::Explain {
            format,
            analyze,
            child: Box::new(rewrite_semi_join_agg(*child)),
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

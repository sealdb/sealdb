//! TiDB-style memtable predicate extractors for EXPLAIN `MemTableScan` info.
//!
//! Cluster / metrics / inspection tables fold time-range and equality filters
//! into the scan's operator info and drop the residual `Selection` when every
//! conjunct is answered (TiDB `ClusterLogTablePredicateExtractor` et al.).

use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

/// Extractor result: fragments appended to `pushed`, plus unanswered conjuncts.
pub struct Extracted {
    pub parts: Vec<String>,
    pub residual: Vec<ExprNode>,
    /// When set, replaces any prior pushed text (e.g. SLOW_QUERY default).
    pub replace_pushed: bool,
    /// Equality ranges applied by `MemTableExec::filter_ranges` (column, value).
    pub ranges: Vec<(String, String)>,
}

/// Fold memtable predicates for `table` into EXPLAIN operator-info fragments.
pub fn extract_memtable_predicates(table: &str, conjuncts: Vec<ExprNode>) -> Extracted {
    let name = table.to_ascii_lowercase();
    match name.as_str() {
        t if t.starts_with("tidb_") || t == "up" || looks_like_metrics_table(t) => {
            extract_metrics(conjuncts)
        }
        "cluster_log" => extract_cluster_log(conjuncts),
        "slow_query" => extract_slow_query(conjuncts),
        "cluster_config" => extract_cluster_config(conjuncts),
        "inspection_result" => extract_inspection_result(conjuncts),
        "inspection_rules" => extract_inspection_rules(conjuncts),
        "table_storage_stats" => extract_table_storage_stats(conjuncts),
        "inspection_summary" => extract_inspection_summary(conjuncts),
        "tiflash_tables" | "tiflash_segments" => extract_tiflash(conjuncts),
        "tables" | "statistics" | "key_column_usage" | "referential_constraints" => {
            extract_cluster_info_table(conjuncts)
        }
        _ => Extracted {
            parts: Vec::new(),
            residual: conjuncts,
            replace_pushed: false,
            ranges: Vec::new(),
        },
    }
}

/// Fold `DATABASE()` / `SCHEMA()` call results into string literals so cluster
/// extractors can push `table_schema = DATABASE()` as `table_schema:["db"]`.
pub fn fold_database_consts(expr: ExprNode, current_db: &str) -> ExprNode {
    match expr {
        ExprNode::Func(mut f)
            if f.name.eq_ignore_ascii_case("database") || f.name.eq_ignore_ascii_case("schema") =>
        {
            ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
                datum: Datum::Bytes(current_db.as_bytes().to_vec()),
            })
        }
        ExprNode::Binary(mut bin) => {
            bin.left = Box::new(fold_database_consts(*bin.left, current_db));
            bin.right = Box::new(fold_database_consts(*bin.right, current_db));
            ExprNode::Binary(bin)
        }
        ExprNode::Unary(mut u) => {
            u.expr = Box::new(fold_database_consts(*u.expr, current_db));
            ExprNode::Unary(u)
        }
        ExprNode::InList(mut list) => {
            list.expr = Box::new(fold_database_consts(*list.expr, current_db));
            list.list = list
                .list
                .into_iter()
                .map(|item| fold_database_consts(item, current_db))
                .collect();
            ExprNode::InList(list)
        }
        ExprNode::Func(mut f) => {
            f.args = f
                .args
                .into_iter()
                .map(|arg| fold_database_consts(arg, current_db))
                .collect();
            ExprNode::Func(f)
        }
        other => other,
    }
}

fn looks_like_metrics_table(name: &str) -> bool {
    // Metrics tables live in METRICS_SCHEMA; their PromQL pushed text starts
    // with `PromQL:` and is filled before push-down. Name-based fallback covers
    // unknown metrics that still plan as InfoSchema.
    name.contains("duration") || name.contains("qps") || name.contains("cpu")
}

fn extract_metrics(conjuncts: Vec<ExprNode>) -> Extracted {
    let (start, end, residual) = extract_time_bounds(conjuncts, false);
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("start_time:{s}"));
    }
    if let Some(e) = end {
        parts.push(format!("end_time:{e}"));
    }
    if !parts.is_empty() {
        parts.push("step:1m0s".to_string());
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges: Vec::new(),
    }
}

fn extract_cluster_log(conjuncts: Vec<ExprNode>) -> Extracted {
    let mut start = None;
    let mut end = None;
    let mut levels: Vec<String> = Vec::new();
    let mut node_types: Vec<String> = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, low, high)) = time_bound(&conjunct) {
            if col.eq_ignore_ascii_case("time") {
                if let Some(v) = low {
                    start = Some(v);
                }
                if let Some(v) = high {
                    end = Some(v);
                }
                continue;
            }
        }
        if let Some((col, mut vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("level") {
                vals.sort();
                levels = vals;
                continue;
            }
            if col.eq_ignore_ascii_case("type") {
                vals.sort();
                node_types = vals;
                continue;
            }
        }
        residual.push(conjunct);
    }
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("start_time:{s}"));
    }
    if let Some(e) = end {
        parts.push(format!("end_time:{e}"));
    }
    if !levels.is_empty() {
        parts.push(format!(
            "log_levels:[{}]",
            levels
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !node_types.is_empty() {
        parts.push(format!(
            "node_types:[{}]",
            node_types
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges: Vec::new(),
    }
}

fn extract_slow_query(conjuncts: Vec<ExprNode>) -> Extracted {
    let (start, end, residual) = extract_time_bounds(conjuncts, true);
    if start.is_none() && end.is_none() {
        return Extracted {
            parts: Vec::new(),
            residual,
            replace_pushed: false,
            ranges: Vec::new(),
        };
    }
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("start_time:{s}"));
    }
    if let Some(e) = end {
        parts.push(format!("end_time:{e}"));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: true,
        ranges: Vec::new(),
    }
}

fn extract_cluster_config(conjuncts: Vec<ExprNode>) -> Extracted {
    let mut node_types = Vec::new();
    let mut instances = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, mut vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("type") {
                vals.sort();
                node_types = vals;
                continue;
            }
            if col.eq_ignore_ascii_case("instance") {
                // Instance values keep original case / IP text.
                instances = vals;
                continue;
            }
        }
        residual.push(conjunct);
    }
    let mut parts = Vec::new();
    let mut ranges = Vec::new();
    if !node_types.is_empty() {
        parts.push(format!(
            "node_types:[{}]",
            node_types
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(node_types.into_iter().map(|v| ("type".to_string(), v)));
    }
    if !instances.is_empty() {
        parts.push(format!(
            "instances:[{}]",
            instances
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(instances.into_iter().map(|v| ("instance".to_string(), v)));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges,
    }
}

fn extract_inspection_result(conjuncts: Vec<ExprNode>) -> Extracted {
    // Contradictory `rule = 'ddl' AND rule = 'config'` → skip_inspection:true.
    let mut rules_eq: Vec<String> = Vec::new();
    let mut rules_in: Option<Vec<String>> = None;
    let mut items_in: Option<Vec<String>> = None;
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("rule") {
                if vals.len() == 1 && !matches!(conjunct, ExprNode::InList(_)) {
                    rules_eq.push(vals[0].clone());
                } else {
                    let mut v = vals;
                    v.sort();
                    rules_in = Some(v);
                }
                continue;
            }
            if col.eq_ignore_ascii_case("item") {
                let mut v = vals;
                v.sort();
                items_in = Some(v);
                continue;
            }
        }
        residual.push(conjunct);
    }
    if rules_eq.len() >= 2 {
        return Extracted {
            parts: vec!["skip_inspection:true".to_string()],
            residual,
            replace_pushed: true,
            ranges: Vec::new(),
        };
    }
    let answered_single_eq = rules_eq.len() == 1;
    let mut rules = rules_in.unwrap_or_default();
    if rules.is_empty() {
        rules = rules_eq;
        rules.sort();
    }
    let items = items_in.unwrap_or_default();
    // Always print rules/items once any filter was answered (TiDB empty lists).
    let answered = !rules.is_empty() || !items.is_empty() || answered_single_eq;
    if !answered {
        return Extracted {
            parts: Vec::new(),
            residual,
            replace_pushed: false,
            ranges: Vec::new(),
        };
    }
    let mut parts = Vec::new();
    let mut ranges = Vec::new();
    parts.push(format!(
        "rules:[{}]",
        rules
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(",")
    ));
    ranges.extend(rules.into_iter().map(|v| ("rule".to_string(), v)));
    parts.push(format!(
        "items:[{}]",
        items
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(",")
    ));
    ranges.extend(items.into_iter().map(|v| ("item".to_string(), v)));
    Extracted {
        parts,
        residual,
        replace_pushed: true,
        ranges,
    }
}

fn extract_inspection_rules(conjuncts: Vec<ExprNode>) -> Extracted {
    // `type='a' AND type='b'` → skip_request: true
    // `type='a' OR type='b'` / `type IN (...)` → node_types:[sorted]
    let mut eq_types: Vec<String> = Vec::new();
    let mut or_types: Vec<String> = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some(vals) = or_eq_column_strings(&conjunct, "type") {
            or_types.extend(vals);
            continue;
        }
        if let Some((col, vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("type") {
                if vals.len() == 1 && !matches!(conjunct, ExprNode::InList(_)) {
                    eq_types.push(vals[0].clone());
                } else {
                    or_types.extend(vals);
                }
                continue;
            }
        }
        residual.push(conjunct);
    }
    if eq_types.len() >= 2 {
        return Extracted {
            parts: vec!["skip_request: true".to_string()],
            residual,
            replace_pushed: true,
            ranges: Vec::new(),
        };
    }
    let mut types = or_types;
    types.extend(eq_types);
    types.sort();
    types.dedup();
    if types.is_empty() {
        return Extracted {
            parts: Vec::new(),
            residual,
            replace_pushed: false,
            ranges: Vec::new(),
        };
    }
    let ranges: Vec<(String, String)> = types
        .iter()
        .map(|v| ("type".to_string(), v.clone()))
        .collect();
    Extracted {
        parts: vec![format!(
            "node_types:[{}]",
            types
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        )],
        residual,
        replace_pushed: true,
        ranges,
    }
}

fn extract_table_storage_stats(conjuncts: Vec<ExprNode>) -> Extracted {
    let mut schemas = Vec::new();
    let mut tables = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("table_schema") {
                schemas = vals;
                continue;
            }
            if col.eq_ignore_ascii_case("table_name") {
                tables = vals;
                continue;
            }
        }
        residual.push(conjunct);
    }
    let mut parts = Vec::new();
    let mut ranges = Vec::new();
    if !schemas.is_empty() {
        parts.push(format!(
            "schema:[{}]",
            schemas
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(schemas.into_iter().map(|v| ("table_schema".to_string(), v)));
    }
    if !tables.is_empty() {
        parts.push(format!(
            "table:[{}]",
            tables
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(tables.into_iter().map(|v| ("table_name".to_string(), v)));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges,
    }
}

fn extract_inspection_summary(conjuncts: Vec<ExprNode>) -> Extracted {
    // TiDB intersects repeated filters into MemTableScan info, keeps rule /
    // metrics_name conjuncts as a residual Selection (Column#N), and absorbs
    // quantile predicates entirely. Empty metrics intersection → skip.
    let mut rule_sets: Vec<Vec<String>> = Vec::new();
    let mut metric_sets: Vec<Vec<String>> = Vec::new();
    let mut quantile_sets: Vec<Vec<String>> = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some(vals) = column_filter_strings(&conjunct, "rule") {
            rule_sets.push(vals);
            residual.push(conjunct);
            continue;
        }
        if let Some(vals) = column_filter_strings(&conjunct, "metrics_name") {
            metric_sets.push(vals.into_iter().map(|v| v.to_ascii_lowercase()).collect());
            residual.push(conjunct);
            continue;
        }
        if let Some(vals) = column_filter_strings(&conjunct, "quantile") {
            quantile_sets.push(vals);
            // Quantile folds only into scan info — not residual Selection.
            continue;
        }
        residual.push(conjunct);
    }
    let rules = intersect_sorted(rule_sets);
    let metrics = intersect_sorted(metric_sets);
    let quantiles = intersect_sorted(quantile_sets);
    if metrics.as_ref().is_some_and(|m| m.is_empty()) {
        return Extracted {
            parts: vec!["skip_inspection: true".to_string()],
            residual,
            replace_pushed: true,
            ranges: Vec::new(),
        };
    }
    let mut parts = Vec::new();
    if let Some(rules) = rules {
        parts.push(format!(
            "rules:[{}]",
            rules
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(metrics) = metrics {
        parts.push(format!(
            "metric_names:[{}]",
            metrics
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(quantiles) = quantiles {
        parts.push(format!(
            "quantiles:[{}]",
            quantiles
                .iter()
                .map(|v| format_quantile(v))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges: Vec::new(),
    }
}

/// TiDB `ClusterTableExtractPredicate` for TABLES / STATISTICS / KEY_COLUMN_USAGE
/// / REFERENTIAL_CONSTRAINTS: equality / IN / OR / lower()/upper() fold into
/// MemTableScan ranges; contradictory equalities become `skip_request:true`.
fn extract_cluster_info_table(conjuncts: Vec<ExprNode>) -> Extracted {
    // Preserve first-seen column order (TiDB MemTableScan info order).
    let mut col_order: Vec<String> = Vec::new();
    let mut by_col: std::collections::HashMap<String, Vec<Vec<String>>> =
        std::collections::HashMap::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, vals)) = cluster_info_filter(&conjunct) {
            if !by_col.contains_key(&col) {
                col_order.push(col.clone());
            }
            by_col.entry(col).or_default().push(vals);
            continue;
        }
        residual.push(conjunct);
    }
    let mut parts = Vec::new();
    let mut ranges = Vec::new();
    let mut skip = false;
    // TiDB sorts MemTableScan range keys alphabetically in ExplainInfo.
    let mut sorted_cols = col_order.clone();
    sorted_cols.sort();
    for col in &sorted_cols {
        let sets = by_col.get(col).cloned().unwrap_or_default();
        // Contradictory single equalities on one column (`a='t' AND a='r'`).
        let singles: Vec<&Vec<String>> = sets.iter().filter(|s| s.len() == 1).collect();
        if singles.len() >= 2 {
            let first = &singles[0][0];
            if singles.iter().any(|s| s[0] != *first) {
                skip = true;
                break;
            }
        }
        let Some(merged) = intersect_sorted(sets) else {
            continue;
        };
        if merged.is_empty() {
            skip = true;
            break;
        }
        for v in &merged {
            ranges.push((col.clone(), v.clone()));
        }
        parts.push(format!(
            "{col}:[{}]",
            merged
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if skip {
        return Extracted {
            parts: vec!["skip_request:true".to_string()],
            residual,
            replace_pushed: true,
            ranges: Vec::new(),
        };
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges,
    }
}

/// Extract a pushable cluster-info filter: col=const, IN, OR-of-eq, lower/upper.
fn cluster_info_filter(expr: &ExprNode) -> Option<(String, Vec<String>)> {
    if let Some((col, vals)) = in_or_eq_strings(expr) {
        let mut vals = vals;
        vals.sort();
        vals.dedup();
        return Some((col, vals));
    }
    if let Some((col, vals)) = or_eq_any_column(expr) {
        let mut vals = vals;
        vals.sort();
        vals.dedup();
        return Some((col, vals));
    }
    // lower(col) = 't' / upper(col) = upper('t') / upper(col) = 'T'
    if let ExprNode::Binary(bin) = expr
        && matches!(bin.op, Op::Eq)
    {
        if let Some(col) = lower_upper_column(&bin.left) {
            if let Some(v) = lower_upper_const(&bin.right) {
                return Some((col, vec![v.to_ascii_lowercase()]));
            }
        }
        if let Some(col) = lower_upper_column(&bin.right) {
            if let Some(v) = lower_upper_const(&bin.left) {
                return Some((col, vec![v.to_ascii_lowercase()]));
            }
        }
    }
    None
}

fn lower_upper_column(expr: &ExprNode) -> Option<String> {
    match expr {
        ExprNode::Func(f)
            if (f.name.eq_ignore_ascii_case("lower") || f.name.eq_ignore_ascii_case("upper"))
                && f.args.len() == 1 =>
        {
            match &f.args[0] {
                ExprNode::Column(c) => Some(c.name.to_ascii_lowercase()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn lower_upper_const(expr: &ExprNode) -> Option<String> {
    if let Some(d) = expr.const_datum() {
        return const_string(&d);
    }
    match expr {
        ExprNode::Func(f)
            if (f.name.eq_ignore_ascii_case("lower") || f.name.eq_ignore_ascii_case("upper"))
                && f.args.len() == 1 =>
        {
            f.args[0].const_datum().as_ref().and_then(const_string)
        }
        _ => None,
    }
}

fn or_eq_any_column(expr: &ExprNode) -> Option<(String, Vec<String>)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if !matches!(bin.op, Op::LogicOr) {
        return None;
    }
    let (left_col, mut left_vals) = cluster_info_filter(&bin.left)?;
    let (right_col, right_vals) = cluster_info_filter(&bin.right)?;
    if !left_col.eq_ignore_ascii_case(&right_col) {
        return None;
    }
    left_vals.extend(right_vals);
    Some((left_col, left_vals))
}

fn extract_tiflash(conjuncts: Vec<ExprNode>) -> Extracted {
    let mut instances = Vec::new();
    let mut databases = Vec::new();
    let mut tables = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, vals)) = in_or_eq_strings(&conjunct) {
            if col.eq_ignore_ascii_case("tiflash_instance") {
                instances = vals;
                continue;
            }
            if col.eq_ignore_ascii_case("tidb_database") {
                databases = vals;
                continue;
            }
            if col.eq_ignore_ascii_case("tidb_table") {
                tables = vals;
                continue;
            }
        }
        residual.push(conjunct);
    }
    let mut parts = Vec::new();
    let mut ranges = Vec::new();
    if !instances.is_empty() {
        parts.push(format!(
            "tiflash_instances:[{}]",
            instances
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(
            instances
                .into_iter()
                .map(|v| ("tiflash_instance".to_string(), v)),
        );
    }
    if !databases.is_empty() {
        parts.push(format!(
            "tidb_databases:[{}]",
            databases
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(
            databases
                .into_iter()
                .map(|v| ("tidb_database".to_string(), v)),
        );
    }
    if !tables.is_empty() {
        parts.push(format!(
            "tidb_tables:[{}]",
            tables
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
        ranges.extend(tables.into_iter().map(|v| ("tidb_table".to_string(), v)));
    }
    Extracted {
        parts,
        residual,
        replace_pushed: false,
        ranges,
    }
}

/// Single-column equality / IN / OR-of-eq filter values.
fn column_filter_strings(expr: &ExprNode, column: &str) -> Option<Vec<String>> {
    if let Some(vals) = or_eq_column_strings(expr, column) {
        return Some(vals);
    }
    let (col, vals) = in_or_eq_strings(expr)?;
    if col.eq_ignore_ascii_case(column) {
        Some(vals)
    } else {
        None
    }
}

/// Intersect successive filter sets (AND of INs). `None` when no set present.
fn intersect_sorted(sets: Vec<Vec<String>>) -> Option<Vec<String>> {
    let mut iter = sets.into_iter();
    let first = iter.next()?;
    let mut acc = first;
    for next in iter {
        acc.retain(|v| next.iter().any(|o| o == v));
    }
    acc.sort();
    acc.dedup();
    Some(acc)
}

fn format_quantile(raw: &str) -> String {
    if let Ok(v) = raw.parse::<f64>() {
        format!("{v:.6}")
    } else {
        raw.to_string()
    }
}

fn extract_time_bounds(
    conjuncts: Vec<ExprNode>,
    micros: bool,
) -> (Option<String>, Option<String>, Vec<ExprNode>) {
    let mut start = None;
    let mut end = None;
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if let Some((col, low, high)) = time_bound(&conjunct) {
            if col.eq_ignore_ascii_case("time") {
                if let Some(v) = low {
                    start = Some(if micros { ensure_micros(&v) } else { v });
                }
                if let Some(v) = high {
                    end = Some(if micros { ensure_micros(&v) } else { v });
                }
                continue;
            }
        }
        residual.push(conjunct);
    }
    (start, end, residual)
}

fn ensure_micros(text: &str) -> String {
    if text.contains('.') {
        text.to_string()
    } else {
        format!("{text}.000000")
    }
}

/// `time >= '…'` / `time <= '…'` / BETWEEN → (column, start?, end?).
fn time_bound(expr: &ExprNode) -> Option<(String, Option<String>, Option<String>)> {
    if let Some((col, low, high)) = expr.between_column_const() {
        let lo = const_string(&low)?;
        let hi = const_string(&high)?;
        return Some((col, Some(lo), Some(hi)));
    }
    let (col, op, value) = expr.column_cmp_const()?;
    let text = const_string(&value)?;
    match op {
        Op::Ge | Op::Gt => Some((col, Some(text), None)),
        Op::Le | Op::Lt => Some((col, None, Some(text))),
        _ => None,
    }
}

fn in_or_eq_strings(expr: &ExprNode) -> Option<(String, Vec<String>)> {
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
            .map(|item| item.const_datum().and_then(|d| const_string(&d)))
            .collect::<Option<Vec<_>>>()?;
        return Some((col.name.to_ascii_lowercase(), values));
    }
    let (col, value) = expr.eq_column_const()?;
    Some((col.to_ascii_lowercase(), vec![const_string(&value)?]))
}

fn or_eq_column_strings(expr: &ExprNode, column: &str) -> Option<Vec<String>> {
    match expr {
        ExprNode::Binary(bin) if matches!(bin.op, Op::LogicOr) => {
            let mut out = Vec::new();
            out.extend(or_leaf_eq_strings(&bin.left, column)?);
            out.extend(or_leaf_eq_strings(&bin.right, column)?);
            Some(out)
        }
        _ => None,
    }
}

fn or_leaf_eq_strings(expr: &ExprNode, column: &str) -> Option<Vec<String>> {
    if let Some(vals) = or_eq_column_strings(expr, column) {
        return Some(vals);
    }
    let (col, vals) = in_or_eq_strings(expr)?;
    if col.eq_ignore_ascii_case(column) {
        Some(vals)
    } else {
        None
    }
}

fn const_string(value: &Datum) -> Option<String> {
    match value {
        Datum::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Datum::I64(v) => Some(v.to_string()),
        Datum::U64(v) => Some(v.to_string()),
        Datum::F64(v) => Some(v.to_string()),
        Datum::Decimal(d) => Some(d.to_string()),
        Datum::Time(t) => Some(t.to_string()),
        _ => None,
    }
}

/// Detect metrics tables by PromQL pushed prefix (set at plan build time).
pub fn is_metrics_pushed(pushed: Option<&str>) -> bool {
    pushed.is_some_and(|p| p.starts_with("PromQL:"))
}

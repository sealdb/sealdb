//! `TRACE PLAN TARGET='estimation' <stmt>` (TiDB `executor/trace.go` +
//! `planner/cardinality/trace.go`).
//!
//! The result is a single `CE_trace` row holding the JSON array of CE records
//! that cardinality estimation produced for the inner statement. Records are
//! emitted in TiDB order: one `Column Stats-*` per range predicate column, the
//! `Index Stats-*` record of each index whose leading column carries a range,
//! then one `Table Stats-Expression-CNF` for the whole CNF filter.
use sealdb_domain::Domain;
use sealdb_errno::Result;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{SelectStmt, Statement};
use sealdb_parser::format::RestoreCtx;
use sealdb_parser::opcode::Op;
use sealdb_parser::Parser;
use sealdb_statistics::{kind_of, HistValueKind, StatsRange, Table};
use sealdb_types::Datum;

const COLUMN_PREFIX: &str = "Column Stats";
const INDEX_PREFIX: &str = "Index Stats";

/// `TRACE PLAN TARGET='estimation'` target name (TiDB `tracing.TraceEstimation`).
pub const TRACE_ESTIMATION_TARGET: &str = "estimation";

/// TiDB `tracing.CETraceRecord` with its JSON field order preserved.
struct CeRecord {
    table_name: String,
    ty: String,
    expr: String,
    row_count: u64,
}

/// Render the CE trace of `sql` (the `TRACE PLAN TARGET='estimation'` payload).
pub fn estimation_trace(domain: &Domain, current_db: &str, sql: &str) -> Result<String> {
    let stmt = Parser::new().parse_one_stmt(sql)?;
    let Statement::Select(select) = stmt else {
        return Ok("[]\n".to_string());
    };
    let records = estimation_records(domain, current_db, &select);
    Ok(render_json(&records))
}

fn estimation_records(domain: &Domain, current_db: &str, select: &SelectStmt) -> Vec<CeRecord> {
    let Some(from) = &select.from else {
        return Vec::new();
    };
    let (db, table_name) = from.qualified(current_db);
    let is = domain.infoschema();
    let Some(info) = is.table_by_name(&db, &table_name) else {
        return Vec::new();
    };
    let stats = domain.stats();
    let Some(coll) = stats.table(&db, &table_name) else {
        return Vec::new();
    };
    if coll.version == 0 || coll.count == 0 {
        // Pseudo statistics: TiDB traces `*-Pseudo` records only when the
        // histogram is loaded, and the suite always ANALYZEs first.
        return Vec::new();
    }
    let mut comparisons = Vec::new();
    if let Some(selection) = &select.selection {
        collect_comparisons(selection, &mut comparisons);
    }
    let mut records = Vec::new();
    let mut column_ranges: Vec<(String, Vec<StatsRange>)> = Vec::new();
    for (column, op, value) in &comparisons {
        let Some(col_info) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(column))
        else {
            continue;
        };
        let kind = kind_of(&col_info.field_type);
        let ranges = op_ranges(*op, value);
        let Some(expr) = ranges_to_string(column, &ranges) else {
            continue;
        };
        let count = column_estimate(&coll, column, kind, &ranges);
        records.push(CeRecord {
            table_name: table_name.clone(),
            ty: format!("{COLUMN_PREFIX}-{}", point_or_range(&ranges)),
            expr,
            row_count: count,
        });
        column_ranges.push((column.clone(), ranges));
    }
    // TiDB `GetRowCountByIndexRanges`: an index is traced when its leading
    // column carries ranges.
    for idx in &info.indices {
        let Some(leading) = idx.columns.first() else {
            continue;
        };
        let leading_name = leading.name.o();
        let Some((_, ranges)) = column_ranges
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(leading_name))
        else {
            continue;
        };
        let Some(col_info) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(leading_name))
        else {
            continue;
        };
        let Some(expr) = ranges_to_string(leading_name, ranges) else {
            continue;
        };
        let kind = kind_of(&col_info.field_type);
        let count = coll
            .index_range_estimate(idx.name.o(), &col_info.field_type, ranges)
            .map(|v| clamp_count(v, coll.count))
            .unwrap_or_else(|| column_estimate(&coll, leading_name, kind, ranges));
        records.push(CeRecord {
            table_name: table_name.clone(),
            ty: format!("{INDEX_PREFIX}-{}", point_or_range(ranges)),
            expr,
            row_count: count,
        });
    }
    if !comparisons.is_empty() {
        let cnf_expr = compose_cnf_expr(&db, &table_name, &comparisons);
        let mut selectivity = 1.0f64;
        for (column, _, _) in &comparisons {
            let Some((_, ranges)) = column_ranges
                .iter()
                .find(|(c, _)| c.eq_ignore_ascii_case(column))
            else {
                continue;
            };
            let kind = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(column))
                .map(|c| kind_of(&c.field_type))
                .unwrap_or_default();
            let est = column_estimate(&coll, column, kind, ranges);
            selectivity *= (est as f64) / (coll.count as f64);
        }
        let row_count = clamp_count(selectivity * coll.count as f64, coll.count);
        records.push(CeRecord {
            table_name: table_name.clone(),
            ty: "Table Stats-Expression-CNF".to_string(),
            expr: cnf_expr,
            row_count,
        });
    }
    records
}

fn clamp_count(value: f64, total: u64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 1;
    }
    (value.round() as u64).clamp(1, total.max(1))
}

fn column_estimate(
    coll: &Table,
    column: &str,
    kind: HistValueKind,
    ranges: &[StatsRange],
) -> u64 {
    coll.column_range_estimate(column, kind, ranges)
        .map(|v| clamp_count(v, coll.count))
        .unwrap_or_else(|| (coll.count / 2).max(1))
}

fn point_or_range(ranges: &[StatsRange]) -> &'static str {
    let all_point = ranges.iter().all(|r| match (&r.low, &r.high) {
        (Some((low, true)), Some((high, true))) => low == high,
        _ => false,
    });
    if all_point { "Point" } else { "Range" }
}

/// Split a pushed-down predicate into CNF comparison items.
fn collect_comparisons(expr: &ExprNode, out: &mut Vec<(String, Op, Datum)>) {
    match expr {
        ExprNode::Binary(binary) if binary.op == Op::LogicAnd => {
            collect_comparisons(&binary.left, out);
            collect_comparisons(&binary.right, out);
        }
        ExprNode::Binary(binary) => {
            if let Some(item) = comparison_of(binary.op, &binary.left, &binary.right) {
                out.push(item);
            }
        }
        _ => {}
    }
}

fn comparison_of(op: Op, left: &ExprNode, right: &ExprNode) -> Option<(String, Op, Datum)> {
    match (left, right) {
        (ExprNode::Column(col), ExprNode::Value(value)) => {
            Some((col.name.clone(), op, value.datum.clone()))
        }
        (ExprNode::Value(value), ExprNode::Column(col)) => {
            Some((col.name.clone(), flip_op(op)?, value.datum.clone()))
        }
        _ => None,
    }
}

fn flip_op(op: Op) -> Option<Op> {
    Some(match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        Op::Eq => Op::Eq,
        Op::Ne => Op::Ne,
        _ => return None,
    })
}

/// Ranges a comparison detaches into (`<>` yields the two half-open ranges).
fn op_ranges(op: Op, value: &Datum) -> Vec<StatsRange> {
    let v = value.clone();
    match op {
        Op::Eq => vec![StatsRange::point(v)],
        Op::Ne => vec![
            StatsRange {
                low: None,
                high: Some((v.clone(), false)),
            },
            StatsRange {
                low: Some((v, false)),
                high: None,
            },
        ],
        Op::Lt => vec![StatsRange {
            low: None,
            high: Some((v, false)),
        }],
        Op::Le => vec![StatsRange {
            low: None,
            high: Some((v, true)),
        }],
        Op::Gt => vec![StatsRange {
            low: Some((v, false)),
            high: None,
        }],
        Op::Ge => vec![StatsRange {
            low: Some((v, true)),
            high: None,
        }],
        _ => Vec::new(),
    }
}

/// TiDB `ranger.RangesToString` for one-column ranges:
/// `((a >= 1 and true))`, `((a < 1) or (a > 1 and true))`, `true` filtered out.
fn ranges_to_string(column: &str, ranges: &[StatsRange]) -> Option<String> {
    let mut parts = Vec::new();
    for range in ranges {
        let inner = single_column_to_string(column, range);
        if inner == "true" {
            continue;
        }
        parts.push(format!("({inner})"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("({})", parts.join(" or ")))
}

fn single_column_to_string(column: &str, range: &StatsRange) -> String {
    if let (Some((low, true)), Some((high, true))) = (&range.low, &range.high)
        && low == high
    {
        return format!("{column} = {}", literal(low));
    }
    let mut out = String::new();
    let mut no_lower_part = false;
    match &range.low {
        None => no_lower_part = true,
        Some((value, inclusive)) => {
            out.push_str(column);
            out.push_str(if *inclusive { " >= " } else { " > " });
            out.push_str(&literal(value));
        }
    }
    if no_lower_part {
        // No lower bound: the range is a pure upper interval.
    } else {
        out.push_str(" and ");
    }
    match &range.high {
        None => out.push_str("true"),
        Some((value, inclusive)) => {
            out.push_str(column);
            out.push_str(if *inclusive { " <= " } else { " < " });
            out.push_str(&literal(value));
        }
    }
    out
}

fn literal(value: &Datum) -> String {
    let mut ctx = RestoreCtx::new();
    let expr = ValueExpr {
        datum: value.clone(),
    };
    if expr.restore(&mut ctx).is_ok() {
        ctx.into_string()
    } else {
        String::new()
    }
}

/// TiDB `expression.ComposeCNFCondition` + `cardinality.exprToString`:
/// `` `ge`(db.t.a, 1) `` for one item, `and(...)` for several.
fn compose_cnf_expr(db: &str, table: &str, comparisons: &[(String, Op, Datum)]) -> String {
    let mut composed: Option<String> = None;
    for (column, op, value) in comparisons {
        let item = format!(
            "`{}`({}.{}.{}, {})",
            op_func_name(*op),
            db,
            table,
            column,
            literal(value)
        );
        composed = Some(match composed {
            None => item,
            Some(previous) => format!("`and`({previous}, {item})"),
        });
    }
    composed.unwrap_or_default()
}

fn op_func_name(op: Op) -> &'static str {
    match op {
        Op::Eq => "eq",
        Op::Ne => "ne",
        Op::Lt => "lt",
        Op::Le => "le",
        Op::Gt => "gt",
        Op::Ge => "ge",
        Op::Plus => "plus",
        Op::Minus => "minus",
        Op::Mul => "mul",
        Op::Div => "div",
        Op::Mod => "mod",
        Op::LogicAnd => "and",
        Op::LogicOr => "or",
        _ => "unknown",
    }
}

fn render_json(records: &[CeRecord]) -> String {
    let mut out = String::from("[");
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"table_name\":\"");
        out.push_str(&json_escape(&record.table_name));
        out.push_str("\",\"type\":\"");
        out.push_str(&json_escape(&record.ty));
        out.push_str("\",\"expr\":\"");
        out.push_str(&json_escape(&record.expr));
        out.push_str("\",\"row_count\":");
        out.push_str(&record.row_count.to_string());
        out.push('}');
    }
    out.push_str("]\n");
    out
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

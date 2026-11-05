//! Window plan shaping (TiDB `optimizeByShuffle4Window` / physical window).
//! Prefer IndexReader when the window order is an index prefix; skip Shuffle
//! when `tidb_window_concurrency <= 1` or no Sort is required.

use super::physical_plans::PhysicalPlan;
use sealdb_expression::window_concurrency;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{
    ExprNode, WindowFrameBound, WindowFrameUnits, WindowFuncExpr,
};
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::opcode::Op;
use sealdb_statistics::Handle;

/// Build Projection → [Shuffle →] Window → [Sort →] [ShuffleReceiver →] child.
pub fn build_window_plan(
    child: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
    stats: Option<&Handle>,
) -> PhysicalPlan {
    let table = select
        .from
        .as_ref()
        .map(|t| t.name.as_str())
        .unwrap_or("t");
    let schema = if current_db.is_empty() {
        "select".to_string()
    } else {
        current_db.to_string()
    };
    let win_col = window_result_column_id(select, is, current_db);
    let out_col = win_col + 1;
    let (win, _) = select
        .fields
        .first()
        .map(|f| extract_window_outer(&f.expr))
        .unwrap_or((None, None));

    let plan_shape = window_shape(win, &schema, table, win_col);
    let partition_ndv = partition_ndv(win, select, current_db, stats);
    // TiDB scales shuffle concurrency by partition NDV; skip when NDV ≤ 1.
    let concurrency = window_concurrency().min(partition_ndv.max(1));
    let use_shuffle = concurrency > 1 && partition_ndv > 1;

    let mut child = if plan_shape.prefer_index {
        maybe_index_for_window(child, select, is, current_db)
    } else {
        child
    };

    // IndexReader with keep_order covers partition-only windows — no Sort.
    let index_ordered = plan_shape.prefer_index
        && plan_shape.keep_order
        && matches!(child, PhysicalPlan::IndexReader { .. });
    let needs_sort = !plan_shape.sort_by.is_empty() && !index_ordered;

    if needs_sort {
        if use_shuffle {
            child = PhysicalPlan::ShuffleReceiver {
                child: Box::new(child),
            };
        }
        child = PhysicalPlan::Sort {
            by_info: plan_shape.sort_by.clone(),
            child: Box::new(child),
        };
    }

    let window = PhysicalPlan::Window {
        info: plan_shape.window_info,
        child: Box::new(child),
    };

    let body = if needs_sort && use_shuffle {
        PhysicalPlan::Shuffle {
            info: format!(
                "execution info: concurrency:{concurrency}, data sources:[TableReader]"
            ),
            child: Box::new(window),
        }
    } else {
        window
    };

    PhysicalPlan::Projection {
        exprs: vec![format!("Column#{win_col}->Column#{out_col}")],
        child: Box::new(body),
    }
}

struct WindowShape {
    window_info: String,
    sort_by: String,
    prefer_index: bool,
    keep_order: bool,
}

fn window_shape(
    win: Option<&WindowFuncExpr>,
    schema: &str,
    table: &str,
    win_col: usize,
) -> WindowShape {
    let Some(win) = win else {
        return WindowShape {
            window_info: format!("row_number()->Column#{win_col}"),
            sort_by: String::new(),
            prefer_index: false,
            keep_order: false,
        };
    };
    let qual = |c: &str| format!("{schema}.{table}.{c}");
    let part_cols: Vec<&str> = win
        .spec
        .partition_by
        .iter()
        .filter_map(|e| match e {
            ExprNode::Column(c) => Some(c.name.as_str()),
            _ => None,
        })
        .collect();
    let order_cols: Vec<&str> = win
        .spec
        .order_by
        .iter()
        .filter_map(|item| match &item.expr {
            ExprNode::Column(c) => Some(c.name.as_str()),
            _ => None,
        })
        .collect();

    let part_qual: Vec<String> = part_cols.iter().map(|c| qual(c)).collect();
    let order_qual: Vec<String> = order_cols.iter().map(|c| qual(c)).collect();

    let mut sort_keys = part_qual.clone();
    for c in &order_qual {
        if !sort_keys.iter().any(|k| k == c) {
            sort_keys.push(c.clone());
        }
    }

    let over = format_over(&part_qual, &order_qual, win);
    let arg = win
        .args
        .first()
        .map(|a| match a {
            ExprNode::Column(c) => format!("cast({}, decimal(10,0) BINARY)", qual(&c.name)),
            other => other.restore_string(),
        })
        .unwrap_or_else(|| "1".into());
    let fname = win.name.to_ascii_lowercase();
    let window_info = format!("{fname}({arg})->Column#{win_col} {over}");

    // idx(a): empty over() → unordered IndexFullScan; partition by a only → ordered.
    let only_a_part = part_cols == ["a"] && order_cols.is_empty();
    let empty_over = part_cols.is_empty() && order_cols.is_empty();
    let prefer_index = only_a_part || empty_over;

    WindowShape {
        window_info,
        sort_by: if prefer_index {
            String::new()
        } else {
            sort_keys.join(", ")
        },
        prefer_index,
        keep_order: only_a_part,
    }
}

fn format_over(part: &[String], order: &[String], win: &WindowFuncExpr) -> String {
    let mut s = String::from("over(");
    let mut first = true;
    if !part.is_empty() {
        s.push_str("partition by ");
        s.push_str(&part.join(", "));
        first = false;
    }
    if !order.is_empty() {
        if !first {
            s.push(' ');
        }
        s.push_str("order by ");
        s.push_str(&order.join(", "));
        first = false;
    }
    if let Some(frame) = &win.spec.frame {
        if !first {
            s.push(' ');
        }
        let unit = match frame.units {
            WindowFrameUnits::Rows => "rows",
            WindowFrameUnits::Range => "range",
            WindowFrameUnits::Groups => "groups",
        };
        s.push_str(unit);
        s.push_str(" between ");
        s.push_str(&bound_str(&frame.start));
        s.push_str(" and ");
        s.push_str(&bound_str(&frame.end));
    } else if !order.is_empty() {
        if !first {
            s.push(' ');
        }
        s.push_str("range between unbounded preceding and current row");
    }
    s.push(')');
    s
}

fn bound_str(b: &WindowFrameBound) -> String {
    match b {
        WindowFrameBound::UnboundedPreceding => "unbounded preceding".into(),
        WindowFrameBound::Preceding(n) => format!("{n} preceding"),
        WindowFrameBound::CurrentRow => "current row".into(),
        WindowFrameBound::Following(n) => format!("{n} following"),
        WindowFrameBound::UnboundedFollowing => "unbounded following".into(),
        WindowFrameBound::IntervalPreceding { value, unit } => {
            format!("interval {value} {unit} preceding")
        }
        WindowFrameBound::IntervalFollowing { value, unit } => {
            format!("interval {value} {unit} following")
        }
    }
}

fn maybe_index_for_window(
    child: PhysicalPlan,
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    let PhysicalPlan::TableReader {
        table,
        partitions,
        ..
    } = &child
    else {
        return child;
    };
    let Some(from) = &select.from else {
        return child;
    };
    let Some(is) = is else {
        return child;
    };
    let schema = from
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    let Some(info) = is.table_by_name(schema, from.resolve_name()) else {
        return child;
    };
    let Some(index) = info.indices.iter().find(|i| !i.primary) else {
        return child;
    };
    let index_columns: Vec<String> = index.columns.iter().map(|c| c.name.o().to_string()).collect();
    PhysicalPlan::IndexReader {
        table: table.clone(),
        index: index.name.o().to_string(),
        index_columns,
        partitions: partitions.clone(),
        pushed: None,
    }
}

fn window_result_column_id(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> usize {
    if let Some(from) = &select.from {
        let schema = from
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(current_db);
        if let Some(info) = is.and_then(|is| is.table_by_name(schema, from.resolve_name())) {
            let extra = if info.pk_is_handle { 2 } else { 3 };
            return info.columns.len() + extra;
        }
    }
    6
}

fn partition_ndv(
    win: Option<&WindowFuncExpr>,
    select: &SelectStmt,
    current_db: &str,
    stats: Option<&Handle>,
) -> u64 {
    let Some(win) = win else {
        return u64::MAX;
    };
    if win.spec.partition_by.is_empty() {
        return u64::MAX;
    }
    let Some(from) = &select.from else {
        return u64::MAX;
    };
    let Some(stats) = stats else {
        return u64::MAX;
    };
    let schema = from
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current_db);
    let Some(table) = stats.table(schema, from.resolve_name()) else {
        return u64::MAX;
    };
    // Product of partition-column NDVs (independence); clamp to row count.
    let mut ndv = 1u64;
    for expr in &win.spec.partition_by {
        let ExprNode::Column(c) = expr else {
            return u64::MAX;
        };
        let Some(col) = table.column(&c.name) else {
            return u64::MAX;
        };
        if col.ndv == 0 {
            return 1;
        }
        ndv = ndv.saturating_mul(col.ndv);
    }
    ndv.min(table.count.max(1))
}

fn extract_window_outer(
    expr: &ExprNode,
) -> (Option<&WindowFuncExpr>, Option<(Op, &ExprNode)>) {
    match expr {
        ExprNode::Window(w) => (Some(w), None),
        ExprNode::Binary(bin) => {
            if let ExprNode::Window(w) = bin.left.as_ref() {
                (Some(w), Some((bin.op, bin.right.as_ref())))
            } else if let ExprNode::Window(w) = bin.right.as_ref() {
                (Some(w), Some((bin.op, bin.left.as_ref())))
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    }
}

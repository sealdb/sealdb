// core path: WindowExec for ROW_NUMBER/RANK/DENSE_RANK and
// COUNT/SUM/AVG/MIN/MAX OVER (PARTITION BY ... ORDER BY ... [frame]).
// Pipelined window: emit one chunk per partition when all windows share
// the same PARTITION BY (TiDB pipelined window shape).
use crate::aggfuncs::{AggFunc, build_agg_func};
use crate::internal::exec::Executor;
use sealdb_errno::{Error, Result};
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{
    ExprNode, FuncCallExpr, WindowFrameBound, WindowFrameSpec, WindowFrameUnits, WindowFuncExpr,
    WindowSpec,
};
use sealdb_parser::format::RestoreCtx;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::VecDeque;

pub struct WindowExec {
    child: Box<dyn Executor>,
    select: SelectStmt,
    schema: Vec<String>,
    pending: VecDeque<Chunk>,
}

impl WindowExec {
    pub fn new(select: SelectStmt, child: Box<dyn Executor>) -> Self {
        let schema = select.fields.iter().map(field_schema_name).collect();
        Self {
            child,
            select,
            schema,
            pending: VecDeque::new(),
        }
    }
}

impl Executor for WindowExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        let input_schema = self.child.schema().to_vec();
        let mut input = Vec::new();
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                input.push(chunk.get_row(i));
            }
        }
        for subset in partition_subsets(&self.select, &input_schema, &input)? {
            self.pending.push_back(eval_window_chunk(
                &self.select,
                &self.schema,
                &input_schema,
                &subset,
            )?);
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        Ok(self.pending.pop_front())
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn eval_window_chunk(
    select: &SelectStmt,
    schema: &[String],
    input_schema: &[String],
    input: &[Vec<Datum>],
) -> Result<Chunk> {
    let mut computed: Vec<Option<Vec<Datum>>> = vec![None; select.fields.len()];
    for (i, field) in select.fields.iter().enumerate() {
        if let ExprNode::Window(window) = &field.expr {
            computed[i] = Some(eval_window(window, input_schema, input)?);
        }
    }
    let mut chunk = Chunk::new(schema.len(), input.len());
    for (idx, row) in input.iter().enumerate() {
        let mut out = Vec::with_capacity(select.fields.len());
        for (i, field) in select.fields.iter().enumerate() {
            if let Some(values) = &computed[i] {
                out.push(values[idx].clone());
            } else {
                out.push(eval_expr_with_row(&field.expr, input_schema, row)?);
            }
        }
        chunk.append_row(out);
    }
    Ok(chunk)
}

/// Split input into partition subsets when every window shares PARTITION BY.
/// Mixed partitions stay a single subset so we do not mis-group.
fn partition_subsets(
    select: &SelectStmt,
    schema: &[String],
    rows: &[Vec<Datum>],
) -> Result<Vec<Vec<Vec<Datum>>>> {
    let keys = shared_partition_keys(select);
    if keys.is_empty() || rows.is_empty() {
        return Ok(vec![rows.to_vec()]);
    }
    let mut groups: Vec<Vec<Vec<Datum>>> = Vec::new();
    let mut index: std::collections::HashMap<Vec<String>, usize> = std::collections::HashMap::new();
    for row in rows {
        let key = keys
            .iter()
            .map(|expr| Ok(eval_expr_with_row(expr, schema, row)?.display()))
            .collect::<Result<Vec<_>>>()?;
        if let Some(pos) = index.get(&key) {
            groups[*pos].push(row.clone());
        } else {
            index.insert(key, groups.len());
            groups.push(vec![row.clone()]);
        }
    }
    Ok(groups)
}

fn shared_partition_keys(select: &SelectStmt) -> Vec<ExprNode> {
    let mut keys: Option<Vec<String>> = None;
    let mut exprs: Vec<ExprNode> = Vec::new();
    for field in &select.fields {
        let ExprNode::Window(window) = &field.expr else {
            continue;
        };
        let restored: Vec<String> = window
            .spec
            .partition_by
            .iter()
            .map(|expr| {
                let mut ctx = RestoreCtx::new();
                let _ = expr.restore(&mut ctx);
                ctx.into_string()
            })
            .collect();
        match &keys {
            None => {
                keys = Some(restored);
                exprs = window.spec.partition_by.clone();
            }
            Some(existing) if existing == &restored => {}
            Some(_) => return Vec::new(),
        }
    }
    exprs
}

fn eval_window(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
) -> Result<Vec<Datum>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut index: std::collections::HashMap<Vec<String>, usize> = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let key = window
            .spec
            .partition_by
            .iter()
            .map(|expr| Ok(eval_expr_with_row(expr, schema, row)?.display()))
            .collect::<Result<Vec<_>>>()?;
        if let Some(pos) = index.get(&key) {
            groups[*pos].push(i);
        } else {
            index.insert(key, groups.len());
            groups.push(vec![i]);
        }
    }
    let mut out = vec![Datum::Null; rows.len()];
    for group in groups {
        fill_group(window, schema, rows, &group, &mut out)?;
    }
    Ok(out)
}

fn fill_group(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
    group: &[usize],
    out: &mut [Datum],
) -> Result<()> {
    let mut ordered = group.to_vec();
    if !window.spec.order_by.is_empty() {
        ordered.sort_by(|a, b| compare_order(&rows[*a], &rows[*b], schema, &window.spec.order_by));
    }
    match window.name.to_ascii_uppercase().as_str() {
        "ROW_NUMBER" => {
            for (i, idx) in ordered.iter().enumerate() {
                out[*idx] = Datum::I64((i + 1) as i64);
            }
        }
        "RANK" => {
            let mut rank = 1i64;
            for (i, idx) in ordered.iter().enumerate() {
                if i > 0
                    && compare_order(
                        &rows[ordered[i - 1]],
                        &rows[*idx],
                        schema,
                        &window.spec.order_by,
                    ) != std::cmp::Ordering::Equal
                {
                    rank = (i + 1) as i64;
                }
                out[*idx] = Datum::I64(rank);
            }
        }
        "DENSE_RANK" => {
            let mut rank = 1i64;
            for (i, idx) in ordered.iter().enumerate() {
                if i > 0
                    && compare_order(
                        &rows[ordered[i - 1]],
                        &rows[*idx],
                        schema,
                        &window.spec.order_by,
                    ) != std::cmp::Ordering::Equal
                {
                    rank += 1;
                }
                out[*idx] = Datum::I64(rank);
            }
        }
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
            fill_window_agg(window, schema, rows, &ordered, out)?
        }
        "LAG" | "LEAD" => fill_lag_lead(window, schema, rows, &ordered, out)?,
        "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" => {
            fill_nth_value(window, schema, rows, &ordered, out)?
        }
        "NTILE" => fill_ntile(window, schema, rows, &ordered, out)?,
        other => return Err(Error::unsupported(format!("window function {other}"))),
    }
    Ok(())
}

fn fill_window_agg(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    out: &mut [Datum],
) -> Result<()> {
    let spec = FuncCallExpr {
        name: window.name.clone(),
        args: window.args.clone(),
        distinct: false,
    };
    if window.spec.frame.is_none() && window.spec.order_by.is_empty() {
        let mut agg = build_agg_func(&spec)?;
        for idx in ordered {
            update_agg(agg.as_mut(), schema, &rows[*idx], &window.args)?;
        }
        let value = agg.finalize()?;
        for idx in ordered {
            out[*idx] = value.clone();
        }
        return Ok(());
    }
    for (i, idx) in ordered.iter().enumerate() {
        match frame_span(&window.spec, schema, rows, ordered, i)? {
            None => out[*idx] = empty_frame_value(&window.name),
            Some((start, end)) => {
                let mut agg = build_agg_func(&spec)?;
                for seen in &ordered[start..=end] {
                    update_agg(agg.as_mut(), schema, &rows[*seen], &window.args)?;
                }
                out[*idx] = agg.finalize()?;
            }
        }
    }
    Ok(())
}

/// `LAG(expr[, offset[, default]])` / `LEAD(...)` relative to the ordered
/// partition. Offset defaults to 1; out-of-range rows use default or NULL.
fn fill_lag_lead(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    out: &mut [Datum],
) -> Result<()> {
    let expr = window
        .args
        .first()
        .ok_or_else(|| Error::unsupported("LAG/LEAD requires an expression"))?;
    let lead = window.name.eq_ignore_ascii_case("LEAD");
    for (i, idx) in ordered.iter().enumerate() {
        let offset = integer_arg(window, schema, &rows[*idx], 1, 1)?.max(0);
        let default = match window.args.get(2) {
            Some(arg) => eval_expr_with_row(arg, schema, &rows[*idx])?,
            None => Datum::Null,
        };
        out[*idx] = offset_value(
            expr,
            schema,
            rows,
            ordered,
            i,
            offset,
            lead,
            window.ignore_nulls,
            default,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn offset_value(
    expr: &ExprNode,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    i: usize,
    offset: i64,
    lead: bool,
    ignore_nulls: bool,
    default: Datum,
) -> Result<Datum> {
    if offset == 0 {
        return eval_expr_with_row(expr, schema, &rows[ordered[i]]);
    }
    let step: isize = if lead { 1 } else { -1 };
    let mut pos = i as isize;
    let mut seen = 0i64;
    loop {
        pos += step;
        if pos < 0 || pos >= ordered.len() as isize {
            return Ok(default);
        }
        let value = eval_expr_with_row(expr, schema, &rows[ordered[pos as usize]])?;
        if ignore_nulls && value.is_null() {
            continue;
        }
        seen += 1;
        if seen >= offset {
            return Ok(value);
        }
    }
}

/// `FIRST_VALUE` / `LAST_VALUE` / `NTH_VALUE(expr, n)` over the current frame.
fn fill_nth_value(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    out: &mut [Datum],
) -> Result<()> {
    let expr = window
        .args
        .first()
        .ok_or_else(|| Error::unsupported(format!("{} requires an expression", window.name)))?;
    for (i, idx) in ordered.iter().enumerate() {
        out[*idx] = match frame_span(&window.spec, schema, rows, ordered, i)? {
            None => Datum::Null,
            Some((start, end)) => {
                frame_nth_value(window, expr, schema, rows, ordered, *idx, start, end)?
            }
        };
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn frame_nth_value(
    window: &WindowFuncExpr,
    expr: &ExprNode,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    idx: usize,
    start: usize,
    end: usize,
) -> Result<Datum> {
    let values = (start..=end)
        .map(|pos| eval_expr_with_row(expr, schema, &rows[ordered[pos]]))
        .collect::<Result<Vec<_>>>()?;
    let values: Vec<Datum> = if window.ignore_nulls {
        values.into_iter().filter(|v| !v.is_null()).collect()
    } else {
        values
    };
    let pick = match window.name.to_ascii_uppercase().as_str() {
        "FIRST_VALUE" => values.first(),
        "LAST_VALUE" => values.last(),
        "NTH_VALUE" => {
            let n = integer_arg(window, schema, &rows[idx], 1, 1)?;
            if n <= 0 {
                return Err(Error::unsupported("NTH_VALUE requires n >= 1"));
            }
            values.get(n as usize - 1)
        }
        _ => None,
    };
    Ok(pick.cloned().unwrap_or(Datum::Null))
}

/// `NTILE(n)` splits the ordered partition into `n` buckets (larger first).
fn fill_ntile(
    window: &WindowFuncExpr,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    out: &mut [Datum],
) -> Result<()> {
    if ordered.is_empty() {
        return Ok(());
    }
    let n = integer_arg(window, schema, &rows[ordered[0]], 0, 1)?.max(1) as usize;
    let len = ordered.len();
    let small = len / n;
    let extra = len % n;
    let mut pos = 0usize;
    for bucket in 1..=n {
        let size = small + usize::from(bucket <= extra);
        for _ in 0..size {
            if pos >= len {
                break;
            }
            out[ordered[pos]] = Datum::I64(bucket as i64);
            pos += 1;
        }
    }
    Ok(())
}

fn integer_arg(
    window: &WindowFuncExpr,
    schema: &[String],
    row: &[Datum],
    index: usize,
    default: i64,
) -> Result<i64> {
    let Some(arg) = window.args.get(index) else {
        return Ok(default);
    };
    match eval_expr_with_row(arg, schema, row)? {
        Datum::I64(n) => Ok(n),
        Datum::U64(n) => Ok(n as i64),
        Datum::Null => Ok(default),
        other => Err(Error::unsupported(format!(
            "window integer argument {other:?}"
        ))),
    }
}

fn empty_frame_value(name: &str) -> Datum {
    if name.eq_ignore_ascii_case("COUNT") {
        Datum::I64(0)
    } else {
        Datum::Null
    }
}

/// Inclusive `[start, end]` into `ordered`, or `None` when the frame is empty.
fn frame_span(
    spec: &WindowSpec,
    schema: &[String],
    rows: &[Vec<Datum>],
    ordered: &[usize],
    i: usize,
) -> Result<Option<(usize, usize)>> {
    let len = ordered.len();
    if len == 0 {
        return Ok(None);
    }
    let (units, start_bound, end_bound) = match &spec.frame {
        None => {
            return Ok(Some((0, i)));
        }
        Some(WindowFrameSpec { units, start, end }) => (*units, start, end),
    };
    let start = bound_index(
        units,
        start_bound,
        true,
        i,
        ordered,
        rows,
        schema,
        &spec.order_by,
    )?;
    let end = bound_index(
        units,
        end_bound,
        false,
        i,
        ordered,
        rows,
        schema,
        &spec.order_by,
    )?;
    if start > end {
        Ok(None)
    } else {
        Ok(Some((start, end)))
    }
}

#[allow(clippy::too_many_arguments)]
fn bound_index(
    units: WindowFrameUnits,
    bound: &WindowFrameBound,
    is_start: bool,
    i: usize,
    ordered: &[usize],
    rows: &[Vec<Datum>],
    schema: &[String],
    order_by: &[sealdb_parser::ast::ByItem],
) -> Result<usize> {
    let last = ordered.len().saturating_sub(1);
    match (units, bound) {
        (_, WindowFrameBound::UnboundedPreceding) => Ok(0),
        (_, WindowFrameBound::UnboundedFollowing) => Ok(last),
        (WindowFrameUnits::Rows, WindowFrameBound::CurrentRow) => Ok(i),
        (WindowFrameUnits::Rows, WindowFrameBound::Preceding(n)) => {
            Ok(i.saturating_sub(*n as usize))
        }
        (WindowFrameUnits::Rows, WindowFrameBound::Following(n)) => {
            Ok(i.saturating_add(*n as usize).min(last))
        }
        (WindowFrameUnits::Range | WindowFrameUnits::Groups, WindowFrameBound::CurrentRow) => {
            Ok(if is_start {
                peer_start(ordered, rows, schema, order_by, i)
            } else {
                peer_end(ordered, rows, schema, order_by, i)
            })
        }
        (WindowFrameUnits::Groups, WindowFrameBound::Preceding(n)) => Ok(group_shift(
            ordered,
            rows,
            schema,
            order_by,
            i,
            -(*n as i64),
            is_start,
        )),
        (WindowFrameUnits::Groups, WindowFrameBound::Following(n)) => Ok(group_shift(
            ordered, rows, schema, order_by, i, *n as i64, is_start,
        )),
        // Numeric / INTERVAL RANGE offsets: approximate with peer group bounds
        // until value-based RANGE is fully ported (EXPLAIN + empty-table Gate B).
        (
            WindowFrameUnits::Range,
            WindowFrameBound::Preceding(_)
            | WindowFrameBound::Following(_)
            | WindowFrameBound::IntervalPreceding { .. }
            | WindowFrameBound::IntervalFollowing { .. },
        ) => Ok(if is_start {
            peer_start(ordered, rows, schema, order_by, i)
        } else {
            peer_end(ordered, rows, schema, order_by, i)
        }),
        (
            WindowFrameUnits::Groups,
            WindowFrameBound::IntervalPreceding { .. }
            | WindowFrameBound::IntervalFollowing { .. },
        ) => Err(Error::unsupported("GROUPS interval frame")),
        (
            WindowFrameUnits::Rows,
            WindowFrameBound::IntervalPreceding { .. }
            | WindowFrameBound::IntervalFollowing { .. },
        ) => Err(Error::unsupported("ROWS interval frame")),
    }
}

fn peer_start(
    ordered: &[usize],
    rows: &[Vec<Datum>],
    schema: &[String],
    order_by: &[sealdb_parser::ast::ByItem],
    i: usize,
) -> usize {
    let mut s = i;
    while s > 0
        && compare_order(&rows[ordered[s - 1]], &rows[ordered[i]], schema, order_by)
            == std::cmp::Ordering::Equal
    {
        s -= 1;
    }
    s
}

fn peer_end(
    ordered: &[usize],
    rows: &[Vec<Datum>],
    schema: &[String],
    order_by: &[sealdb_parser::ast::ByItem],
    i: usize,
) -> usize {
    let mut e = i;
    while e + 1 < ordered.len()
        && compare_order(&rows[ordered[e + 1]], &rows[ordered[i]], schema, order_by)
            == std::cmp::Ordering::Equal
    {
        e += 1;
    }
    e
}

fn group_shift(
    ordered: &[usize],
    rows: &[Vec<Datum>],
    schema: &[String],
    order_by: &[sealdb_parser::ast::ByItem],
    i: usize,
    delta: i64,
    is_start: bool,
) -> usize {
    let last = ordered.len().saturating_sub(1);
    let mut pos = if is_start {
        peer_start(ordered, rows, schema, order_by, i)
    } else {
        peer_end(ordered, rows, schema, order_by, i)
    };
    let mut remaining = delta.unsigned_abs() as usize;
    if delta <= 0 {
        while remaining > 0 && pos > 0 {
            pos = peer_start(ordered, rows, schema, order_by, pos - 1);
            remaining -= 1;
        }
        if is_start {
            pos
        } else {
            peer_end(ordered, rows, schema, order_by, pos)
        }
    } else {
        while remaining > 0 && pos < last {
            pos = peer_end(ordered, rows, schema, order_by, pos + 1);
            remaining -= 1;
        }
        if is_start {
            peer_start(ordered, rows, schema, order_by, pos)
        } else {
            pos
        }
    }
}

fn update_agg(
    agg: &mut dyn AggFunc,
    schema: &[String],
    row: &[Datum],
    args: &[ExprNode],
) -> Result<()> {
    if args.is_empty() {
        return agg.update(None);
    }
    let value = eval_expr_with_row(&args[0], schema, row)?;
    agg.update(Some(&value))
}

fn compare_order(
    left: &[Datum],
    right: &[Datum],
    schema: &[String],
    by: &[sealdb_parser::ast::ByItem],
) -> std::cmp::Ordering {
    for item in by {
        let lv = eval_expr_with_row(&item.expr, schema, left).unwrap_or(Datum::Null);
        let rv = eval_expr_with_row(&item.expr, schema, right).unwrap_or(Datum::Null);
        let ord = lv.compare(&rv);
        if ord != std::cmp::Ordering::Equal {
            return if item.desc { ord.reverse() } else { ord };
        }
    }
    std::cmp::Ordering::Equal
}

fn field_schema_name(field: &sealdb_parser::ast::SelectField) -> String {
    if let Some(name) = &field.as_name
        && !name.is_empty()
    {
        return name.clone();
    }
    match &field.expr {
        ExprNode::Column(col) => col.name.clone(),
        _ => restore_expr(&field.expr),
    }
}

fn restore_expr(expr: &ExprNode) -> String {
    let mut ctx = RestoreCtx::new();
    let _ = expr.restore(&mut ctx);
    ctx.into_string()
}

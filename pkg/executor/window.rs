// F16 core path: WindowExec for ROW_NUMBER/RANK/DENSE_RANK and
// COUNT/SUM/AVG/MIN/MAX OVER (PARTITION BY ... ORDER BY ...).
// Completeness wave / not this feature: frame clauses, named windows,
// LAG/LEAD/FIRST_VALUE/NTILE, IGNORE NULLS, pipelined window.
use crate::aggfuncs::{AggFunc, build_agg_func};
use crate::internal::exec::Executor;
use sealdb_errno::{Error, Result};
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{ExprNode, FuncCallExpr, WindowFuncExpr};
use sealdb_parser::format::RestoreCtx;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub struct WindowExec {
    child: Box<dyn Executor>,
    select: SelectStmt,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl WindowExec {
    pub fn new(select: SelectStmt, child: Box<dyn Executor>) -> Self {
        let schema = select.fields.iter().map(field_schema_name).collect();
        Self {
            child,
            select,
            schema,
            rows: Vec::new(),
            emitted: false,
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
        let mut computed: Vec<Option<Vec<Datum>>> = vec![None; self.select.fields.len()];
        for (i, field) in self.select.fields.iter().enumerate() {
            if let ExprNode::Window(window) = &field.expr {
                computed[i] = Some(eval_window(window, &input_schema, &input)?);
            }
        }
        for (idx, row) in input.iter().enumerate() {
            let mut out = Vec::with_capacity(self.select.fields.len());
            for (i, field) in self.select.fields.iter().enumerate() {
                if let Some(values) = &computed[i] {
                    out.push(values[idx].clone());
                } else {
                    out.push(eval_expr_with_row(&field.expr, &input_schema, row)?);
                }
            }
            self.rows.push(out);
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
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
    if window.spec.order_by.is_empty() {
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
        let mut agg = build_agg_func(&spec)?;
        for seen in &ordered[..=i] {
            update_agg(agg.as_mut(), schema, &rows[*seen], &window.args)?;
        }
        out[*idx] = agg.finalize()?;
    }
    Ok(())
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

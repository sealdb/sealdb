use super::dml::{ByItem, SelectStmt};
use crate::format::RestoreCtx;
use crate::opcode::Op;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Clone, Debug)]
pub enum ExprNode {
    Value(ValueExpr),
    Column(ColumnNameExpr),
    Binary(BinaryOpExpr),
    Unary(UnaryOpExpr),
    Func(FuncCallExpr),
    Cast(FuncCastExpr),
    Exists(ExistsExpr),
    InSubquery(InSubqueryExpr),
    InList(InListExpr),
    Between(BetweenExpr),
    Like(PatternLikeExpr),
    IsNull(IsNullExpr),
    ScalarSubquery(ScalarSubqueryExpr),
    Window(WindowFuncExpr),
}

impl ExprNode {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            ExprNode::Value(v) => v.restore(ctx),
            ExprNode::Column(v) => v.restore(ctx),
            ExprNode::Binary(v) => v.restore(ctx),
            ExprNode::Unary(v) => v.restore(ctx),
            ExprNode::Func(v) => v.restore(ctx),
            ExprNode::Cast(v) => v.restore(ctx),
            ExprNode::Exists(v) => v.restore(ctx),
            ExprNode::InSubquery(v) => v.restore(ctx),
            ExprNode::InList(v) => v.restore(ctx),
            ExprNode::Between(v) => v.restore(ctx),
            ExprNode::Like(v) => v.restore(ctx),
            ExprNode::IsNull(v) => v.restore(ctx),
            ExprNode::ScalarSubquery(v) => v.restore(ctx),
            ExprNode::Window(v) => v.restore(ctx),
        }
    }

    pub fn has_subquery(&self) -> bool {
        match self {
            ExprNode::Exists(_) | ExprNode::InSubquery(_) | ExprNode::ScalarSubquery(_) => true,
            ExprNode::Binary(b) => b.left.has_subquery() || b.right.has_subquery(),
            ExprNode::Unary(u) => u.expr.has_subquery(),
            ExprNode::Func(f) => f.args.iter().any(Self::has_subquery),
            ExprNode::Cast(c) => c.expr.has_subquery(),
            ExprNode::InList(i) => i.expr.has_subquery() || i.list.iter().any(Self::has_subquery),
            ExprNode::Between(b) => {
                b.expr.has_subquery() || b.low.has_subquery() || b.high.has_subquery()
            }
            ExprNode::Like(l) => l.expr.has_subquery() || l.pattern.has_subquery(),
            ExprNode::IsNull(i) => i.expr.has_subquery(),
            ExprNode::Window(w) => {
                w.args.iter().any(Self::has_subquery)
                    || w.spec.partition_by.iter().any(Self::has_subquery)
                    || w.spec.order_by.iter().any(|item| item.expr.has_subquery())
            }
            ExprNode::Value(_) | ExprNode::Column(_) => false,
        }
    }

    pub fn restore_string(&self) -> String {
        let mut ctx = RestoreCtx::new();
        let _ = self.restore(&mut ctx);
        ctx.into_string()
    }

    pub fn split_cnf(&self) -> Vec<ExprNode> {
        match self {
            ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
                let mut out = bin.left.split_cnf();
                out.extend(bin.right.split_cnf());
                out
            }
            other => vec![other.clone()],
        }
    }

    pub fn compose_cnf(exprs: Vec<ExprNode>) -> Option<ExprNode> {
        exprs.into_iter().reduce(|left, right| {
            ExprNode::Binary(BinaryOpExpr {
                op: Op::LogicAnd,
                left: Box::new(left),
                right: Box::new(right),
            })
        })
    }

    pub fn referenced_columns(&self) -> Vec<(Option<String>, String)> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    pub fn eq_column_const(&self) -> Option<(String, Datum)> {
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        if bin.op != Op::Eq {
            return None;
        }
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(col), ExprNode::Value(v))
            | (ExprNode::Value(v), ExprNode::Column(col)) => {
                Some((col.name.clone(), v.datum.clone()))
            }
            _ => None,
        }
    }

    pub fn eq_const_on(&self, column: &str) -> Option<Datum> {
        self.split_cnf().into_iter().find_map(|expr| {
            expr.eq_column_const()
                .and_then(|(name, value)| name.eq_ignore_ascii_case(column).then_some(value))
        })
    }

    pub fn column_cmp_const(&self) -> Option<(String, Op, Datum)> {
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(col), ExprNode::Value(v))
                if matches!(bin.op, Op::Lt | Op::Le | Op::Gt | Op::Ge) =>
            {
                Some((col.name.clone(), bin.op, v.datum.clone()))
            }
            (ExprNode::Value(v), ExprNode::Column(col)) => {
                flip_cmp(bin.op).map(|op| (col.name.clone(), op, v.datum.clone()))
            }
            _ => None,
        }
    }

    pub fn between_column_const(&self) -> Option<(String, Datum, Datum)> {
        let ExprNode::Between(b) = self else {
            return None;
        };
        if b.not {
            return None;
        }
        let ExprNode::Column(col) = b.expr.as_ref() else {
            return None;
        };
        let (ExprNode::Value(low), ExprNode::Value(high)) = (b.low.as_ref(), b.high.as_ref())
        else {
            return None;
        };
        Some((col.name.clone(), low.datum.clone(), high.datum.clone()))
    }

    pub fn column_ranges(&self) -> Vec<ColumnRange> {
        let mut ranges = Vec::new();
        for item in self.split_cnf() {
            if let Some((name, op, value)) = item.column_cmp_const() {
                apply_cmp_range(&mut ranges, name, op, value);
            } else if let Some((name, low, high)) = item.between_column_const() {
                apply_cmp_range(&mut ranges, name.clone(), Op::Ge, low);
                apply_cmp_range(&mut ranges, name, Op::Le, high);
            }
        }
        ranges
    }

    fn collect_columns(&self, out: &mut Vec<(Option<String>, String)>) {
        match self {
            ExprNode::Column(col) => out.push((col.table.clone(), col.name.clone())),
            ExprNode::Binary(b) => {
                b.left.collect_columns(out);
                b.right.collect_columns(out);
            }
            ExprNode::Unary(u) => u.expr.collect_columns(out),
            ExprNode::Func(f) => {
                for arg in &f.args {
                    arg.collect_columns(out);
                }
            }
            ExprNode::Cast(c) => c.expr.collect_columns(out),
            ExprNode::InList(i) => {
                i.expr.collect_columns(out);
                for item in &i.list {
                    item.collect_columns(out);
                }
            }
            ExprNode::Between(b) => {
                b.expr.collect_columns(out);
                b.low.collect_columns(out);
                b.high.collect_columns(out);
            }
            ExprNode::Like(l) => {
                l.expr.collect_columns(out);
                l.pattern.collect_columns(out);
            }
            ExprNode::IsNull(i) => i.expr.collect_columns(out),
            ExprNode::Window(w) => {
                for arg in &w.args {
                    arg.collect_columns(out);
                }
                for part in &w.spec.partition_by {
                    part.collect_columns(out);
                }
                for item in &w.spec.order_by {
                    item.expr.collect_columns(out);
                }
            }
            ExprNode::Exists(e) => collect_select_columns(&e.query, out),
            ExprNode::InSubquery(i) => {
                i.expr.collect_columns(out);
                collect_select_columns(&i.query, out);
            }
            ExprNode::ScalarSubquery(s) => collect_select_columns(&s.query, out),
            ExprNode::Value(_) => {}
        }
    }
}

fn collect_select_columns(query: &SelectStmt, out: &mut Vec<(Option<String>, String)>) {
    if let Some(selection) = &query.selection {
        selection.collect_columns(out);
    }
}

#[derive(Clone, Debug)]
pub struct ColumnNameExpr {
    pub schema: Option<String>,
    pub table: Option<String>,
    pub name: String,
}

impl ColumnNameExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(schema) = &self.schema {
            ctx.write_name(schema);
            ctx.write_str(".");
        }
        if let Some(table) = &self.table {
            ctx.write_name(table);
            ctx.write_str(".");
        }
        ctx.write_name(&self.name);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ValueExpr {
    pub datum: Datum,
}

impl ValueExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&match &self.datum {
            Datum::Null => "NULL".to_string(),
            Datum::I64(v) => v.to_string(),
            Datum::U64(v) => v.to_string(),
            Datum::F64(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    format!("{v}")
                } else {
                    v.to_string()
                }
            }
            Datum::Bytes(b) => format!("'{}'", String::from_utf8_lossy(b).replace('\'', "''")),
            Datum::Decimal(d) => d.to_string(),
            Datum::Time(t) => format!("'{t}'"),
        });
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct BinaryOpExpr {
    pub op: Op,
    pub left: Box<ExprNode>,
    pub right: Box<ExprNode>,
}

impl BinaryOpExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.left.restore(ctx)?;
        ctx.write_str(" ");
        ctx.write_str(self.op.restore());
        ctx.write_str(" ");
        self.right.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct UnaryOpExpr {
    pub op: Op,
    pub expr: Box<ExprNode>,
}

impl UnaryOpExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(self.op.restore());
        if matches!(self.op, Op::Not) {
            ctx.write_str(" ");
        }
        self.expr.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct FuncCallExpr {
    pub name: String,
    pub args: Vec<ExprNode>,
    pub distinct: bool,
}

impl FuncCallExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&self.name);
        ctx.write_str("(");
        if self.distinct {
            ctx.write_str("DISTINCT ");
        }
        if self.name.eq_ignore_ascii_case("COUNT") && self.args.is_empty() {
            ctx.write_str("*");
        } else {
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                arg.restore(ctx)?;
            }
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FuncCastExpr {
    pub expr: Box<ExprNode>,
    pub target: String,
}

impl FuncCastExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CAST(");
        self.expr.restore(ctx)?;
        ctx.write_str(" AS ");
        ctx.write_str(&self.target);
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ExistsExpr {
    pub not: bool,
    pub query: Box<SelectStmt>,
}

impl ExistsExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if self.not {
            ctx.write_str("NOT ");
        }
        ctx.write_str("EXISTS (");
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InSubqueryExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub query: Box<SelectStmt>,
}

impl InSubqueryExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT IN (");
        } else {
            ctx.write_str(" IN (");
        }
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InListExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub list: Vec<ExprNode>,
}

impl InListExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT IN (");
        } else {
            ctx.write_str(" IN (");
        }
        for (i, item) in self.list.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            item.restore(ctx)?;
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ColumnRange {
    pub column: String,
    pub low: Option<(Datum, bool)>,
    pub high: Option<(Datum, bool)>,
}

fn flip_cmp(op: Op) -> Option<Op> {
    match op {
        Op::Lt => Some(Op::Gt),
        Op::Le => Some(Op::Ge),
        Op::Gt => Some(Op::Lt),
        Op::Ge => Some(Op::Le),
        _ => None,
    }
}

fn apply_cmp_range(ranges: &mut Vec<ColumnRange>, name: String, op: Op, value: Datum) {
    let slot = match ranges
        .iter()
        .position(|range| range.column.eq_ignore_ascii_case(&name))
    {
        Some(idx) => idx,
        None => {
            ranges.push(ColumnRange {
                column: name,
                low: None,
                high: None,
            });
            ranges.len() - 1
        }
    };
    match op {
        Op::Gt => tighten_low(&mut ranges[slot].low, value, false),
        Op::Ge => tighten_low(&mut ranges[slot].low, value, true),
        Op::Lt => tighten_high(&mut ranges[slot].high, value, false),
        Op::Le => tighten_high(&mut ranges[slot].high, value, true),
        _ => {}
    }
}

fn tighten_low(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool) {
    *slot = Some(match slot.take() {
        None => (value, inclusive),
        Some((existing, existing_incl)) => match value.compare(&existing) {
            std::cmp::Ordering::Greater => (value, inclusive),
            std::cmp::Ordering::Less => (existing, existing_incl),
            std::cmp::Ordering::Equal => (existing, existing_incl && inclusive),
        },
    });
}

fn tighten_high(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool) {
    *slot = Some(match slot.take() {
        None => (value, inclusive),
        Some((existing, existing_incl)) => match value.compare(&existing) {
            std::cmp::Ordering::Less => (value, inclusive),
            std::cmp::Ordering::Greater => (existing, existing_incl),
            std::cmp::Ordering::Equal => (existing, existing_incl && inclusive),
        },
    });
}

#[derive(Clone, Debug)]
pub struct BetweenExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub low: Box<ExprNode>,
    pub high: Box<ExprNode>,
}

impl BetweenExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT BETWEEN ");
        } else {
            ctx.write_str(" BETWEEN ");
        }
        self.low.restore(ctx)?;
        ctx.write_str(" AND ");
        self.high.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct PatternLikeExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub pattern: Box<ExprNode>,
    pub ci: bool,
}

impl PatternLikeExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT LIKE ");
        } else {
            ctx.write_str(" LIKE ");
        }
        self.pattern.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct IsNullExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
}

impl IsNullExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" IS NOT NULL");
        } else {
            ctx.write_str(" IS NULL");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ScalarSubqueryExpr {
    pub query: Box<SelectStmt>,
}

impl ScalarSubqueryExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("(");
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct WindowSpec {
    pub partition_by: Vec<ExprNode>,
    pub order_by: Vec<ByItem>,
}

impl WindowSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        let mut first = true;
        if !self.partition_by.is_empty() {
            ctx.write_str("PARTITION BY ");
            for (i, expr) in self.partition_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                expr.restore(ctx)?;
            }
            first = false;
        }
        if !self.order_by.is_empty() {
            if !first {
                ctx.write_str(" ");
            }
            ctx.write_str("ORDER BY ");
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                item.expr.restore(ctx)?;
                if item.desc {
                    ctx.write_str(" DESC");
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct WindowFuncExpr {
    pub name: String,
    pub args: Vec<ExprNode>,
    pub spec: WindowSpec,
}

impl WindowFuncExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&self.name);
        ctx.write_str("(");
        if self.name.eq_ignore_ascii_case("COUNT") && self.args.is_empty() {
            ctx.write_str("*");
        } else {
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                arg.restore(ctx)?;
            }
        }
        ctx.write_str(") OVER (");
        self.spec.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

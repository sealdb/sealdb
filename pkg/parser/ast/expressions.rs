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
            ExprNode::Window(w) => {
                w.args.iter().any(Self::has_subquery)
                    || w.spec.partition_by.iter().any(Self::has_subquery)
                    || w.spec.order_by.iter().any(|item| item.expr.has_subquery())
            }
            ExprNode::Value(_) | ExprNode::Column(_) => false,
        }
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
}

impl FuncCallExpr {
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

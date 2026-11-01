use crate::format::RestoreCtx;
use crate::opcode::Op;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Clone, Debug)]
pub enum ExprNode {
    Value(ValueExpr),
    Binary(BinaryOpExpr),
    Unary(UnaryOpExpr),
    Func(FuncCallExpr),
    Cast(FuncCastExpr),
}

impl ExprNode {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            ExprNode::Value(v) => v.restore(ctx),
            ExprNode::Binary(v) => v.restore(ctx),
            ExprNode::Unary(v) => v.restore(ctx),
            ExprNode::Func(v) => v.restore(ctx),
            ExprNode::Cast(v) => v.restore(ctx),
        }
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
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            arg.restore(ctx)?;
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

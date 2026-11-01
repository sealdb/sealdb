use super::expressions::ExprNode;
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Debug)]
pub struct SelectField {
    pub expr: ExprNode,
    pub as_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SelectStmt {
    pub fields: Vec<SelectField>,
    pub from: Option<String>,
    pub text: String,
}

impl SelectStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SELECT ");
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            field.expr.restore(ctx)?;
            if let Some(name) = &field.as_name {
                ctx.write_str(" AS ");
                ctx.write_str(name);
            }
        }
        if let Some(from) = &self.from {
            ctx.write_str(" FROM ");
            ctx.write_str(from);
        }
        Ok(())
    }
}

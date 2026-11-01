use super::ddl::TableName;
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
    pub from: Option<TableName>,
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
            from.restore(ctx)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InsertStmt {
    pub table: TableName,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ExprNode>>,
}

impl InsertStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("INSERT INTO ");
        self.table.restore(ctx)?;
        if !self.columns.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.columns.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                ctx.write_name(col);
            }
            ctx.write_str(")");
        }
        ctx.write_str(" VALUES ");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            ctx.write_str("(");
            for (j, expr) in row.iter().enumerate() {
                if j > 0 {
                    ctx.write_str(",");
                }
                expr.restore(ctx)?;
            }
            ctx.write_str(")");
        }
        Ok(())
    }
}

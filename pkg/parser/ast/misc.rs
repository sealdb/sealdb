use super::ddl::TableName;
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowStmtKind {
    Databases,
    Tables,
    Columns,
}

#[derive(Clone, Debug)]
pub struct ShowStmt {
    pub kind: ShowStmtKind,
    pub db_name: Option<String>,
    pub table: Option<TableName>,
}

impl ShowStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self.kind {
            ShowStmtKind::Databases => ctx.write_str("SHOW DATABASES"),
            ShowStmtKind::Tables => {
                ctx.write_str("SHOW TABLES");
                if let Some(db) = &self.db_name {
                    ctx.write_str(" FROM ");
                    ctx.write_name(db);
                }
            }
            ShowStmtKind::Columns => {
                ctx.write_str("DESC ");
                if let Some(table) = &self.table {
                    table.restore(ctx)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct UseStmt {
    pub db_name: String,
}

impl UseStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("USE ");
        ctx.write_name(&self.db_name);
        Ok(())
    }
}

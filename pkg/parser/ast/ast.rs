use super::ddl::{CreateDatabaseStmt, CreateTableStmt, DropDatabaseStmt, DropTableStmt};
use super::{dml::SelectStmt, expressions::ExprNode};
use crate::format::RestoreCtx;
use sealdb_errno::Result;

pub trait Node {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()>;
}

pub trait StmtNode: Node {
    fn stmt_type(&self) -> &'static str;
}

#[derive(Clone, Debug)]
pub enum Statement {
    Select(SelectStmt),
    CreateDatabase(CreateDatabaseStmt),
    DropDatabase(DropDatabaseStmt),
    CreateTable(CreateTableStmt),
    DropTable(DropTableStmt),
}

impl Node for Statement {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            Statement::Select(s) => s.restore(ctx),
            Statement::CreateDatabase(s) => s.restore(ctx),
            Statement::DropDatabase(s) => s.restore(ctx),
            Statement::CreateTable(s) => s.restore(ctx),
            Statement::DropTable(s) => s.restore(ctx),
        }
    }
}

impl StmtNode for Statement {
    fn stmt_type(&self) -> &'static str {
        match self {
            Statement::Select(_) => "SelectStmt",
            Statement::CreateDatabase(_) => "CreateDatabaseStmt",
            Statement::DropDatabase(_) => "DropDatabaseStmt",
            Statement::CreateTable(_) => "CreateTableStmt",
            Statement::DropTable(_) => "DropTableStmt",
        }
    }
}

impl Statement {
    pub fn as_select(&self) -> Option<&SelectStmt> {
        match self {
            Statement::Select(s) => Some(s),
            _ => None,
        }
    }
}

pub fn restore_expr(expr: &ExprNode, ctx: &mut RestoreCtx) -> Result<()> {
    expr.restore(ctx)
}

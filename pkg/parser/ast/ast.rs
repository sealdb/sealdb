use super::ddl::{CreateDatabaseStmt, CreateTableStmt, DropDatabaseStmt, DropTableStmt};
use super::dml::{DeleteStmt, InsertStmt, SelectStmt, UpdateStmt};
use super::expressions::ExprNode;
use super::misc::{ShowStmt, UseStmt};
use crate::format::RestoreCtx;
use sealdb_errno::Result;

pub trait Node {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()>;
}

pub trait StmtNode: Node {
    fn stmt_type(&self) -> &'static str;
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Statement {
    Select(SelectStmt),
    CreateDatabase(CreateDatabaseStmt),
    DropDatabase(DropDatabaseStmt),
    CreateTable(CreateTableStmt),
    DropTable(DropTableStmt),
    Show(ShowStmt),
    Use(UseStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
}

impl Node for Statement {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            Statement::Select(s) => s.restore(ctx),
            Statement::CreateDatabase(s) => s.restore(ctx),
            Statement::DropDatabase(s) => s.restore(ctx),
            Statement::CreateTable(s) => s.restore(ctx),
            Statement::DropTable(s) => s.restore(ctx),
            Statement::Show(s) => s.restore(ctx),
            Statement::Use(s) => s.restore(ctx),
            Statement::Insert(s) => s.restore(ctx),
            Statement::Update(s) => s.restore(ctx),
            Statement::Delete(s) => s.restore(ctx),
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
            Statement::Show(_) => "ShowStmt",
            Statement::Use(_) => "UseStmt",
            Statement::Insert(_) => "InsertStmt",
            Statement::Update(_) => "UpdateStmt",
            Statement::Delete(_) => "DeleteStmt",
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

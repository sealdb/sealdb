use super::ddl::{CreateDatabaseStmt, CreateTableStmt, DropDatabaseStmt, DropTableStmt};
use super::dml::{DeleteStmt, InsertStmt, SelectStmt, SetOprStmt, UpdateStmt};
use super::expressions::ExprNode;
use super::misc::{BeginStmt, CommitStmt, RollbackStmt, ShowStmt, UseStmt};
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
    Begin(BeginStmt),
    Commit(CommitStmt),
    Rollback(RollbackStmt),
    SetOpr(SetOprStmt),
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
            Statement::Begin(s) => s.restore(ctx),
            Statement::Commit(s) => s.restore(ctx),
            Statement::Rollback(s) => s.restore(ctx),
            Statement::SetOpr(s) => s.restore(ctx),
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
            Statement::Begin(_) => "BeginStmt",
            Statement::Commit(_) => "CommitStmt",
            Statement::Rollback(_) => "RollbackStmt",
            Statement::SetOpr(_) => "SetOprStmt",
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

    pub fn with_clause(&self) -> Option<&super::dml::WithClause> {
        match self {
            Statement::Select(s) => s.with.as_ref(),
            Statement::SetOpr(s) => s.with.as_ref(),
            _ => None,
        }
    }
}

pub fn restore_expr(expr: &ExprNode, ctx: &mut RestoreCtx) -> Result<()> {
    expr.restore(ctx)
}

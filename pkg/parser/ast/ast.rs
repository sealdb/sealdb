use super::ddl::{
    AlterTableStmt, CreateDatabaseStmt, CreateIndexStmt, CreateTableStmt, DropDatabaseStmt,
    DropIndexStmt, DropTableStmt,
};
use super::dml::{DeleteStmt, InsertStmt, SelectStmt, SetOprStmt, UpdateStmt};
use super::expressions::ExprNode;
use super::misc::{
    AnalyzeTableStmt, BeginStmt, CommitStmt, GrantStmt, RevokeStmt, RollbackStmt, SetTxnStmt,
    ShowStmt, UseStmt,
};
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
    CreateIndex(CreateIndexStmt),
    DropIndex(DropIndexStmt),
    AlterTable(AlterTableStmt),
    Show(ShowStmt),
    Use(UseStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Begin(BeginStmt),
    Commit(CommitStmt),
    Rollback(RollbackStmt),
    SetOpr(SetOprStmt),
    Grant(GrantStmt),
    Revoke(RevokeStmt),
    AnalyzeTable(AnalyzeTableStmt),
    Explain(ExplainStmt),
    SetTxn(SetTxnStmt),
}

impl Node for Statement {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            Statement::Select(s) => s.restore(ctx),
            Statement::CreateDatabase(s) => s.restore(ctx),
            Statement::DropDatabase(s) => s.restore(ctx),
            Statement::CreateTable(s) => s.restore(ctx),
            Statement::DropTable(s) => s.restore(ctx),
            Statement::CreateIndex(s) => s.restore(ctx),
            Statement::DropIndex(s) => s.restore(ctx),
            Statement::AlterTable(s) => s.restore(ctx),
            Statement::Show(s) => s.restore(ctx),
            Statement::Use(s) => s.restore(ctx),
            Statement::Insert(s) => s.restore(ctx),
            Statement::Update(s) => s.restore(ctx),
            Statement::Delete(s) => s.restore(ctx),
            Statement::Begin(s) => s.restore(ctx),
            Statement::Commit(s) => s.restore(ctx),
            Statement::Rollback(s) => s.restore(ctx),
            Statement::SetOpr(s) => s.restore(ctx),
            Statement::Grant(s) => s.restore(ctx),
            Statement::Revoke(s) => s.restore(ctx),
            Statement::AnalyzeTable(s) => s.restore(ctx),
            Statement::Explain(s) => s.restore(ctx),
            Statement::SetTxn(s) => s.restore(ctx),
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
            Statement::CreateIndex(_) => "CreateIndexStmt",
            Statement::DropIndex(_) => "DropIndexStmt",
            Statement::AlterTable(_) => "AlterTableStmt",
            Statement::Show(_) => "ShowStmt",
            Statement::Use(_) => "UseStmt",
            Statement::Insert(_) => "InsertStmt",
            Statement::Update(_) => "UpdateStmt",
            Statement::Delete(_) => "DeleteStmt",
            Statement::Begin(_) => "BeginStmt",
            Statement::Commit(_) => "CommitStmt",
            Statement::Rollback(_) => "RollbackStmt",
            Statement::SetOpr(_) => "SetOprStmt",
            Statement::Grant(_) => "GrantStmt",
            Statement::Revoke(_) => "RevokeStmt",
            Statement::AnalyzeTable(_) => "AnalyzeTableStmt",
            Statement::Explain(_) => "ExplainStmt",
            Statement::SetTxn(_) => "SetTxnStmt",
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
            Statement::Explain(s) => s.stmt.with_clause(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub enum ExplainFormat {
    #[default]
    Row,
    Dot,
    Json,
}

#[derive(Clone, Debug)]
pub struct ExplainStmt {
    pub analyze: bool,
    pub format: ExplainFormat,
    pub stmt: Box<Statement>,
}

impl ExplainStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("EXPLAIN ");
        if self.analyze {
            ctx.write_str("ANALYZE ");
        }
        match self.format {
            ExplainFormat::Row => {}
            ExplainFormat::Dot => ctx.write_str("FORMAT = 'dot' "),
            ExplainFormat::Json => ctx.write_str("FORMAT = 'json' "),
        }
        self.stmt.restore(ctx)
    }
}

pub fn restore_expr(expr: &ExprNode, ctx: &mut RestoreCtx) -> Result<()> {
    expr.restore(ctx)
}

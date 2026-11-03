use super::ddl::{
    AlterDatabaseStmt, AlterSequenceStmt, AlterTableStmt, CreateDatabaseStmt, CreateIndexStmt,
    CreateSequenceStmt, CreateTableStmt, CreateViewStmt, DropDatabaseStmt, DropIndexStmt,
    DropSequenceStmt, DropTableStmt, TruncateTableStmt,
};
use super::dml::{DeleteStmt, InsertStmt, SelectStmt, SetOprStmt, UpdateStmt};
use super::expressions::ExprNode;
use super::misc::{
    AlterInstanceStmt, AnalyzeTableStmt, BeginStmt, BinlogStmt, ChangeStmt, CleanupTableLockStmt,
    CommitStmt, CreateBindingStmt, CreateRoleStmt, CreateUserStmt, DeallocateStmt, DoStmt,
    DropBindingStmt, DropRoleStmt, DropUserStmt, ExecuteStmt, FlushStmt, GrantRoleStmt, GrantStmt,
    KillStmt, LockTablesStmt, OptimizeTableStmt, PrepareStmt, ReleaseSavepointStmt, RenameUserStmt,
    RepairTableStmt, RevokeRoleStmt, RevokeStmt, RollbackStmt, SavepointStmt, SetPwdStmt,
    SetRoleStmt, SetStmt, SetTxnStmt, ShowStmt, ShutdownStmt, UseStmt,
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
    AlterDatabase(AlterDatabaseStmt),
    CreateTable(CreateTableStmt),
    CreateView(CreateViewStmt),
    CreateSequence(CreateSequenceStmt),
    DropSequence(DropSequenceStmt),
    AlterSequence(AlterSequenceStmt),
    DropTable(DropTableStmt),
    TruncateTable(TruncateTableStmt),
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
    Savepoint(SavepointStmt),
    ReleaseSavepoint(ReleaseSavepointStmt),
    SetOpr(SetOprStmt),
    Grant(GrantStmt),
    Revoke(RevokeStmt),
    CreateRole(CreateRoleStmt),
    DropRole(DropRoleStmt),
    CreateUser(CreateUserStmt),
    DropUser(DropUserStmt),
    GrantRole(GrantRoleStmt),
    RevokeRole(RevokeRoleStmt),
    SetRole(SetRoleStmt),
    SetPwd(SetPwdStmt),
    Flush(FlushStmt),
    Do(DoStmt),
    Shutdown(ShutdownStmt),
    Kill(KillStmt),
    LockTables(LockTablesStmt),
    CleanupTableLock(CleanupTableLockStmt),
    OptimizeTable(OptimizeTableStmt),
    RepairTable(RepairTableStmt),
    RenameUser(RenameUserStmt),
    Binlog(BinlogStmt),
    Change(ChangeStmt),
    AlterInstance(AlterInstanceStmt),
    AnalyzeTable(AnalyzeTableStmt),
    Explain(ExplainStmt),
    SetTxn(SetTxnStmt),
    Prepare(PrepareStmt),
    Execute(ExecuteStmt),
    Deallocate(DeallocateStmt),
    CreateBinding(CreateBindingStmt),
    DropBinding(DropBindingStmt),
    Set(SetStmt),
}

impl Node for Statement {
    fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            Statement::Select(s) => s.restore(ctx),
            Statement::CreateDatabase(s) => s.restore(ctx),
            Statement::DropDatabase(s) => s.restore(ctx),
            Statement::AlterDatabase(s) => s.restore(ctx),
            Statement::CreateTable(s) => s.restore(ctx),
            Statement::CreateView(s) => s.restore(ctx),
            Statement::CreateSequence(s) => s.restore(ctx),
            Statement::DropSequence(s) => s.restore(ctx),
            Statement::AlterSequence(s) => s.restore(ctx),
            Statement::DropTable(s) => s.restore(ctx),
            Statement::TruncateTable(s) => s.restore(ctx),
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
            Statement::Savepoint(s) => s.restore(ctx),
            Statement::ReleaseSavepoint(s) => s.restore(ctx),
            Statement::SetOpr(s) => s.restore(ctx),
            Statement::Grant(s) => s.restore(ctx),
            Statement::Revoke(s) => s.restore(ctx),
            Statement::CreateRole(s) => s.restore(ctx),
            Statement::DropRole(s) => s.restore(ctx),
            Statement::CreateUser(s) => s.restore(ctx),
            Statement::DropUser(s) => s.restore(ctx),
            Statement::GrantRole(s) => s.restore(ctx),
            Statement::RevokeRole(s) => s.restore(ctx),
            Statement::SetRole(s) => s.restore(ctx),
            Statement::SetPwd(s) => s.restore(ctx),
            Statement::Flush(s) => s.restore(ctx),
            Statement::Do(s) => s.restore(ctx),
            Statement::Shutdown(s) => s.restore(ctx),
            Statement::Kill(s) => s.restore(ctx),
            Statement::LockTables(s) => s.restore(ctx),
            Statement::CleanupTableLock(s) => s.restore(ctx),
            Statement::OptimizeTable(s) => s.restore(ctx),
            Statement::RepairTable(s) => s.restore(ctx),
            Statement::RenameUser(s) => s.restore(ctx),
            Statement::Binlog(s) => s.restore(ctx),
            Statement::Change(s) => s.restore(ctx),
            Statement::AlterInstance(s) => s.restore(ctx),
            Statement::AnalyzeTable(s) => s.restore(ctx),
            Statement::Explain(s) => s.restore(ctx),
            Statement::SetTxn(s) => s.restore(ctx),
            Statement::Prepare(s) => s.restore(ctx),
            Statement::Execute(s) => s.restore(ctx),
            Statement::Deallocate(s) => s.restore(ctx),
            Statement::CreateBinding(s) => s.restore(ctx),
            Statement::DropBinding(s) => s.restore(ctx),
            Statement::Set(s) => s.restore(ctx),
        }
    }
}

impl StmtNode for Statement {
    fn stmt_type(&self) -> &'static str {
        match self {
            Statement::Select(_) => "SelectStmt",
            Statement::CreateDatabase(_) => "CreateDatabaseStmt",
            Statement::DropDatabase(_) => "DropDatabaseStmt",
            Statement::AlterDatabase(_) => "AlterDatabaseStmt",
            Statement::CreateTable(_) => "CreateTableStmt",
            Statement::CreateView(_) => "CreateViewStmt",
            Statement::CreateSequence(_) => "CreateSequenceStmt",
            Statement::DropSequence(_) => "DropSequenceStmt",
            Statement::AlterSequence(_) => "AlterSequenceStmt",
            Statement::DropTable(_) => "DropTableStmt",
            Statement::TruncateTable(_) => "TruncateTableStmt",
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
            Statement::Savepoint(_) => "SavepointStmt",
            Statement::ReleaseSavepoint(_) => "ReleaseSavepointStmt",
            Statement::SetOpr(_) => "SetOprStmt",
            Statement::Grant(_) => "GrantStmt",
            Statement::Revoke(_) => "RevokeStmt",
            Statement::CreateRole(_) => "CreateRoleStmt",
            Statement::DropRole(_) => "DropRoleStmt",
            Statement::CreateUser(_) => "CreateUserStmt",
            Statement::DropUser(_) => "DropUserStmt",
            Statement::GrantRole(_) => "GrantRoleStmt",
            Statement::RevokeRole(_) => "RevokeRoleStmt",
            Statement::SetRole(_) => "SetRoleStmt",
            Statement::SetPwd(_) => "SetPwdStmt",
            Statement::Flush(_) => "FlushStmt",
            Statement::Do(_) => "DoStmt",
            Statement::Shutdown(_) => "ShutdownStmt",
            Statement::Kill(_) => "KillStmt",
            Statement::LockTables(_) => "LockTablesStmt",
            Statement::CleanupTableLock(_) => "CleanupTableLockStmt",
            Statement::OptimizeTable(_) => "OptimizeTableStmt",
            Statement::RepairTable(_) => "RepairTableStmt",
            Statement::RenameUser(_) => "RenameUserStmt",
            Statement::Binlog(_) => "BinlogStmt",
            Statement::Change(_) => "ChangeStmt",
            Statement::AlterInstance(_) => "AlterInstanceStmt",
            Statement::AnalyzeTable(_) => "AnalyzeTableStmt",
            Statement::Explain(_) => "ExplainStmt",
            Statement::SetTxn(_) => "SetTxnStmt",
            Statement::Prepare(_) => "PrepareStmt",
            Statement::Execute(_) => "ExecuteStmt",
            Statement::Deallocate(_) => "DeallocateStmt",
            Statement::CreateBinding(_) => "CreateBindingStmt",
            Statement::DropBinding(_) => "DropBindingStmt",
            Statement::Set(_) => "SetStmt",
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
            Statement::Insert(s) => s.with.as_ref(),
            Statement::Update(s) => s.with.as_ref(),
            Statement::Delete(s) => s.with.as_ref(),
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

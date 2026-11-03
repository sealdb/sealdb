use super::ddl::{
    AlterDatabaseStmt, AlterSequenceStmt, AlterTableStmt, CreateDatabaseStmt, CreateIndexStmt,
    CreateSequenceStmt, CreateTableStmt, CreateViewStmt, DropDatabaseStmt, DropIndexStmt,
    DropSequenceStmt, DropTableStmt, TruncateTableStmt,
};
use super::dml::{DeleteStmt, InsertStmt, LoadDataStmt, SelectStmt, SetOprStmt, UpdateStmt};
use super::expressions::ExprNode;
use super::misc::{
    AddQueryWatchStmt, AlterInstanceStmt, AnalyzeTableStmt, BRIEStmt, BeginStmt, BinlogStmt,
    CalibrateResourceStmt, ChangeStmt, CleanupTableLockStmt, CommitStmt, CompactTableStmt,
    CreateBindingStmt, CreateResourceGroupStmt, CreateRoleStmt, CreateUserStmt, DeallocateStmt,
    DistributeTableStmt, DoStmt, DropBindingStmt, DropPlacementPolicyStmt, DropQueryWatchStmt,
    DropRoleStmt, DropUserStmt, ExecuteStmt, FlashBackDatabaseStmt, FlashBackTableStmt,
    FlashBackToTimestampStmt, FlushStmt, GrantRoleStmt, GrantStmt, ImportIntoStmt, KillStmt,
    LoadStatsStmt, LockStatsStmt, LockTablesStmt, OptimizeTableStmt, PlanReplayerStmt, PrepareStmt,
    RecommendIndexStmt, RecoverTableStmt, ReleaseSavepointStmt, RenameUserStmt, RepairTableStmt,
    RevokeRoleStmt, RevokeStmt, RollbackStmt, SavepointStmt, SetPwdStmt, SetResourceGroupStmt,
    SetRoleStmt, SetStmt, SetTxnStmt, ShowStmt, ShutdownStmt, SplitRegionStmt, TraceStmt,
    UnlockStatsStmt, UseStmt,
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
    LoadData(LoadDataStmt),
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
    LoadStats(LoadStatsStmt),
    LockStats(LockStatsStmt),
    UnlockStats(UnlockStatsStmt),
    RenameUser(RenameUserStmt),
    Binlog(BinlogStmt),
    Change(ChangeStmt),
    AlterInstance(AlterInstanceStmt),
    Trace(TraceStmt),
    PlanReplayer(PlanReplayerStmt),
    BRIE(BRIEStmt),
    ImportInto(ImportIntoStmt),
    SplitRegion(SplitRegionStmt),
    DistributeTable(DistributeTableStmt),
    CompactTable(CompactTableStmt),
    RecommendIndex(RecommendIndexStmt),
    CalibrateResource(CalibrateResourceStmt),
    FlashBackTable(FlashBackTableStmt),
    FlashBackDatabase(FlashBackDatabaseStmt),
    FlashBackToTimestamp(FlashBackToTimestampStmt),
    RecoverTable(RecoverTableStmt),
    DropPlacementPolicy(DropPlacementPolicyStmt),
    CreateResourceGroup(CreateResourceGroupStmt),
    SetResourceGroup(SetResourceGroupStmt),
    AddQueryWatch(AddQueryWatchStmt),
    DropQueryWatch(DropQueryWatchStmt),
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
            Statement::LoadData(s) => s.restore(ctx),
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
            Statement::LoadStats(s) => s.restore(ctx),
            Statement::LockStats(s) => s.restore(ctx),
            Statement::UnlockStats(s) => s.restore(ctx),
            Statement::RenameUser(s) => s.restore(ctx),
            Statement::Binlog(s) => s.restore(ctx),
            Statement::Change(s) => s.restore(ctx),
            Statement::AlterInstance(s) => s.restore(ctx),
            Statement::Trace(s) => s.restore(ctx),
            Statement::PlanReplayer(s) => s.restore(ctx),
            Statement::BRIE(s) => s.restore(ctx),
            Statement::ImportInto(s) => s.restore(ctx),
            Statement::SplitRegion(s) => s.restore(ctx),
            Statement::DistributeTable(s) => s.restore(ctx),
            Statement::CompactTable(s) => s.restore(ctx),
            Statement::RecommendIndex(s) => s.restore(ctx),
            Statement::CalibrateResource(s) => s.restore(ctx),
            Statement::FlashBackTable(s) => s.restore(ctx),
            Statement::FlashBackDatabase(s) => s.restore(ctx),
            Statement::FlashBackToTimestamp(s) => s.restore(ctx),
            Statement::RecoverTable(s) => s.restore(ctx),
            Statement::DropPlacementPolicy(s) => s.restore(ctx),
            Statement::CreateResourceGroup(s) => s.restore(ctx),
            Statement::SetResourceGroup(s) => s.restore(ctx),
            Statement::AddQueryWatch(s) => s.restore(ctx),
            Statement::DropQueryWatch(s) => s.restore(ctx),
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
            Statement::LoadData(_) => "LoadDataStmt",
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
            Statement::LoadStats(_) => "LoadStatsStmt",
            Statement::LockStats(_) => "LockStatsStmt",
            Statement::UnlockStats(_) => "UnlockStatsStmt",
            Statement::RenameUser(_) => "RenameUserStmt",
            Statement::Binlog(_) => "BinlogStmt",
            Statement::Change(_) => "ChangeStmt",
            Statement::AlterInstance(_) => "AlterInstanceStmt",
            Statement::Trace(_) => "TraceStmt",
            Statement::PlanReplayer(_) => "PlanReplayerStmt",
            Statement::BRIE(_) => "BRIEStmt",
            Statement::ImportInto(_) => "ImportIntoStmt",
            Statement::SplitRegion(_) => "SplitRegionStmt",
            Statement::DistributeTable(_) => "DistributeTableStmt",
            Statement::CompactTable(_) => "CompactTableStmt",
            Statement::RecommendIndex(_) => "RecommendIndexStmt",
            Statement::CalibrateResource(_) => "CalibrateResourceStmt",
            Statement::FlashBackTable(_) => "FlashBackTableStmt",
            Statement::FlashBackDatabase(_) => "FlashBackDatabaseStmt",
            Statement::FlashBackToTimestamp(_) => "FlashBackToTimestampStmt",
            Statement::RecoverTable(_) => "RecoverTableStmt",
            Statement::DropPlacementPolicy(_) => "DropPlacementPolicyStmt",
            Statement::CreateResourceGroup(_) => "CreateResourceGroupStmt",
            Statement::SetResourceGroup(_) => "SetResourceGroupStmt",
            Statement::AddQueryWatch(_) => "AddQueryWatchStmt",
            Statement::DropQueryWatch(_) => "DropQueryWatchStmt",
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

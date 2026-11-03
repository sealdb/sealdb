use super::ddl::TableName;
use super::expressions::ExprNode;
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrivilegeType(u64);

impl PrivilegeType {
    pub const SELECT: Self = Self(1 << 0);
    pub const INSERT: Self = Self(1 << 1);
    pub const UPDATE: Self = Self(1 << 2);
    pub const DELETE: Self = Self(1 << 3);
    pub const CREATE: Self = Self(1 << 4);
    pub const DROP: Self = Self(1 << 5);
    pub const ALL: Self = Self(
        Self::SELECT.0
            | Self::INSERT.0
            | Self::UPDATE.0
            | Self::DELETE.0
            | Self::CREATE.0
            | Self::DROP.0,
    );

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn is_all(self) -> bool {
        self == Self::ALL
    }

    /// Bitmask for persistence (mysql.user-style grant bits).
    pub fn bits(self) -> u64 {
        self.0
    }

    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub fn names(self) -> Vec<&'static str> {
        if self.is_all() {
            return vec!["ALL"];
        }
        let mut names = Vec::new();
        for (bit, name) in [
            (Self::SELECT, "SELECT"),
            (Self::INSERT, "INSERT"),
            (Self::UPDATE, "UPDATE"),
            (Self::DELETE, "DELETE"),
            (Self::CREATE, "CREATE"),
            (Self::DROP, "DROP"),
        ] {
            if self.contains(bit) {
                names.push(name);
            }
        }
        names
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantLevelType {
    Global,
    DB,
    Table,
}

#[derive(Clone, Debug)]
pub struct GrantLevel {
    pub level: GrantLevelType,
    pub db_name: String,
    pub table_name: String,
}

impl GrantLevel {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self.level {
            GrantLevelType::Global => ctx.write_str("*.*"),
            GrantLevelType::DB => {
                if self.db_name.is_empty() {
                    ctx.write_str("*");
                } else {
                    ctx.write_name(&self.db_name);
                    ctx.write_str(".*");
                }
            }
            GrantLevelType::Table => {
                if !self.db_name.is_empty() {
                    ctx.write_name(&self.db_name);
                    ctx.write_str(".");
                }
                ctx.write_name(&self.table_name);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserIdentity {
    pub username: String,
    pub hostname: String,
}

impl UserIdentity {
    pub fn new(username: impl Into<String>, hostname: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            hostname: hostname.into(),
        }
    }

    pub fn root() -> Self {
        Self::new("root", "%")
    }

    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&format!("'{}'@'{}'", self.username, self.hostname));
        Ok(())
    }

    pub fn display(&self) -> String {
        format!("{}@{}", self.username, self.hostname)
    }
}

#[derive(Clone, Debug)]
pub struct PrivElem {
    pub tp: PrivilegeType,
    /// Column list for `GRANT SELECT (c1, c2) ON t` (empty = table-level).
    pub cols: Vec<String>,
}

impl PrivElem {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&self.tp.names().join(", "));
        if !self.cols.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.cols.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                ctx.write_name(col);
            }
            ctx.write_str(")");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GrantStmt {
    pub privs: Vec<PrivElem>,
    pub level: GrantLevel,
    pub users: Vec<UserIdentity>,
    /// TiDB/MySQL `WITH GRANT OPTION`.
    pub with_grant: bool,
    /// TiDB dynamic privileges (`BACKUP_ADMIN`, `ROLE_ADMIN`, …) on `*.*`.
    pub dynamic_privs: Vec<String>,
}

impl GrantStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("GRANT ");
        if !self.dynamic_privs.is_empty() {
            for (i, name) in self.dynamic_privs.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                ctx.write_str(name);
            }
        } else {
            restore_privs(&self.privs, ctx)?;
        }
        ctx.write_str(" ON ");
        self.level.restore(ctx)?;
        ctx.write_str(" TO ");
        restore_users(&self.users, ctx)?;
        if self.with_grant {
            ctx.write_str(" WITH GRANT OPTION");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RevokeStmt {
    pub privs: Vec<PrivElem>,
    pub level: GrantLevel,
    pub users: Vec<UserIdentity>,
    /// `REVOKE GRANT OPTION FOR ...` clears only the grant option bit.
    pub grant_option: bool,
    /// TiDB dynamic privileges to revoke.
    pub dynamic_privs: Vec<String>,
}

impl RevokeStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("REVOKE ");
        if self.grant_option {
            ctx.write_str("GRANT OPTION FOR ");
        }
        if !self.dynamic_privs.is_empty() {
            for (i, name) in self.dynamic_privs.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                ctx.write_str(name);
            }
        } else {
            restore_privs(&self.privs, ctx)?;
        }
        ctx.write_str(" ON ");
        self.level.restore(ctx)?;
        ctx.write_str(" FROM ");
        restore_users(&self.users, ctx)
    }
}

/// `CREATE USER 'u'@'%' [IDENTIFIED BY 'pwd']` (TiDB `CreateUserStmt`).
#[derive(Clone, Debug)]
pub struct CreateUserStmt {
    pub if_not_exists: bool,
    pub users: Vec<UserSpec>,
}

impl CreateUserStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE USER ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        for (i, spec) in self.users.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            spec.restore(ctx)?;
        }
        Ok(())
    }
}

/// `DROP USER [IF EXISTS] 'u'@'%'`.
#[derive(Clone, Debug)]
pub struct DropUserStmt {
    pub if_exists: bool,
    pub users: Vec<UserIdentity>,
}

impl DropUserStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP USER ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        restore_users(&self.users, ctx)
    }
}

#[derive(Clone, Debug)]
pub struct UserSpec {
    pub user: UserIdentity,
    pub auth: Option<String>,
}

impl UserSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.user.restore(ctx)?;
        if let Some(auth) = &self.auth {
            ctx.write_str(" IDENTIFIED BY ");
            ctx.write_str(&format!("'{auth}'"));
        }
        Ok(())
    }
}

/// `CREATE ROLE r1 [, r2 ...]` (TiDB `CreateRoleStmt`).
#[derive(Clone, Debug)]
pub struct CreateRoleStmt {
    pub if_not_exists: bool,
    pub roles: Vec<UserIdentity>,
}

impl CreateRoleStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE ROLE ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        restore_role_names(&self.roles, ctx)
    }
}

/// `DROP ROLE r1 [, r2 ...]`.
#[derive(Clone, Debug)]
pub struct DropRoleStmt {
    pub if_exists: bool,
    pub roles: Vec<UserIdentity>,
}

impl DropRoleStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP ROLE ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        restore_role_names(&self.roles, ctx)
    }
}

/// `GRANT r1 [, r2 ...] TO user` (no ON clause).
#[derive(Clone, Debug)]
pub struct GrantRoleStmt {
    pub roles: Vec<UserIdentity>,
    pub users: Vec<UserIdentity>,
}

impl GrantRoleStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("GRANT ");
        restore_role_names(&self.roles, ctx)?;
        ctx.write_str(" TO ");
        restore_users(&self.users, ctx)
    }
}

/// `REVOKE r1 [, r2 ...] FROM user`.
#[derive(Clone, Debug)]
pub struct RevokeRoleStmt {
    pub roles: Vec<UserIdentity>,
    pub users: Vec<UserIdentity>,
}

impl RevokeRoleStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("REVOKE ");
        restore_role_names(&self.roles, ctx)?;
        ctx.write_str(" FROM ");
        restore_users(&self.users, ctx)
    }
}

/// `SET ROLE` target (TiDB `SetRoleStmt`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetRoleKind {
    None,
    All,
    Roles(Vec<UserIdentity>),
}

#[derive(Clone, Debug)]
pub struct SetRoleStmt {
    pub kind: SetRoleKind,
}

impl SetRoleStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SET ROLE ");
        match &self.kind {
            SetRoleKind::None => ctx.write_str("NONE"),
            SetRoleKind::All => ctx.write_str("ALL"),
            SetRoleKind::Roles(roles) => restore_role_names(roles, ctx)?,
        }
        Ok(())
    }
}

fn restore_role_names(roles: &[UserIdentity], ctx: &mut RestoreCtx) -> Result<()> {
    for (i, role) in roles.iter().enumerate() {
        if i > 0 {
            ctx.write_str(", ");
        }
        if role.hostname == "%" {
            ctx.write_name(&role.username);
        } else {
            role.restore(ctx)?;
        }
    }
    Ok(())
}

fn restore_privs(privs: &[PrivElem], ctx: &mut RestoreCtx) -> Result<()> {
    for (i, elem) in privs.iter().enumerate() {
        if i > 0 {
            ctx.write_str(", ");
        }
        elem.restore(ctx)?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct AnalyzeTableStmt {
    pub table: TableName,
    /// Optional index names for `ANALYZE TABLE t INDEX idx1, idx2`.
    pub indexes: Vec<String>,
}

impl AnalyzeTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ANALYZE TABLE ");
        self.table.restore(ctx)?;
        if !self.indexes.is_empty() {
            ctx.write_str(" INDEX ");
            for (i, name) in self.indexes.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                ctx.write_name(name);
            }
        }
        Ok(())
    }
}

fn restore_users(users: &[UserIdentity], ctx: &mut RestoreCtx) -> Result<()> {
    for (i, user) in users.iter().enumerate() {
        if i > 0 {
            ctx.write_str(", ");
        }
        user.restore(ctx)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowStmtKind {
    Databases,
    Tables,
    Columns,
    Grants,
    CreateTable,
    CreateView,
    CreateDatabase,
    Index,
    Charset,
    Collation,
    /// `ADMIN SHOW DDL JOBS`
    DdlJobs,
    /// `SHOW BINDINGS`
    Bindings,
}

#[derive(Clone, Debug)]
pub struct ShowStmt {
    pub kind: ShowStmtKind,
    pub db_name: Option<String>,
    pub table: Option<TableName>,
    pub user: Option<UserIdentity>,
    pub full: bool,
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
                if self.full {
                    ctx.write_str("SHOW FULL COLUMNS FROM ");
                } else {
                    ctx.write_str("DESC ");
                }
                if let Some(table) = &self.table {
                    table.restore(ctx)?;
                }
            }
            ShowStmtKind::Grants => {
                ctx.write_str("SHOW GRANTS");
                if let Some(user) = &self.user {
                    ctx.write_str(" FOR ");
                    user.restore(ctx)?;
                }
            }
            ShowStmtKind::CreateTable => {
                ctx.write_str("SHOW CREATE TABLE ");
                if let Some(table) = &self.table {
                    table.restore(ctx)?;
                }
            }
            ShowStmtKind::CreateView => {
                ctx.write_str("SHOW CREATE VIEW ");
                if let Some(table) = &self.table {
                    table.restore(ctx)?;
                }
            }
            ShowStmtKind::CreateDatabase => {
                ctx.write_str("SHOW CREATE DATABASE ");
                if let Some(db) = &self.db_name {
                    ctx.write_name(db);
                }
            }
            ShowStmtKind::Index => {
                ctx.write_str("SHOW INDEX FROM ");
                if let Some(table) = &self.table {
                    table.restore(ctx)?;
                }
            }
            ShowStmtKind::Charset => ctx.write_str("SHOW CHARSET"),
            ShowStmtKind::Collation => ctx.write_str("SHOW COLLATION"),
            ShowStmtKind::DdlJobs => ctx.write_str("ADMIN SHOW DDL JOBS"),
            ShowStmtKind::Bindings => ctx.write_str("SHOW BINDINGS"),
        }
        Ok(())
    }
}

/// `CREATE [GLOBAL|SESSION] BINDING FOR <original> USING <bind>` (TiDB `CreateBindingStmt`).
#[derive(Clone, Debug)]
pub struct CreateBindingStmt {
    pub original_sql: String,
    pub bind_sql: String,
}

impl CreateBindingStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE BINDING FOR ");
        ctx.write_str(&self.original_sql);
        ctx.write_str(" USING ");
        ctx.write_str(&self.bind_sql);
        Ok(())
    }
}

/// `DROP [GLOBAL|SESSION] BINDING FOR <original>` (TiDB `DropBindingStmt`).
#[derive(Clone, Debug)]
pub struct DropBindingStmt {
    pub original_sql: String,
}

impl DropBindingStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP BINDING FOR ");
        ctx.write_str(&self.original_sql);
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IsolationLevel {
    ReadCommitted,
    #[default]
    RepeatableRead,
    /// Accepted; SealDB maps to RR snapshot semantics (MVB).
    Serializable,
}

#[derive(Clone, Debug, Default)]
pub struct BeginStmt {
    pub pessimistic: bool,
    /// Isolation from `START TRANSACTION ISOLATION LEVEL ...`. `None` inherits
    /// the session default set by `SET TRANSACTION`.
    pub isolation: Option<IsolationLevel>,
    /// Access mode from `READ ONLY` / `READ WRITE`. `None` inherits the session default.
    pub read_only: Option<bool>,
}

impl BeginStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("BEGIN");
        if self.pessimistic {
            ctx.write_str(" PESSIMISTIC");
        }
        restore_txn_chars(ctx, self.isolation, self.read_only)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SetTxnStmt {
    /// `SET TRANSACTION ISOLATION LEVEL ...`. `None` leaves the session isolation unchanged.
    pub isolation: Option<IsolationLevel>,
    /// `SET TRANSACTION READ ONLY` / `READ WRITE`. `None` leaves the access mode unchanged.
    pub read_only: Option<bool>,
}

impl SetTxnStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SET TRANSACTION");
        restore_txn_chars(ctx, self.isolation, self.read_only)?;
        Ok(())
    }
}

fn restore_txn_chars(
    ctx: &mut RestoreCtx,
    isolation: Option<IsolationLevel>,
    read_only: Option<bool>,
) -> Result<()> {
    if let Some(isolation) = isolation {
        ctx.write_str(" ISOLATION LEVEL ");
        ctx.write_str(match isolation {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
        });
    }
    if let Some(read_only) = read_only {
        ctx.write_str(if read_only {
            " READ ONLY"
        } else {
            " READ WRITE"
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct CommitStmt {
    /// `COMMIT AND CHAIN` starts a new transaction with the same characteristics.
    pub chain: bool,
}

impl CommitStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("COMMIT");
        if self.chain {
            ctx.write_str(" AND CHAIN");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct RollbackStmt {
    /// `ROLLBACK TO SAVEPOINT name`. Full rollback when `None`.
    pub savepoint: Option<String>,
    /// `ROLLBACK AND CHAIN` starts a new transaction with the same characteristics.
    pub chain: bool,
}

impl RollbackStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ROLLBACK");
        if self.chain {
            ctx.write_str(" AND CHAIN");
        }
        if let Some(name) = &self.savepoint {
            ctx.write_str(" TO ");
            ctx.write_name(name);
        }
        Ok(())
    }
}

/// `SAVEPOINT name` marker inside an open transaction.
#[derive(Clone, Debug)]
pub struct SavepointStmt {
    pub name: String,
}

impl SavepointStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SAVEPOINT ");
        ctx.write_name(&self.name);
        Ok(())
    }
}

/// `RELEASE SAVEPOINT name` drops that marker and later ones.
#[derive(Clone, Debug)]
pub struct ReleaseSavepointStmt {
    pub name: String,
}

impl ReleaseSavepointStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("RELEASE SAVEPOINT ");
        ctx.write_name(&self.name);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PrepareStmt {
    pub name: String,
    pub sql: String,
}

impl PrepareStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("PREPARE ");
        ctx.write_name(&self.name);
        ctx.write_str(" FROM '");
        ctx.write_str(&self.sql);
        ctx.write_str("'");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ExecuteStmt {
    pub name: String,
    pub using: Vec<ExprNode>,
}

impl ExecuteStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("EXECUTE ");
        ctx.write_name(&self.name);
        if !self.using.is_empty() {
            ctx.write_str(" USING ");
            for (i, expr) in self.using.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                expr.restore(ctx)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DeallocateStmt {
    pub name: String,
}

impl DeallocateStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DEALLOCATE PREPARE ");
        ctx.write_name(&self.name);
        Ok(())
    }
}

/// `SET` / `SET SESSION` / `SET GLOBAL` scope (from sqlparser `ContextModifier`
/// or `@@GLOBAL.` / `@@SESSION.` object-name prefixes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SetVarScope {
    #[default]
    Session,
    Global,
}

#[derive(Clone, Debug)]
pub struct SetAssignment {
    pub name: String,
    pub value: String,
    pub scope: SetVarScope,
}

#[derive(Clone, Debug)]
pub struct SetStmt {
    pub assignments: Vec<SetAssignment>,
}

impl SetStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SET ");
        for (i, a) in self.assignments.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            match a.scope {
                SetVarScope::Global => ctx.write_str("GLOBAL "),
                SetVarScope::Session => {}
            }
            ctx.write_str(&a.name);
            ctx.write_str(" = ");
            ctx.write_str(&a.value);
        }
        Ok(())
    }
}

/// `DO expr[, expr…]` — evaluate for side effects; no result set (MySQL/TiDB `DoStmt`).
#[derive(Clone, Debug)]
pub struct DoStmt {
    pub exprs: Vec<ExprNode>,
}

impl DoStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DO ");
        for (i, expr) in self.exprs.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            expr.restore(ctx)?;
        }
        Ok(())
    }
}

/// `SET PASSWORD [FOR user] = 'pwd'` (TiDB `SetPwdStmt`).
#[derive(Clone, Debug)]
pub struct SetPwdStmt {
    pub user: Option<UserIdentity>,
    pub password: String,
}

impl SetPwdStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SET PASSWORD");
        if let Some(user) = &self.user {
            ctx.write_str(" FOR ");
            user.restore(ctx)?;
        }
        ctx.write_str(" = ");
        ctx.write_str(&format!("'{}'", self.password));
        Ok(())
    }
}

/// Flush target (TiDB `FlushStmtType` subset).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushStmtType {
    Privileges,
    Tables,
    Other,
}

/// `FLUSH PRIVILEGES` / `FLUSH TABLES` (TiDB `FlushStmt`).
#[derive(Clone, Debug)]
pub struct FlushStmt {
    pub tp: FlushStmtType,
    pub no_write_to_binlog: bool,
}

impl FlushStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("FLUSH ");
        if self.no_write_to_binlog {
            ctx.write_str("NO_WRITE_TO_BINLOG ");
        }
        match self.tp {
            FlushStmtType::Privileges => ctx.write_str("PRIVILEGES"),
            FlushStmtType::Tables => ctx.write_str("TABLES"),
            FlushStmtType::Other => ctx.write_str("STATUS"),
        }
        Ok(())
    }
}

/// `SHUTDOWN` — request server exit (TiDB `ShutdownStmt`).
#[derive(Clone, Debug, Default)]
pub struct ShutdownStmt;

impl ShutdownStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SHUTDOWN");
        Ok(())
    }
}

/// `KILL [CONNECTION|QUERY] id` (TiDB `KillStmt`).
#[derive(Clone, Debug)]
pub struct KillStmt {
    pub query: bool,
    pub connection_id: u64,
}

impl KillStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("KILL ");
        if self.query {
            ctx.write_str("QUERY ");
        } else {
            ctx.write_str("CONNECTION ");
        }
        ctx.write_str(&self.connection_id.to_string());
        Ok(())
    }
}

/// Lock mode for `LOCK TABLES` (TiDB `TableLock` / READ|WRITE).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableLockType {
    Read,
    Write,
}

/// One entry in `LOCK TABLES t READ|WRITE`.
#[derive(Clone, Debug)]
pub struct TableLockItem {
    pub table: TableName,
    pub lock_type: TableLockType,
}

/// `LOCK TABLES t READ|WRITE[, …]` (TiDB `LockTablesStmt`).
#[derive(Clone, Debug)]
pub struct LockTablesStmt {
    pub table_locks: Vec<TableLockItem>,
}

impl LockTablesStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("LOCK TABLES ");
        for (i, item) in self.table_locks.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            item.table.restore(ctx)?;
            match item.lock_type {
                TableLockType::Read => ctx.write_str(" READ"),
                TableLockType::Write => ctx.write_str(" WRITE"),
            }
        }
        Ok(())
    }
}

/// `UNLOCK TABLES` / `ADMIN CLEANUP TABLE LOCK` (TiDB `CleanupTableLockStmt`).
#[derive(Clone, Debug)]
pub struct CleanupTableLockStmt {
    /// Empty = unlock every table held by the current session (`UNLOCK TABLES`).
    pub tables: Vec<TableName>,
}

impl CleanupTableLockStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if self.tables.is_empty() {
            ctx.write_str("UNLOCK TABLES");
            return Ok(());
        }
        ctx.write_str("ADMIN CLEANUP TABLE LOCK ");
        for (i, table) in self.tables.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            table.restore(ctx)?;
        }
        Ok(())
    }
}

/// `OPTIMIZE TABLE t[, …]` (TiDB `OptimizeTableStmt`) — MVB status-row no-op.
#[derive(Clone, Debug)]
pub struct OptimizeTableStmt {
    pub tables: Vec<TableName>,
}

impl OptimizeTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("OPTIMIZE TABLE ");
        for (i, table) in self.tables.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            table.restore(ctx)?;
        }
        Ok(())
    }
}

/// `REPAIR TABLE t[, …]` MySQL-style status rows (mapped to TiDB `RepairTableStmt`).
#[derive(Clone, Debug)]
pub struct RepairTableStmt {
    pub tables: Vec<TableName>,
}

impl RepairTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("REPAIR TABLE ");
        for (i, table) in self.tables.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            table.restore(ctx)?;
        }
        Ok(())
    }
}

/// One `old TO new` pair in `RENAME USER`.
#[derive(Clone, Debug)]
pub struct UserToUser {
    pub old_user: UserIdentity,
    pub new_user: UserIdentity,
}

/// `RENAME USER 'u'@'%' TO 'v'@'%'` (TiDB `RenameUserStmt`).
#[derive(Clone, Debug)]
pub struct RenameUserStmt {
    pub user_to_users: Vec<UserToUser>,
}

impl RenameUserStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("RENAME USER ");
        for (i, pair) in self.user_to_users.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            pair.old_user.restore(ctx)?;
            ctx.write_str(" TO ");
            pair.new_user.restore(ctx)?;
        }
        Ok(())
    }
}

/// `BINLOG '…'` (TiDB `BinlogStmt`) — accepted no-op.
#[derive(Clone, Debug)]
pub struct BinlogStmt {
    pub payload: String,
}

impl BinlogStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("BINLOG ");
        ctx.write_str(&format!("'{}'", self.payload));
        Ok(())
    }
}

/// TiDB `CHANGE PUMP/DRAINER …` (TiDB `ChangeStmt`) — accepted no-op.
#[derive(Clone, Debug)]
pub struct ChangeStmt {
    pub node_type: String,
    pub state: String,
    pub node_id: String,
}

impl ChangeStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CHANGE ");
        ctx.write_str(&self.node_type);
        ctx.write_str(" TO NODE_STATE = ");
        ctx.write_str(&format!("'{}'", self.state));
        ctx.write_str(" FOR NODE_ID ");
        ctx.write_str(&format!("'{}'", self.node_id));
        Ok(())
    }
}

/// `ALTER INSTANCE RELOAD TLS` (TiDB `AlterInstanceStmt`) — accepted no-op.
#[derive(Clone, Debug, Default)]
pub struct AlterInstanceStmt {
    pub reload_tls: bool,
    pub no_rollback_on_error: bool,
}

impl AlterInstanceStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ALTER INSTANCE");
        if self.reload_tls {
            ctx.write_str(" RELOAD TLS");
        }
        if self.no_rollback_on_error {
            ctx.write_str(" NO ROLLBACK ON ERROR");
        }
        Ok(())
    }
}

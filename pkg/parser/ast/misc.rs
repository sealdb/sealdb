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
}

impl PrivElem {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&self.tp.names().join(", "));
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GrantStmt {
    pub privs: Vec<PrivElem>,
    pub level: GrantLevel,
    pub users: Vec<UserIdentity>,
}

impl GrantStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("GRANT ");
        restore_privs(&self.privs, ctx)?;
        ctx.write_str(" ON ");
        self.level.restore(ctx)?;
        ctx.write_str(" TO ");
        restore_users(&self.users, ctx)
    }
}

#[derive(Clone, Debug)]
pub struct RevokeStmt {
    pub privs: Vec<PrivElem>,
    pub level: GrantLevel,
    pub users: Vec<UserIdentity>,
}

impl RevokeStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("REVOKE ");
        restore_privs(&self.privs, ctx)?;
        ctx.write_str(" ON ");
        self.level.restore(ctx)?;
        ctx.write_str(" FROM ");
        restore_users(&self.users, ctx)
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
}

impl AnalyzeTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ANALYZE TABLE ");
        self.table.restore(ctx)
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
    CreateDatabase,
    Index,
    Charset,
    Collation,
    /// `ADMIN SHOW DDL JOBS`
    DdlJobs,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IsolationLevel {
    ReadCommitted,
    #[default]
    RepeatableRead,
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

#[derive(Clone, Debug)]
pub struct SetAssignment {
    pub name: String,
    pub value: String,
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
            ctx.write_str(&a.name);
            ctx.write_str(" = ");
            ctx.write_str(&a.value);
        }
        Ok(())
    }
}

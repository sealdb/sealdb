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
}

#[derive(Clone, Debug)]
pub struct ShowStmt {
    pub kind: ShowStmtKind,
    pub db_name: Option<String>,
    pub table: Option<TableName>,
    pub user: Option<UserIdentity>,
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
            ShowStmtKind::Grants => {
                ctx.write_str("SHOW GRANTS");
                if let Some(user) = &self.user {
                    ctx.write_str(" FOR ");
                    user.restore(ctx)?;
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IsolationLevel {
    ReadCommitted,
    #[default]
    RepeatableRead,
}

#[derive(Clone, Debug, Default)]
pub struct BeginStmt {
    pub pessimistic: bool,
    pub isolation: IsolationLevel,
}

impl BeginStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("BEGIN");
        if self.pessimistic {
            ctx.write_str(" PESSIMISTIC");
        }
        match self.isolation {
            IsolationLevel::RepeatableRead => {}
            IsolationLevel::ReadCommitted => {
                ctx.write_str(" ISOLATION LEVEL READ COMMITTED");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SetTxnStmt {
    pub isolation: IsolationLevel,
}

impl SetTxnStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("SET TRANSACTION ISOLATION LEVEL ");
        ctx.write_str(match self.isolation {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
        });
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommitStmt;

impl CommitStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("COMMIT");
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct RollbackStmt;

impl RollbackStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ROLLBACK");
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

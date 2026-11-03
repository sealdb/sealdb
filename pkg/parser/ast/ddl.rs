use super::dml::SelectStmt;
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TableName {
    pub schema: Option<String>,
    pub name: String,
}

impl TableName {
    pub fn new(schema: Option<String>, name: impl Into<String>) -> Self {
        Self {
            schema,
            name: name.into(),
        }
    }

    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(schema) = &self.schema {
            ctx.write_name(schema);
            ctx.write_str(".");
        }
        ctx.write_name(&self.name);
        Ok(())
    }

    pub fn qualified(&self, default_schema: &str) -> (String, String) {
        let schema = self
            .schema
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_schema.to_string());
        (schema, self.name.clone())
    }
}

/// MySQL `FIRST` / `AFTER col` placement for ADD/CHANGE/MODIFY COLUMN.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ColumnPosition {
    #[default]
    None,
    First,
    After(String),
}

impl ColumnPosition {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            ColumnPosition::None => Ok(()),
            ColumnPosition::First => {
                ctx.write_str(" FIRST");
                Ok(())
            }
            ColumnPosition::After(name) => {
                ctx.write_str(" AFTER ");
                ctx.write_name(name);
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub type_name: String,
}

impl ColumnDef {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_name(&self.name);
        ctx.write_str(" ");
        ctx.write_str(&self.type_name);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CreateDatabaseStmt {
    pub if_not_exists: bool,
    pub name: String,
}

impl CreateDatabaseStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE DATABASE ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        ctx.write_name(&self.name);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DropDatabaseStmt {
    pub if_exists: bool,
    pub name: String,
}

impl DropDatabaseStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP DATABASE ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        ctx.write_name(&self.name);
        Ok(())
    }
}

/// `ALTER DATABASE db [CHARACTER SET [=] cs] [COLLATE [=] coll]` (TiDB `AlterDatabaseStmt`).
#[derive(Clone, Debug)]
pub struct AlterDatabaseStmt {
    /// Empty means alter the session current database.
    pub name: String,
    pub charset: Option<String>,
    pub collate: Option<String>,
}

impl AlterDatabaseStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ALTER DATABASE");
        if !self.name.is_empty() {
            ctx.write_str(" ");
            ctx.write_name(&self.name);
        }
        if let Some(cs) = &self.charset {
            ctx.write_str(" CHARACTER SET ");
            ctx.write_str(cs);
        }
        if let Some(coll) = &self.collate {
            ctx.write_str(" COLLATE ");
            ctx.write_str(coll);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

impl IndexDef {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if self.primary {
            ctx.write_str("PRIMARY KEY (");
        } else if self.unique {
            ctx.write_str("UNIQUE INDEX ");
            ctx.write_name(&self.name);
            ctx.write_str(" (");
        } else {
            ctx.write_str("INDEX ");
            ctx.write_name(&self.name);
            ctx.write_str(" (");
        }
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            ctx.write_name(col);
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionType {
    Range,
    Hash,
    List,
    Key,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionDef {
    pub name: String,
    /// RANGE bound; `None` means MAXVALUE. Unused for HASH/KEY/LIST.
    pub less_than: Option<i64>,
    /// RANGE COLUMNS extra bounds after the first `less_than` (`None` = MAXVALUE).
    pub extra_bounds: Vec<Option<i64>>,
    /// LIST `VALUES IN (...)` integers.
    pub in_values: Vec<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionOption {
    pub tp: PartitionType,
    pub column: String,
    /// RANGE COLUMNS list; empty means a single `column` or an `expr` key.
    pub columns: Vec<String>,
    /// Non-column partition key such as `id % 4` or `id + 1`.
    pub expr: Option<String>,
    pub defs: Vec<PartitionDef>,
    pub num: u64,
    /// TiDB accepts `SUBPARTITION BY` under RANGE/LIST then strips it; true when
    /// that clause was present so DDL can ignore it (HASH/KEY reject instead).
    pub subpartition_ignored: bool,
}

#[derive(Clone, Debug)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableName,
    pub cols: Vec<ColumnDef>,
    pub indices: Vec<IndexDef>,
    pub partition: Option<PartitionOption>,
    /// TiDB `ReferTable` (`CREATE TABLE t LIKE src`).
    pub refer_table: Option<TableName>,
    /// TiDB `Select` (`CREATE TABLE t AS SELECT ...`).
    pub select: Option<Box<SelectStmt>>,
}

impl CreateTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE TABLE ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        self.table.restore(ctx)?;
        if let Some(refer) = &self.refer_table {
            ctx.write_str(" LIKE ");
            refer.restore(ctx)?;
        } else if !self.cols.is_empty() || !self.indices.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.cols.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                col.restore(ctx)?;
            }
            for index in &self.indices {
                ctx.write_str(",");
                index.restore(ctx)?;
            }
            ctx.write_str(")");
        }
        if let Some(part) = &self.partition {
            ctx.write_str(" PARTITION BY ");
            match part.tp {
                PartitionType::Range if part.columns.len() > 1 => ctx.write_str("RANGE COLUMNS"),
                PartitionType::Range => ctx.write_str("RANGE"),
                PartitionType::Hash => ctx.write_str("HASH"),
                PartitionType::List => ctx.write_str("LIST"),
                PartitionType::Key => ctx.write_str("KEY"),
            }
            ctx.write_str(" (");
            if let Some(expr) = &part.expr {
                ctx.write_str(expr);
            } else if part.columns.len() > 1 {
                for (i, col) in part.columns.iter().enumerate() {
                    if i > 0 {
                        ctx.write_str(", ");
                    }
                    ctx.write_name(col);
                }
            } else {
                ctx.write_name(&part.column);
            }
            ctx.write_str(")");
            if matches!(part.tp, PartitionType::Hash | PartitionType::Key) && part.defs.is_empty() {
                ctx.write_str(" PARTITIONS ");
                ctx.write_str(&part.num.to_string());
            } else {
                ctx.write_str(" (");
                for (i, def) in part.defs.iter().enumerate() {
                    if i > 0 {
                        ctx.write_str(", ");
                    }
                    ctx.write_str("PARTITION ");
                    ctx.write_name(&def.name);
                    if part.tp == PartitionType::Range {
                        ctx.write_str(" VALUES LESS THAN (");
                        match def.less_than {
                            Some(v) => ctx.write_str(&v.to_string()),
                            None => ctx.write_str("MAXVALUE"),
                        }
                        for bound in &def.extra_bounds {
                            ctx.write_str(", ");
                            match bound {
                                Some(v) => ctx.write_str(&v.to_string()),
                                None => ctx.write_str("MAXVALUE"),
                            }
                        }
                        ctx.write_str(")");
                    } else if part.tp == PartitionType::List {
                        ctx.write_str(" VALUES IN (");
                        for (j, v) in def.in_values.iter().enumerate() {
                            if j > 0 {
                                ctx.write_str(", ");
                            }
                            ctx.write_str(&v.to_string());
                        }
                        ctx.write_str(")");
                    }
                }
                ctx.write_str(")");
            }
        }
        if let Some(select) = &self.select {
            ctx.write_str(" AS ");
            select.restore(ctx)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DropTableStmt {
    pub if_exists: bool,
    pub tables: Vec<TableName>,
}

impl DropTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP TABLE ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        for (i, table) in self.tables.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            table.restore(ctx)?;
        }
        Ok(())
    }
}

/// `TRUNCATE [TABLE] db.t` — empty the table, keep schema (TiDB `TruncateTableStmt`).
#[derive(Clone, Debug)]
pub struct TruncateTableStmt {
    pub table: TableName,
}

impl TruncateTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("TRUNCATE TABLE ");
        self.table.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct CreateIndexStmt {
    pub if_not_exists: bool,
    pub table: TableName,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

impl CreateIndexStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE ");
        if self.unique {
            ctx.write_str("UNIQUE ");
        }
        ctx.write_str("INDEX ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        ctx.write_name(&self.name);
        ctx.write_str(" ON ");
        self.table.restore(ctx)?;
        ctx.write_str(" (");
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            ctx.write_name(col);
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DropIndexStmt {
    pub if_exists: bool,
    pub table: TableName,
    pub name: String,
}

impl DropIndexStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP INDEX ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        ctx.write_name(&self.name);
        ctx.write_str(" ON ");
        self.table.restore(ctx)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum AlterTableSpec {
    AddColumn {
        if_not_exists: bool,
        column: ColumnDef,
        position: ColumnPosition,
    },
    DropColumn {
        if_exists: bool,
        name: String,
    },
    AddIndex(IndexDef),
    DropIndex {
        if_exists: bool,
        name: String,
    },
    /// `CHANGE [COLUMN] old new type` — rename and/or change the column type.
    ChangeColumn {
        old_name: String,
        new_name: String,
        type_name: String,
        position: ColumnPosition,
    },
    /// `MODIFY [COLUMN] col type` — keep the name, change the type.
    ModifyColumn {
        name: String,
        type_name: String,
        position: ColumnPosition,
    },
    /// `RENAME COLUMN old TO new`.
    RenameColumn {
        old_name: String,
        new_name: String,
    },
    /// `ALTER TABLE t RENAME TO` / `RENAME TABLE t TO`.
    RenameTable {
        new_name: TableName,
    },
    /// `ALTER TABLE t RENAME INDEX old TO new`.
    RenameIndex {
        old_name: String,
        new_name: String,
    },
    /// `ALGORITHM [=] DEFAULT|INSTANT|INPLACE|COPY` — accepted and ignored.
    Algorithm(String),
    /// `LOCK [=] DEFAULT|NONE|SHARED|EXCLUSIVE` — accepted and ignored.
    Lock(String),
    /// `ADD PARTITION (PARTITION ...)` for RANGE/LIST tables.
    AddPartition {
        defs: Vec<PartitionDef>,
    },
    /// `DROP PARTITION p0 [, p1 ...]`.
    DropPartition {
        if_exists: bool,
        names: Vec<String>,
    },
}

impl AlterTableSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            AlterTableSpec::AddColumn {
                if_not_exists,
                column,
                position,
            } => {
                ctx.write_str("ADD COLUMN ");
                if *if_not_exists {
                    ctx.write_str("IF NOT EXISTS ");
                }
                column.restore(ctx)?;
                position.restore(ctx)
            }
            AlterTableSpec::DropColumn { if_exists, name } => {
                ctx.write_str("DROP COLUMN ");
                if *if_exists {
                    ctx.write_str("IF EXISTS ");
                }
                ctx.write_name(name);
                Ok(())
            }
            AlterTableSpec::AddIndex(index) => {
                ctx.write_str("ADD ");
                index.restore(ctx)
            }
            AlterTableSpec::DropIndex { if_exists, name } => {
                ctx.write_str("DROP INDEX ");
                if *if_exists {
                    ctx.write_str("IF EXISTS ");
                }
                ctx.write_name(name);
                Ok(())
            }
            AlterTableSpec::ChangeColumn {
                old_name,
                new_name,
                type_name,
                position,
            } => {
                ctx.write_str("CHANGE COLUMN ");
                ctx.write_name(old_name);
                ctx.write_str(" ");
                ctx.write_name(new_name);
                ctx.write_str(" ");
                ctx.write_str(type_name);
                position.restore(ctx)
            }
            AlterTableSpec::ModifyColumn {
                name,
                type_name,
                position,
            } => {
                ctx.write_str("MODIFY COLUMN ");
                ctx.write_name(name);
                ctx.write_str(" ");
                ctx.write_str(type_name);
                position.restore(ctx)
            }
            AlterTableSpec::RenameColumn { old_name, new_name } => {
                ctx.write_str("RENAME COLUMN ");
                ctx.write_name(old_name);
                ctx.write_str(" TO ");
                ctx.write_name(new_name);
                Ok(())
            }
            AlterTableSpec::RenameTable { new_name } => {
                ctx.write_str("RENAME TO ");
                new_name.restore(ctx)
            }
            AlterTableSpec::RenameIndex { old_name, new_name } => {
                ctx.write_str("RENAME INDEX ");
                ctx.write_name(old_name);
                ctx.write_str(" TO ");
                ctx.write_name(new_name);
                Ok(())
            }
            AlterTableSpec::Algorithm(name) => {
                ctx.write_str("ALGORITHM=");
                ctx.write_str(name);
                Ok(())
            }
            AlterTableSpec::Lock(name) => {
                ctx.write_str("LOCK=");
                ctx.write_str(name);
                Ok(())
            }
            AlterTableSpec::AddPartition { defs } => {
                ctx.write_str("ADD PARTITION (");
                for (i, def) in defs.iter().enumerate() {
                    if i > 0 {
                        ctx.write_str(", ");
                    }
                    ctx.write_str("PARTITION ");
                    ctx.write_name(&def.name);
                    if !def.in_values.is_empty() {
                        ctx.write_str(" VALUES IN (");
                        for (j, v) in def.in_values.iter().enumerate() {
                            if j > 0 {
                                ctx.write_str(", ");
                            }
                            ctx.write_str(&v.to_string());
                        }
                        ctx.write_str(")");
                    } else {
                        ctx.write_str(" VALUES LESS THAN (");
                        match def.less_than {
                            Some(v) => ctx.write_str(&v.to_string()),
                            None => ctx.write_str("MAXVALUE"),
                        }
                        for bound in &def.extra_bounds {
                            ctx.write_str(", ");
                            match bound {
                                Some(v) => ctx.write_str(&v.to_string()),
                                None => ctx.write_str("MAXVALUE"),
                            }
                        }
                        ctx.write_str(")");
                    }
                }
                ctx.write_str(")");
                Ok(())
            }
            AlterTableSpec::DropPartition { if_exists, names } => {
                ctx.write_str("DROP PARTITION ");
                if *if_exists {
                    ctx.write_str("IF EXISTS ");
                }
                for (i, name) in names.iter().enumerate() {
                    if i > 0 {
                        ctx.write_str(", ");
                    }
                    ctx.write_name(name);
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct AlterTableStmt {
    pub table: TableName,
    pub specs: Vec<AlterTableSpec>,
}

impl AlterTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ALTER TABLE ");
        self.table.restore(ctx)?;
        ctx.write_str(" ");
        for (i, spec) in self.specs.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            spec.restore(ctx)?;
        }
        Ok(())
    }
}

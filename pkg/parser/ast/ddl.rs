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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionDef {
    pub name: String,
    /// RANGE bound; `None` means MAXVALUE. Unused for HASH.
    pub less_than: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionOption {
    pub tp: PartitionType,
    pub column: String,
    pub defs: Vec<PartitionDef>,
    pub num: u64,
}

#[derive(Clone, Debug)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableName,
    pub cols: Vec<ColumnDef>,
    pub indices: Vec<IndexDef>,
    pub partition: Option<PartitionOption>,
}

impl CreateTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE TABLE ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        self.table.restore(ctx)?;
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
        if let Some(part) = &self.partition {
            ctx.write_str(" PARTITION BY ");
            match part.tp {
                PartitionType::Range => ctx.write_str("RANGE"),
                PartitionType::Hash => ctx.write_str("HASH"),
            }
            ctx.write_str(" (");
            ctx.write_name(&part.column);
            ctx.write_str(")");
            if part.tp == PartitionType::Hash && part.defs.is_empty() {
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
                        ctx.write_str(")");
                    }
                }
                ctx.write_str(")");
            }
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
}

impl AlterTableSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            AlterTableSpec::AddColumn {
                if_not_exists,
                column,
            } => {
                ctx.write_str("ADD COLUMN ");
                if *if_not_exists {
                    ctx.write_str("IF NOT EXISTS ");
                }
                column.restore(ctx)
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

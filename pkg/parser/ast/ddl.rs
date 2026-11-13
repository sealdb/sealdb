use super::dml::{SelectStmt, SetOprStmt};
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TableName {
    pub schema: Option<String>,
    pub name: String,
    /// Physical table name when `name` is an alias (`t t1` → name=t1, physical=t).
    pub physical: Option<String>,
    /// `USE INDEX` / `FORCE INDEX` names (TiDB index hints on the table factor).
    pub use_indexes: Vec<String>,
    /// `IGNORE INDEX` names.
    pub ignore_indexes: Vec<String>,
    /// `FROM t PARTITION (p2, p1)` name filter (definition order after resolve).
    pub partition_names: Vec<String>,
    /// `FROM t AS OF TIMESTAMP <expr>` stale read: raw expression text.
    pub as_of_ts: Option<String>,
}

impl TableName {
    pub fn new(schema: Option<String>, name: impl Into<String>) -> Self {
        Self {
            schema,
            name: name.into(),
            physical: None,
            use_indexes: Vec::new(),
            ignore_indexes: Vec::new(),
            partition_names: Vec::new(),
            as_of_ts: None,
        }
    }

    /// Name used for infoschema / storage lookup.
    pub fn resolve_name(&self) -> &str {
        self.physical.as_deref().unwrap_or(&self.name)
    }

    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(db) = ctx.view_db().map(str::to_string) {
            // TiDB view restore always qualifies the table and prints the
            // physical name followed by the user alias.
            let schema = self.schema.as_deref().unwrap_or(&db);
            ctx.write_name(schema);
            ctx.write_str(".");
            match &self.physical {
                Some(physical) => {
                    ctx.write_name(physical);
                    ctx.write_str(" AS ");
                    ctx.write_name(&self.name);
                }
                None => ctx.write_name(&self.name),
            }
            return Ok(());
        }
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
        (schema, self.resolve_name().to_string())
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
    /// `DEFAULT <expr>` kept as restored SQL text (evaluated on insert /
    /// `ALTER TABLE ADD COLUMN` backfill).
    pub default: Option<String>,
    /// `GENERATED ALWAYS AS (expr) [VIRTUAL|STORED]`; the expression is kept
    /// as restored SQL text so ColumnDef stays Eq/PartialEq.
    pub generated: Option<GeneratedColumn>,
    /// `AUTO_INCREMENT` column option (MySQL/TiDB).
    pub auto_increment: bool,
    /// TiDB `AUTO_RANDOM[(shard_bits[, range_bits])]`: the number of high bits
    /// XOR-shuffled with a random value (`None` = not an auto-random column).
    pub auto_random: Option<u32>,
    /// Explicit `NOT NULL` (or PRIMARY KEY implied) column option.
    pub not_null: bool,
    /// Column `CHARACTER SET <name>` (TiDB `FieldType.Charset`).
    pub charset: Option<String>,
    /// Column `COLLATE <name>` (TiDB `FieldType.Collate`).
    pub collate: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedColumn {
    pub expr: String,
    pub stored: bool,
}

impl ColumnDef {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_name(&self.name);
        ctx.write_str(" ");
        ctx.write_str(&self.type_name);
        if let Some(charset) = &self.charset {
            ctx.write_str(" CHARACTER SET ");
            ctx.write_str(charset);
        }
        if let Some(collate) = &self.collate {
            ctx.write_str(" COLLATE ");
            ctx.write_str(collate);
        }
        if let Some(default) = &self.default {
            ctx.write_str(" DEFAULT ");
            ctx.write_str(default);
        }
        if let Some(g) = &self.generated {
            ctx.write_str(" GENERATED ALWAYS AS (");
            ctx.write_str(&g.expr);
            ctx.write_str(if g.stored { ") STORED" } else { ") VIRTUAL" });
        }
        if self.not_null {
            ctx.write_str(" NOT NULL");
        }
        if self.auto_increment {
            ctx.write_str(" AUTO_INCREMENT");
        }
        if let Some(bits) = self.auto_random {
            // TiDB renders the option inside a `T` version comment so MySQL
            // clients ignore it.
            ctx.write_str(" /*T![auto_rand] AUTO_RANDOM(");
            ctx.write_str(&bits.to_string());
            ctx.write_str(") */");
        }
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

/// One key part of an index: a plain column or a TiDB expression index part
/// (`KEY idx ((`a` + `b` + 1))`, `UNIQUE KEY uk ((tidb_shard(`a`)), a)`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexPart {
    Column(String),
    /// MySQL prefix index part (`KEY idx (b(10))`): only the first `length`
    /// bytes of the column take part in the key (`model.IndexColumn.Length`).
    Prefix {
        name: String,
        length: i32,
    },
    /// TiDB expression index part. The text is `field.Text()` spelling, which
    /// is what `SHOW CREATE TABLE` / `information_schema.tidb_indexes` render.
    Expression(String),
}

impl IndexPart {
    pub fn column(name: impl Into<String>) -> Self {
        Self::Column(name.into())
    }

    pub fn prefix(name: impl Into<String>, length: i32) -> Self {
        Self::Prefix {
            name: name.into(),
            length,
        }
    }

    pub fn expression(text: impl Into<String>) -> Self {
        Self::Expression(text.into())
    }

    /// Column name for a plain (or prefix) part, `None` for an expression part.
    pub fn column_name(&self) -> Option<&str> {
        match self {
            Self::Column(name) | Self::Prefix { name, .. } => Some(name),
            Self::Expression(_) => None,
        }
    }

    /// Prefix length of a `col(N)` key part (`-1` for a full-column part).
    pub fn prefix_length(&self) -> i32 {
        match self {
            Self::Prefix { length, .. } => *length,
            _ => -1,
        }
    }

    /// Expression text for an expression part.
    pub fn as_expression(&self) -> Option<&str> {
        match self {
            Self::Column(_) | Self::Prefix { .. } => None,
            Self::Expression(text) => Some(text),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<IndexPart>,
    pub unique: bool,
    pub primary: bool,
    /// TiDB `GLOBAL` index option on a partitioned table: the index spans every
    /// partition instead of being partitioned alongside the table.
    pub global: bool,
    /// TiDB `PRIMARY KEY (...) NONCLUSTERED`: the PK is a secondary index.
    pub nonclustered: bool,
    /// TiDB `PRIMARY KEY (...) CLUSTERED`: the PK is the row handle even when
    /// `tidb_enable_clustered_index` would otherwise keep it secondary.
    pub clustered: bool,
    /// TiDB `INVISIBLE` index option: the optimizer ignores the index unless a
    /// `USE INDEX` / `FORCE INDEX` hint names it.
    pub invisible: bool,
}

/// `FOREIGN KEY (a, b) REFERENCES t (c, d) ON UPDATE … ON DELETE …`.
///
/// Only the cascade flag of each referential action is kept: TiDB's
/// `Foreign_Key_Check` / `Foreign_Key_Cascade` planning distinguishes CASCADE
/// from the RESTRICT-like default, which is all EXPLAIN renders.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ForeignKeyDef {
    /// Constraint name; MySQL generates `<table>_ibfk_<n>` when omitted.
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    /// Referenced columns; empty means the referenced table's PRIMARY KEY.
    pub ref_columns: Vec<String>,
    pub on_update_cascade: bool,
    pub on_delete_cascade: bool,
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
            match col {
                IndexPart::Column(name) => ctx.write_name(name),
                IndexPart::Prefix { name, length } => {
                    ctx.write_name(name);
                    ctx.write_str("(");
                    ctx.write_str(&length.to_string());
                    ctx.write_str(")");
                }
                // TiDB wraps an expression index part in an extra paren pair:
                // `KEY idx ((`a` + 1))`.
                IndexPart::Expression(text) => {
                    ctx.write_str("(");
                    ctx.write_str(text);
                    ctx.write_str(")");
                }
            }
        }
        ctx.write_str(")");
        // TiDB `SHOW CREATE TABLE` keeps the GLOBAL option behind a version
        // comment so MySQL clients ignore it.
        if self.global {
            ctx.write_str(" /*T![global_index] GLOBAL */");
        }
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
    /// RANGE bound as written (integer or quoted string); `None` means
    /// MAXVALUE. Unused for HASH/KEY/LIST.
    pub less_than: Option<String>,
    /// RANGE COLUMNS extra bounds after the first `less_than` (`None` = MAXVALUE).
    pub extra_bounds: Vec<Option<String>>,
    /// LIST `VALUES IN (...)` integers.
    pub in_values: Vec<Option<i64>>,
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

/// TiDB table TTL option (`TTL = col + INTERVAL 1 HOUR`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtlInfo {
    /// Normalized Explain/SHOW form: `` `created_time` + INTERVAL 1 HOUR ``.
    pub expr: String,
    pub enable: bool,
    pub job_interval: String,
}

impl TtlInfo {
    pub fn new(expr: impl Into<String>) -> Self {
        Self {
            expr: expr.into(),
            enable: true,
            job_interval: "1h".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateTableStmt {
    pub if_not_exists: bool,
    pub table: TableName,
    pub cols: Vec<ColumnDef>,
    pub indices: Vec<IndexDef>,
    pub partition: Option<PartitionOption>,
    /// TiDB `TTL = …` table option.
    pub ttl: Option<TtlInfo>,
    /// `FOREIGN KEY` constraints in definition order (TiDB `TableInfo.ForeignKeys`).
    pub foreign_keys: Vec<ForeignKeyDef>,
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
                        match &def.less_than {
                            Some(v) => write_partition_bound(ctx, v),
                            None => ctx.write_str("MAXVALUE"),
                        }
                        for bound in &def.extra_bounds {
                            ctx.write_str(", ");
                            match bound {
                                Some(v) => write_partition_bound(ctx, v),
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
                            match v {
                                Some(v) => ctx.write_str(&v.to_string()),
                                None => ctx.write_str("NULL"),
                            }
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
    /// TiDB `IsView`: `DROP VIEW` vs `DROP TABLE`.
    pub is_view: bool,
}

impl DropTableStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if self.is_view {
            ctx.write_str("DROP VIEW ");
        } else {
            ctx.write_str("DROP TABLE ");
        }
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

/// Sequence options shared by CREATE / ALTER SEQUENCE.
#[derive(Clone, Debug, Default)]
pub struct SequenceOptionSet {
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
    pub no_min: bool,
    pub no_max: bool,
    pub no_cache: bool,
}

/// TiDB `ast.CreateSequenceStmt`.
#[derive(Clone, Debug)]
pub struct CreateSequenceStmt {
    pub if_not_exists: bool,
    pub name: TableName,
    pub options: SequenceOptionSet,
}

impl CreateSequenceStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE SEQUENCE ");
        if self.if_not_exists {
            ctx.write_str("IF NOT EXISTS ");
        }
        self.name.restore(ctx)?;
        restore_seq_options(&self.options, ctx);
        Ok(())
    }
}

/// TiDB `ast.DropSequenceStmt`.
#[derive(Clone, Debug)]
pub struct DropSequenceStmt {
    pub if_exists: bool,
    pub sequences: Vec<TableName>,
}

impl DropSequenceStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DROP SEQUENCE ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        for (i, name) in self.sequences.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            name.restore(ctx)?;
        }
        Ok(())
    }
}

/// TiDB `ast.AlterSequenceStmt`.
#[derive(Clone, Debug)]
pub struct AlterSequenceStmt {
    pub if_exists: bool,
    pub name: TableName,
    pub options: SequenceOptionSet,
}

impl AlterSequenceStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("ALTER SEQUENCE ");
        if self.if_exists {
            ctx.write_str("IF EXISTS ");
        }
        self.name.restore(ctx)?;
        restore_seq_options(&self.options, ctx);
        Ok(())
    }
}

fn restore_seq_options(opts: &SequenceOptionSet, ctx: &mut RestoreCtx) {
    if let Some(v) = opts.increment {
        ctx.write_str(&format!(" INCREMENT BY {v}"));
    }
    if opts.no_min {
        ctx.write_str(" NO MINVALUE");
    } else if let Some(v) = opts.min_value {
        ctx.write_str(&format!(" MINVALUE {v}"));
    }
    if opts.no_max {
        ctx.write_str(" NO MAXVALUE");
    } else if let Some(v) = opts.max_value {
        ctx.write_str(&format!(" MAXVALUE {v}"));
    }
    if let Some(v) = opts.start {
        ctx.write_str(&format!(" START WITH {v}"));
    }
    if opts.no_cache {
        ctx.write_str(" NOCACHE");
    } else if let Some(v) = opts.cache {
        ctx.write_str(&format!(" CACHE {v}"));
    }
    if let Some(cycle) = opts.cycle {
        ctx.write_str(if cycle { " CYCLE" } else { " NOCYCLE" });
    }
}

/// TiDB `ast.CreateViewStmt`.
#[derive(Clone, Debug)]
pub struct CreateViewStmt {
    pub or_replace: bool,
    pub view_name: TableName,
    pub cols: Vec<String>,
    pub select: Box<SelectStmt>,
    /// Set operation body for `CREATE VIEW ... AS SELECT ... UNION ...`.
    /// `select` then holds the first arm (view column names / types come from it).
    pub select_set: Option<Box<SetOprStmt>>,
    pub algorithm: String,
    pub definer: String,
    pub security: String,
    pub check_option: String,
}

impl CreateViewStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CREATE ");
        if self.or_replace {
            ctx.write_str("OR REPLACE ");
        }
        ctx.write_str("ALGORITHM = ");
        ctx.write_str(&self.algorithm);
        ctx.write_str(" DEFINER = ");
        ctx.write_str(&self.definer);
        ctx.write_str(" SQL SECURITY ");
        ctx.write_str(&self.security);
        ctx.write_str(" VIEW ");
        self.view_name.restore(ctx)?;
        if !self.cols.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.cols.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                ctx.write_name(col);
            }
            ctx.write_str(")");
        }
        ctx.write_str(" AS ");
        if let Some(set) = &self.select_set {
            set.restore(ctx)?;
        } else {
            self.select.restore(ctx)?;
        }
        if !self.check_option.is_empty() && !self.check_option.eq_ignore_ascii_case("NONE") {
            ctx.write_str(" WITH ");
            if !self.check_option.eq_ignore_ascii_case("CASCADED") {
                ctx.write_str(&self.check_option);
                ctx.write_str(" ");
            }
            ctx.write_str("CHECK OPTION");
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
    pub columns: Vec<IndexPart>,
    pub unique: bool,
    /// TiDB `INVISIBLE`: the optimizer ignores the new index.
    pub invisible: bool,
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
            match col {
                IndexPart::Column(name) => ctx.write_name(name),
                IndexPart::Prefix { name, length } => {
                    ctx.write_name(name);
                    ctx.write_str("(");
                    ctx.write_str(&length.to_string());
                    ctx.write_str(")");
                }
                IndexPart::Expression(text) => {
                    ctx.write_str("(");
                    ctx.write_str(text);
                    ctx.write_str(")");
                }
            }
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
        /// `DEFAULT <expr>` given with the new definition.
        default: Option<String>,
        /// `GENERATED ALWAYS AS (expr)` given with the new definition.
        generated: Option<GeneratedColumn>,
        position: ColumnPosition,
    },
    /// `MODIFY [COLUMN] col type` — keep the name, change the type.
    ModifyColumn {
        name: String,
        type_name: String,
        /// `DEFAULT <expr>` given with the new definition.
        default: Option<String>,
        /// `GENERATED ALWAYS AS (expr)` given with the new definition.
        generated: Option<GeneratedColumn>,
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
    /// `ALTER TABLE t ALTER INDEX name [IN]VISIBLE`.
    AlterIndexVisibility {
        name: String,
        invisible: bool,
    },
    /// `ALGORITHM [=] DEFAULT|INSTANT|INPLACE|COPY` — accepted and ignored.
    Algorithm(String),
    /// `LOCK [=] DEFAULT|NONE|SHARED|EXCLUSIVE` — accepted and ignored.
    Lock(String),
    /// `ENGINE = InnoDB` / `ROW_FORMAT = DYNAMIC` table option in an
    /// `ALTER TABLE` spec list — accepted and ignored (storage-level only).
    TableOption {
        name: String,
        value: String,
    },
    /// `ADD PARTITION (PARTITION ...)` for RANGE/LIST tables.
    AddPartition {
        defs: Vec<PartitionDef>,
    },
    /// `DROP PARTITION p0 [, p1 ...]`.
    DropPartition {
        if_exists: bool,
        names: Vec<String>,
    },
    /// `EXCHANGE PARTITION p WITH TABLE t`.
    ExchangePartition {
        partition: String,
        table: TableName,
    },
    /// `TTL = <expr>` / `TTL_ENABLE = 'ON'|'OFF'` / `TTL_JOB_INTERVAL = '1d'`
    /// (`AlterTTLInfo`).
    SetTtl {
        /// `TTL = <expr>` (`None` for an ENABLE / JOB_INTERVAL change).
        expr: Option<String>,
        enable: Option<bool>,
        job_interval: Option<String>,
    },
    /// `REMOVE TTL`.
    RemoveTtl,
    /// `ALTER [COLUMN] col SET DEFAULT <expr>` / `ALTER [COLUMN] col DROP DEFAULT`.
    AlterColumnDefault {
        name: String,
        /// The new default literal, `None` for `DROP DEFAULT`.
        default: Option<String>,
    },
    /// `ADD [CONSTRAINT name] FOREIGN KEY (cols) REFERENCES t (cols)` — only the
    /// referenced table is validated (TTL conflict); enforcement lands later.
    AddForeignKey {
        ref_table: String,
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
                default,
                generated,
                position,
            } => {
                ctx.write_str("CHANGE COLUMN ");
                ctx.write_name(old_name);
                ctx.write_str(" ");
                ctx.write_name(new_name);
                ctx.write_str(" ");
                ctx.write_str(type_name);
                if let Some(default) = default {
                    ctx.write_str(" DEFAULT ");
                    ctx.write_str(default);
                }
                if let Some(generated) = generated {
                    ctx.write_str(" GENERATED ALWAYS AS (");
                    ctx.write_str(&generated.expr);
                    ctx.write_str(") ");
                    ctx.write_str(if generated.stored {
                        "STORED"
                    } else {
                        "VIRTUAL"
                    });
                }
                position.restore(ctx)
            }
            AlterTableSpec::ModifyColumn {
                name,
                type_name,
                default,
                generated,
                position,
            } => {
                ctx.write_str("MODIFY COLUMN ");
                ctx.write_name(name);
                ctx.write_str(" ");
                ctx.write_str(type_name);
                if let Some(default) = default {
                    ctx.write_str(" DEFAULT ");
                    ctx.write_str(default);
                }
                if let Some(generated) = generated {
                    ctx.write_str(" GENERATED ALWAYS AS (");
                    ctx.write_str(&generated.expr);
                    ctx.write_str(") ");
                    ctx.write_str(if generated.stored {
                        "STORED"
                    } else {
                        "VIRTUAL"
                    });
                }
                position.restore(ctx)
            }
            AlterTableSpec::AlterColumnDefault { name, default } => {
                ctx.write_str("ALTER COLUMN ");
                ctx.write_name(name);
                match default {
                    Some(default) => {
                        ctx.write_str(" SET DEFAULT ");
                        ctx.write_str(default);
                    }
                    None => ctx.write_str(" DROP DEFAULT"),
                }
                Ok(())
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
            AlterTableSpec::AlterIndexVisibility { name, invisible } => {
                ctx.write_str("ALTER INDEX ");
                ctx.write_name(name);
                ctx.write_str(if *invisible { " INVISIBLE" } else { " VISIBLE" });
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
            AlterTableSpec::TableOption { name, value } => {
                ctx.write_str(name);
                ctx.write_str("=");
                ctx.write_str(value);
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
                            match v {
                                Some(v) => ctx.write_str(&v.to_string()),
                                None => ctx.write_str("NULL"),
                            }
                        }
                        ctx.write_str(")");
                    } else {
                        ctx.write_str(" VALUES LESS THAN (");
                        match &def.less_than {
                            Some(v) => write_partition_bound(ctx, v),
                            None => ctx.write_str("MAXVALUE"),
                        }
                        for bound in &def.extra_bounds {
                            ctx.write_str(", ");
                            match bound {
                                Some(v) => write_partition_bound(ctx, v),
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
            AlterTableSpec::ExchangePartition { partition, table } => {
                ctx.write_str("EXCHANGE PARTITION ");
                ctx.write_name(partition);
                ctx.write_str(" WITH TABLE ");
                table.restore(ctx)
            }
            AlterTableSpec::SetTtl {
                expr,
                enable,
                job_interval,
            } => {
                if let Some(expr) = expr {
                    ctx.write_str("TTL = ");
                    ctx.write_str(expr);
                    if enable.is_some() || job_interval.is_some() {
                        ctx.write_str(" ");
                    }
                }
                if let Some(enable) = enable {
                    ctx.write_str("TTL_ENABLE = '");
                    ctx.write_str(if *enable { "ON" } else { "OFF" });
                    ctx.write_str("'");
                    if job_interval.is_some() {
                        ctx.write_str(" ");
                    }
                }
                if let Some(interval) = job_interval {
                    ctx.write_str("TTL_JOB_INTERVAL = '");
                    ctx.write_str(interval);
                    ctx.write_str("'");
                }
                Ok(())
            }
            AlterTableSpec::RemoveTtl => {
                ctx.write_str("REMOVE TTL");
                Ok(())
            }
            AlterTableSpec::AddForeignKey { ref_table } => {
                ctx.write_str("ADD FOREIGN KEY REFERENCES ");
                ctx.write_name(ref_table);
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

/// RANGE bound text: integers stay bare, literal (string/temporal) bounds are
/// re-quoted the way TiDB prints them (`VALUES LESS THAN ("2020-01-01 00:00:00")`).
fn write_partition_bound(ctx: &mut RestoreCtx, bound: &str) {
    if bound.parse::<i64>().is_ok() || bound.parse::<f64>().is_ok() {
        ctx.write_str(bound);
    } else {
        ctx.write_str("\"");
        ctx.write_str(bound);
        ctx.write_str("\"");
    }
}

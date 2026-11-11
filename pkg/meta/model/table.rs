use super::column::ColumnInfo;
use super::index::IndexInfo;
use super::job::{STATE_PUBLIC, SchemaState};
use super::partition::PartitionInfo;
use sealdb_parser::model::CIStr;
use sealdb_types::{EvalType, FieldType};
use serde::{Deserialize, Serialize};
use std::cell::Cell;

pub const CURR_LATEST_TABLE_INFO_VERSION: u16 = 5;

/// TiDB `tidb_enable_clustered_index` (default `ON`). Decides, at CREATE TABLE
/// time, whether a PRIMARY KEY that names no `CLUSTERED` / `NONCLUSTERED`
/// keyword becomes the row handle:
/// * `ON` — every PRIMARY KEY is clustered (integer ones as `pk_is_handle`,
///   composite / string ones as a common handle).
/// * `INT_ONLY` — only single integer PRIMARY KEYs are clustered handles; other
///   PRIMARY KEYs stay secondary unique indexes.
/// * `OFF` — no PRIMARY KEY is clustered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClusteredIndexMode {
    #[default]
    On,
    IntOnly,
    Off,
}

impl ClusteredIndexMode {
    /// TiDB `GetClusteredIndexMode`-style parse of the variable literal.
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_uppercase()
            .as_str()
        {
            "ON" | "1" | "TRUE" | "DEFAULT" => Some(Self::On),
            "INT_ONLY" => Some(Self::IntOnly),
            "OFF" | "0" | "FALSE" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "ON",
            Self::IntOnly => "INT_ONLY",
            Self::Off => "OFF",
        }
    }

    /// Whether a single integer PRIMARY KEY becomes the integer row handle.
    pub fn clusters_int_primary(self) -> bool {
        matches!(self, Self::On | Self::IntOnly)
    }

    /// Whether a non-integer (composite / string) PRIMARY KEY becomes a common
    /// handle.
    pub fn clusters_common_primary(self) -> bool {
        matches!(self, Self::On)
    }
}

thread_local! {
    /// DDL runs on the session thread; the session installs its
    /// `tidb_enable_clustered_index` here for the duration of the statement so
    /// `BuildTableInfo` need not thread session state through the DDL layer.
    static CLUSTERED_INDEX_MODE: Cell<ClusteredIndexMode> =
        const { Cell::new(ClusteredIndexMode::On) };
}

pub fn clustered_index_mode() -> ClusteredIndexMode {
    CLUSTERED_INDEX_MODE.with(|mode| mode.get())
}

/// Runs `f` with `tidb_enable_clustered_index` overridden, restoring the
/// previous value afterwards.
pub fn with_clustered_index_mode<T>(mode: ClusteredIndexMode, f: impl FnOnce() -> T) -> T {
    let previous = CLUSTERED_INDEX_MODE.with(|cell| cell.replace(mode));
    let out = f();
    CLUSTERED_INDEX_MODE.with(|cell| cell.set(previous));
    out
}

/// TiDB `model.SequenceInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SequenceInfo {
    pub start: i64,
    pub cache: bool,
    pub cycle: bool,
    pub min_value: i64,
    pub max_value: i64,
    pub increment: i64,
    pub cache_value: i64,
    #[serde(default)]
    pub comment: String,
    /// Next value to return from NEXTVAL (base + offset style).
    #[serde(default)]
    pub next_value: i64,
}

impl Default for SequenceInfo {
    fn default() -> Self {
        Self {
            start: 1,
            cache: true,
            cycle: false,
            min_value: 1,
            max_value: 9223372036854775806,
            increment: 1,
            cache_value: 1000,
            comment: String::new(),
            next_value: 1,
        }
    }
}

impl SequenceInfo {
    pub fn with_defaults_for_increment(increment: i64) -> Self {
        let increment = if increment == 0 { 1 } else { increment };
        let (start, min_value, max_value) = if increment > 0 {
            (1, 1, 9223372036854775806)
        } else {
            (-1, -9223372036854775807, -1)
        };
        Self {
            start,
            min_value,
            max_value,
            increment,
            next_value: start,
            ..Self::default()
        }
    }

    /// Allocate and return the next sequence value.
    pub fn next_val(&mut self) -> Result<i64, String> {
        let cur = self.next_value;
        if self.increment > 0 {
            if cur > self.max_value {
                if self.cycle {
                    self.next_value = self.min_value;
                    let out = self.next_value;
                    self.next_value = self.next_value.saturating_add(self.increment);
                    return Ok(out);
                }
                return Err("Sequence has run out".to_string());
            }
        } else if cur < self.min_value {
            if self.cycle {
                self.next_value = self.max_value;
                let out = self.next_value;
                self.next_value = self.next_value.saturating_add(self.increment);
                return Ok(out);
            }
            return Err("Sequence has run out".to_string());
        }
        self.next_value = cur.saturating_add(self.increment);
        Ok(cur)
    }
}

/// TiDB `model.ViewInfo` — metadata for a VIEW (select text + security attrs).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewInfo {
    #[serde(default = "default_view_algorithm")]
    pub algorithm: String,
    #[serde(default)]
    pub definer: String,
    #[serde(default = "default_view_security")]
    pub security: String,
    pub select_stmt: String,
    #[serde(default = "default_view_check_option")]
    pub check_option: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cols: Vec<CIStr>,
}

fn default_view_algorithm() -> String {
    "UNDEFINED".to_string()
}

fn default_view_security() -> String {
    "DEFINER".to_string()
}

fn default_view_check_option() -> String {
    "NONE".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableInfo {
    pub id: i64,
    pub name: CIStr,
    pub charset: String,
    pub collate: String,
    #[serde(rename = "cols")]
    pub columns: Vec<ColumnInfo>,
    #[serde(rename = "index_info", default, skip_serializing_if = "Vec::is_empty")]
    pub indices: Vec<IndexInfo>,
    /// TiDB `TableInfo.ForeignKeys`: `FOREIGN KEY` constraints in definition
    /// order. The matching local index is created automatically at DDL time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub state: SchemaState,
    pub pk_is_handle: bool,
    /// Clustered PRIMARY KEY that is not a single integer column: the handle is
    /// the encoded PK prefix (TiDB `TableInfo.CommonHandle`). Secondary index
    /// keys then carry the PK columns, so they cover them.
    #[serde(default)]
    pub common_handle: bool,
    #[serde(default)]
    pub version: u16,
    #[serde(default)]
    pub max_col_id: i64,
    #[serde(default)]
    pub max_idx_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<PartitionInfo>,
    /// TiDB TTL table option (SHOW CREATE /*T![ttl] … */).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<TtlInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<SequenceInfo>,
}

/// Persisted TTL metadata (TiDB `TTLInfo`).
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct TtlInfo {
    pub expr: String,
    pub enable: bool,
    pub job_interval: String,
}

/// `FOREIGN KEY` constraint metadata (TiDB `FKInfo`).
///
/// Only CASCADE is distinguished from the RESTRICT-like default: that is the
/// distinction TiDB's `Foreign_Key_Check` / `Foreign_Key_Cascade` planning
/// (and therefore EXPLAIN) makes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct ForeignKeyInfo {
    /// Constraint name (`fk_1`); MySQL generates `<table>_ibfk_<n>` when the
    /// DDL omits it.
    pub name: String,
    /// Local columns, in declaration order.
    pub cols: Vec<String>,
    /// Referenced table (unqualified; TiDB rejects cross-schema references).
    pub ref_table: String,
    /// Referenced columns; empty means the referenced table's PRIMARY KEY.
    pub ref_cols: Vec<String>,
    #[serde(default)]
    pub on_update_cascade: bool,
    #[serde(default)]
    pub on_delete_cascade: bool,
}

impl TableInfo {
    pub fn new(id: i64, name: impl Into<String>, columns: Vec<ColumnInfo>) -> Self {
        let max_col_id = columns.iter().map(|c| c.id).max().unwrap_or(0);
        Self {
            id,
            name: CIStr::new(name),
            charset: "utf8mb4".to_string(),
            collate: "utf8mb4_bin".to_string(),
            columns,
            indices: Vec::new(),
            foreign_keys: Vec::new(),
            state: STATE_PUBLIC,
            pk_is_handle: false,
            common_handle: false,
            version: CURR_LATEST_TABLE_INFO_VERSION,
            max_col_id,
            max_idx_id: 0,
            partition: None,
            ttl: None,
            view: None,
            sequence: None,
        }
    }

    /// Clustered PRIMARY KEY: integer handle or common handle.
    pub fn pk_is_clustered(&self) -> bool {
        self.pk_is_handle || self.common_handle
    }

    /// Leading column of a clustered PRIMARY KEY (the seek prefix of a
    /// `TableRangeScan` on a common handle).
    pub fn clustered_prefix_column(&self) -> Option<&ColumnInfo> {
        if !self.pk_is_clustered() {
            return None;
        }
        let pk = self.indices.iter().find(|idx| idx.primary)?;
        let first = pk.columns.first()?;
        self.columns
            .iter()
            .find(|col| col.name.l() == first.name.l())
    }

    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    pub fn is_sequence(&self) -> bool {
        self.sequence.is_some()
    }

    pub fn find_index_covering(&self, column: &str) -> Option<&IndexInfo> {
        let name = column.to_ascii_lowercase();
        self.indices.iter().find(|idx| {
            // Clustered integer PRIMARY KEY is the row handle, not an index path.
            !(idx.primary && self.pk_is_handle)
                && !idx.invisible
                && idx.columns.first().is_some_and(|col| col.name.l() == name)
        })
    }

    /// Unique index whose only column is `column` — the point-get access path
    /// TiDB prefers over a composite index sharing that leading column.
    pub fn find_unique_index_on(&self, column: &str) -> Option<&IndexInfo> {
        let name = column.to_ascii_lowercase();
        self.indices.iter().find(|idx| {
            !(idx.primary && self.pk_is_handle)
                && !idx.invisible
                && idx.unique
                && idx.columns.len() == 1
                && idx.columns[0].name.l() == name
        })
    }

    /// `(col_id, FieldType)` pairs for v128 row decode (no type tags in the data).
    pub fn col_field_types(&self) -> Vec<(i64, FieldType)> {
        self.columns
            .iter()
            .map(|c| (c.id, c.field_type.clone()))
            .collect()
    }

    /// Integer clustered PRIMARY KEY column (`pk_is_handle`). Prefer the
    /// PRIMARY KEY index column; fall back to first INT for legacy tables.
    pub fn handle_column(&self) -> Option<&ColumnInfo> {
        if self.pk_is_handle
            && let Some(pk) = self.indices.iter().find(|idx| idx.primary)
            && let Some(col) = pk.columns.first()
        {
            return self
                .columns
                .iter()
                .find(|c| c.offset == col.offset)
                .filter(|c| c.field_type.eval_type() == EvalType::Int);
        }
        self.columns
            .iter()
            .find(|col| col.id == 1 && col.field_type.eval_type() == EvalType::Int)
    }

    pub fn covering_column_names(&self, index: &IndexInfo) -> Vec<String> {
        let mut names: Vec<String> = index
            .columns
            .iter()
            .map(|col| col.name.o().to_string())
            .collect();
        // Only an integer PRIMARY KEY handle is free with the index key.
        // Non-PK tables use `_tidb_rowid`; first INT is not covered by a
        // secondary index (`KEY (b)` + `SELECT *` → IndexLookUp, not IndexReader).
        if self.pk_is_handle
            && let Some(handle) = self.handle_column()
        {
            let handle_name = handle.name.o();
            if !names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(handle_name))
            {
                names.push(handle_name.to_string());
            }
        }
        // A common handle is encoded into every secondary index key, so the PK
        // columns are covered (`KEY idx_9(col_11)` covers `col_14` when the
        // clustered PK starts with `col_14`).
        if self.common_handle
            && let Some(pk) = self.indices.iter().find(|idx| idx.primary)
        {
            for col in &pk.columns {
                let name = col.name.o();
                if !names.iter().any(|have| have.eq_ignore_ascii_case(&name)) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    pub fn index_covers(&self, index: &IndexInfo, columns: &[String]) -> bool {
        let available = self.covering_column_names(index);
        columns.iter().all(|needed| {
            available
                .iter()
                .any(|have| have.eq_ignore_ascii_case(needed))
        })
    }
}

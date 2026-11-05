use super::column::ColumnInfo;
use super::index::IndexInfo;
use super::job::{STATE_PUBLIC, SchemaState};
use super::partition::PartitionInfo;
use sealdb_parser::model::CIStr;
use sealdb_types::{EvalType, FieldType};
use serde::{Deserialize, Serialize};

pub const CURR_LATEST_TABLE_INFO_VERSION: u16 = 5;

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
    pub state: SchemaState,
    pub pk_is_handle: bool,
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
            state: STATE_PUBLIC,
            pk_is_handle: false,
            version: CURR_LATEST_TABLE_INFO_VERSION,
            max_col_id,
            max_idx_id: 0,
            partition: None,
            ttl: None,
            view: None,
            sequence: None,
        }
    }

    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    pub fn is_sequence(&self) -> bool {
        self.sequence.is_some()
    }

    pub fn find_index_covering(&self, column: &str) -> Option<&IndexInfo> {
        let name = column.to_ascii_lowercase();
        self.indices
            .iter()
            .find(|idx| idx.columns.first().is_some_and(|col| col.name.l() == name))
    }

    /// `(col_id, FieldType)` pairs for v128 row decode (no type tags in the data).
    pub fn col_field_types(&self) -> Vec<(i64, FieldType)> {
        self.columns
            .iter()
            .map(|c| (c.id, c.field_type.clone()))
            .collect()
    }

    /// Integer handle column is the original first INT (id == 1), so AFTER/FIRST
    /// reordering does not steal the row key. Used for covering-index completeness
    /// (`pk_is_handle` / common-handle heuristics); decode must not overwrite
    /// values already filled from the index key (see `covering_row`).
    pub fn handle_column(&self) -> Option<&ColumnInfo> {
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

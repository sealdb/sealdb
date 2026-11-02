use super::column::ColumnInfo;
use super::index::IndexInfo;
use super::job::{STATE_PUBLIC, SchemaState};
use super::partition::PartitionInfo;
use sealdb_parser::model::CIStr;
use sealdb_types::{EvalType, FieldType};
use serde::{Deserialize, Serialize};

pub const CURR_LATEST_TABLE_INFO_VERSION: u16 = 5;

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
        }
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
    /// reordering does not steal the row key.
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
        if let Some(handle) = self.handle_column() {
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

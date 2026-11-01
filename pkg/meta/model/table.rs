use super::column::ColumnInfo;
use super::job::{STATE_PUBLIC, SchemaState};
use sealdb_parser::model::CIStr;
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
    pub indices: Vec<serde_json::Value>,
    pub state: SchemaState,
    pub pk_is_handle: bool,
    #[serde(default)]
    pub version: u16,
    #[serde(default)]
    pub max_col_id: i64,
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
        }
    }
}

use super::job::{STATE_PUBLIC, SchemaState};
use sealdb_parser::model::CIStr;
use sealdb_types::FieldType;
use serde::{Deserialize, Serialize};

pub const CURR_LATEST_COLUMN_INFO_VERSION: u64 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub id: i64,
    pub name: CIStr,
    pub offset: i32,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub state: SchemaState,
    pub version: u64,
    /// `GENERATED ALWAYS AS (expr)` body, restored as SQL text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_expr: Option<String>,
    #[serde(default)]
    pub generated_stored: bool,
}

impl ColumnInfo {
    pub fn new(id: i64, offset: i32, name: impl Into<String>, field_type: FieldType) -> Self {
        Self {
            id,
            name: CIStr::new(name),
            offset,
            field_type,
            state: STATE_PUBLIC,
            version: CURR_LATEST_COLUMN_INFO_VERSION,
            generated_expr: None,
            generated_stored: false,
        }
    }
}

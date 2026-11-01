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
        }
    }
}

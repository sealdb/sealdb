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
    /// `DEFAULT <expr>` body, restored as SQL text.
    #[serde(rename = "default", default, skip_serializing_if = "Option::is_none")]
    pub default_expr: Option<String>,
    /// `AUTO_INCREMENT` column option (TiDB `ColumnInfo.AutoIncrement`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_increment: bool,
    /// TiDB `AUTO_RANDOM(n)` shard bits (`ColumnInfo.AutoRandomBits`; `None`
    /// when the column is not an auto-random column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_random: Option<u32>,
    /// TiDB `ColumnInfo.Hidden`: an internal column backing an expression index
    /// (`_V$_idx_0`). Hidden columns are stored in the row but never shown to
    /// clients (`SELECT *`, `SHOW CREATE TABLE`, `information_schema`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
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
            default_expr: None,
            auto_increment: false,
            auto_random: None,
            hidden: false,
        }
    }
}

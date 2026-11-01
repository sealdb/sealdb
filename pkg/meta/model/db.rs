use super::job::{STATE_PUBLIC, SchemaState};
use sealdb_parser::model::CIStr;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DBInfo {
    pub id: i64,
    #[serde(rename = "db_name")]
    pub name: CIStr,
    pub charset: String,
    pub collate: String,
    pub state: SchemaState,
}

impl DBInfo {
    pub fn new(id: i64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: CIStr::new(name),
            charset: "utf8mb4".to_string(),
            collate: "utf8mb4_bin".to_string(),
            state: STATE_PUBLIC,
        }
    }
}

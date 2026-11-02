use super::job::{STATE_PUBLIC, SchemaState};
use sealdb_parser::model::CIStr;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexColumn {
    pub name: CIStr,
    pub offset: i32,
    /// UnspecifiedLength (-1) when the full column is indexed.
    /// Prefix indexes stay out of F10.
    #[serde(default = "unspecified_length")]
    pub length: i32,
}

fn unspecified_length() -> i32 {
    -1
}

impl IndexColumn {
    pub fn new(name: impl Into<String>, offset: i32) -> Self {
        Self {
            name: CIStr::new(name),
            offset,
            length: -1,
        }
    }
}

/// TiDB `model.IndexInfo` core fields used by F10.
/// Completeness wave / not this feature: invisible, global, MV, vector,
/// partial. ADD INDEX walks SchemaState via ddl/reorg.rs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexInfo {
    pub id: i64,
    #[serde(rename = "idx_name")]
    pub name: CIStr,
    #[serde(rename = "tbl_name")]
    pub table: CIStr,
    #[serde(rename = "idx_cols")]
    pub columns: Vec<IndexColumn>,
    pub state: SchemaState,
    #[serde(rename = "is_unique", default)]
    pub unique: bool,
    #[serde(rename = "is_primary", default)]
    pub primary: bool,
}

impl IndexInfo {
    pub fn new(
        id: i64,
        name: impl Into<String>,
        table: impl Into<String>,
        columns: Vec<IndexColumn>,
    ) -> Self {
        Self {
            id,
            name: CIStr::new(name),
            table: CIStr::new(table),
            columns,
            state: STATE_PUBLIC,
            unique: false,
            primary: false,
        }
    }

    pub fn with_flags(mut self, unique: bool, primary: bool) -> Self {
        self.unique = unique || primary;
        self.primary = primary;
        self
    }
}

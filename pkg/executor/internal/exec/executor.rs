use sealdb_errno::Result;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub trait Executor {
    fn open(&mut self) -> Result<()>;
    fn next(&mut self) -> Result<Option<Chunk>>;
    fn close(&mut self) -> Result<()>;
    fn schema(&self) -> &[String];
}

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Datum>>,
}

impl QueryResult {
    pub fn typed_json(&self) -> serde_json::Value {
        serde_json::json!({
            "columns": self.columns,
            "rows": self.rows.iter().map(|row| {
                row.iter().map(Datum::typed_json).collect::<Vec<_>>()
            }).collect::<Vec<_>>(),
        })
    }

    pub fn display_rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| row.iter().map(Datum::display).collect())
            .collect()
    }
}

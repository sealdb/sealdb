use sealdb_errno::Result;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub trait Executor {
    fn open(&mut self) -> Result<()>;
    fn next(&mut self) -> Result<Option<Chunk>>;
    fn close(&mut self) -> Result<()>;
    fn schema(&self) -> &[String];
}

#[cfg(test)]
pub(crate) struct VecExec {
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

#[cfg(test)]
impl VecExec {
    pub(crate) fn new(schema: Vec<String>, rows: Vec<Vec<Datum>>) -> Self {
        Self {
            schema,
            rows,
            emitted: false,
        }
    }
}

#[cfg(test)]
impl Executor for VecExec {
    fn open(&mut self) -> Result<()> {
        self.emitted = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in &self.rows {
            chunk.append_row(row.clone());
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
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

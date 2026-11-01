// F17 core path: CTEExec for non-recursive WITH (TiDB pkg/executor/cte.go).
// Completeness wave / not this feature: recursive CTE, CTETableReader shared
// storage, multi-consumer reuse, MATERIALIZED, UNION in CTE, CTE in JOIN/DML.
use crate::internal::exec::Executor;
use sealdb_errno::{ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub struct CTEExec {
    columns: Vec<String>,
    seed: Box<dyn Executor>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl CTEExec {
    pub fn new(columns: Vec<String>, seed: Box<dyn Executor>) -> Self {
        Self {
            columns,
            seed,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for CTEExec {
    fn open(&mut self) -> Result<()> {
        self.seed.open()?;
        self.schema = self.seed.schema().to_vec();
        self.rows = collect_rows(&mut self.seed)?;
        if !self.columns.is_empty() {
            let width = self
                .rows
                .first()
                .map(|row| row.len())
                .unwrap_or(self.schema.len());
            if width != 0 && self.columns.len() != width {
                return Err(Error::new(
                    ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT,
                    "The used SELECT statements have a different number of columns",
                ));
            }
            self.schema = self.columns.clone();
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        self.seed.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn collect_rows(exec: &mut Box<dyn Executor>) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = exec.next()? {
        for i in 0..chunk.num_rows() {
            rows.push(chunk.get_row(i));
        }
    }
    Ok(rows)
}

// F15 core path: UnionExec for UNION / UNION ALL (TiDB pkg/executor/unionexec/union.go).
// Completeness wave / not this feature: EXCEPT/INTERSECT, parenthesized nested
// set ops, UNION in subquery, parallel workers.
use crate::internal::exec::Executor;
use sealdb_errno::{ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashSet;

pub struct UnionExec {
    left: Box<dyn Executor>,
    right: Box<dyn Executor>,
    distinct: bool,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl UnionExec {
    pub fn new(left: Box<dyn Executor>, right: Box<dyn Executor>, distinct: bool) -> Self {
        Self {
            left,
            right,
            distinct,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for UnionExec {
    fn open(&mut self) -> Result<()> {
        self.left.open()?;
        self.right.open()?;
        self.schema = self.left.schema().to_vec();
        if self.schema.is_empty() {
            self.schema = self.right.schema().to_vec();
        }
        let left_rows = collect_rows(&mut self.left)?;
        let right_rows = collect_rows(&mut self.right)?;
        let width = left_rows
            .first()
            .or(right_rows.first())
            .map(|row| row.len())
            .unwrap_or(self.schema.len());
        for row in left_rows.iter().chain(right_rows.iter()) {
            if row.len() != width {
                return Err(column_mismatch());
            }
        }
        if self.distinct {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for row in left_rows.into_iter().chain(right_rows) {
                if seen.insert(row_key(&row)) {
                    out.push(row);
                }
            }
            self.rows = out;
        } else {
            let mut rows = left_rows;
            rows.extend(right_rows);
            self.rows = rows;
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
        self.left.close()?;
        self.right.close()
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

fn row_key(row: &[Datum]) -> Vec<String> {
    row.iter().map(Datum::display).collect()
}

fn column_mismatch() -> Error {
    Error::new(
        ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT,
        "The used SELECT statements have a different number of columns",
    )
}

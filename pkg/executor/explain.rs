use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_parser::ast::ExplainFormat;
use sealdb_planner::PhysicalPlan;
use sealdb_planner::core::explain::explain_result;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub struct ExplainExec {
    format: ExplainFormat,
    plan: PhysicalPlan,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl ExplainExec {
    pub fn new(format: ExplainFormat, plan: PhysicalPlan) -> Self {
        Self {
            format,
            plan,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for ExplainExec {
    fn open(&mut self) -> Result<()> {
        let (columns, rows) = explain_result(&self.plan, self.format);
        self.schema = columns;
        self.rows = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|s| Datum::Bytes(s.into_bytes()))
                    .collect()
            })
            .collect();
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
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_parser::ast::ExplainFormat;
use sealdb_planner::PhysicalPlan;
use sealdb_planner::core::explain::{explain_analyze_result, explain_result};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::time::Instant;

pub struct ExplainExec {
    format: ExplainFormat,
    plan: PhysicalPlan,
    analyze: bool,
    child: Option<Box<dyn Executor>>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl ExplainExec {
    pub fn new(format: ExplainFormat, plan: PhysicalPlan) -> Self {
        Self {
            format,
            plan,
            analyze: false,
            child: None,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }

    pub fn with_analyze(
        format: ExplainFormat,
        plan: PhysicalPlan,
        child: Box<dyn Executor>,
    ) -> Self {
        Self {
            format,
            plan,
            analyze: true,
            child: Some(child),
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for ExplainExec {
    fn open(&mut self) -> Result<()> {
        let (columns, rows) = if self.analyze {
            let mut child = self
                .child
                .take()
                .expect("EXPLAIN ANALYZE requires a child executor");
            let start = Instant::now();
            child.open()?;
            let mut act_rows = 0usize;
            while let Some(chunk) = child.next()? {
                act_rows += chunk.num_rows();
            }
            child.close()?;
            explain_analyze_result(&self.plan, self.format, act_rows, start.elapsed())
        } else {
            explain_result(&self.plan, self.format)
        };
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

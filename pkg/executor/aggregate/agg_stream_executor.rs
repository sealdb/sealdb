// StreamAgg for scalar aggregations (no GROUP BY).
// Completeness wave: sorted GROUP BY StreamAgg, pipelined partial aggs.
use super::HashAggExec;
use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_parser::ast::SelectStmt;
use sealdb_util::chunk::Chunk;

pub struct StreamAggExec {
    inner: HashAggExec,
}

impl StreamAggExec {
    pub fn new(select: &SelectStmt, child: Box<dyn Executor>) -> Result<Self> {
        Ok(Self {
            inner: HashAggExec::new(select, child)?,
        })
    }
}

impl Executor for StreamAggExec {
    fn open(&mut self) -> Result<()> {
        self.inner.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        self.inner.next()
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn schema(&self) -> &[String] {
        self.inner.schema()
    }
}

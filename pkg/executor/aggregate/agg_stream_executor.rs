// StreamAgg for scalar aggregations (no GROUP BY).
// Completeness wave: sorted GROUP BY StreamAgg, pipelined partial aggs.
use super::HashAggExec;
use crate::internal::exec::Executor;
use crate::join::apply::SubqueryEvalCtx;
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

    pub(crate) fn with_subquery(mut self, subquery: SubqueryEvalCtx) -> Self {
        self.inner = self.inner.with_subquery(subquery);
        self
    }

    pub(crate) fn with_group_collations(
        mut self,
        modes: Vec<sealdb_util::collate::CollateMode>,
    ) -> Self {
        self.inner = self.inner.with_group_collations(modes);
        self
    }

    /// ORDER BY-only columns, exposed as extra first-row outputs.
    pub(crate) fn with_order_columns(mut self, columns: &[String]) -> Self {
        self.inner = self.inner.with_order_columns(columns);
        self
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

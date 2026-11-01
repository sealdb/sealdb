use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::eval_expr;
use sealdb_parser::ast::ExprNode;
use sealdb_util::chunk::Chunk;

pub struct ProjectionExec {
    exprs: Vec<ExprNode>,
    names: Vec<String>,
    child: Box<dyn Executor>,
    opened: bool,
}

impl ProjectionExec {
    pub fn new(exprs: Vec<ExprNode>, names: Vec<String>, child: Box<dyn Executor>) -> Self {
        Self {
            exprs,
            names,
            child,
            opened: false,
        }
    }
}

impl Executor for ProjectionExec {
    fn open(&mut self) -> Result<()> {
        self.opened = true;
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.child.next()?.is_none() {
            return Ok(None);
        }
        let mut chunk = Chunk::new(self.exprs.len(), 1);
        let row = self
            .exprs
            .iter()
            .map(eval_expr)
            .collect::<Result<Vec<_>>>()?;
        chunk.append_row(row);
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        self.opened = false;
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        &self.names
    }
}

use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::eval_expr_with_row;
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
        let Some(input) = self.child.next()? else {
            return Ok(None);
        };
        let schema = self.child.schema();
        let mut chunk = Chunk::new(self.exprs.len(), input.num_rows());
        for i in 0..input.num_rows() {
            let row = input.get_row(i);
            let projected = self
                .exprs
                .iter()
                .map(|expr| eval_expr_with_row(expr, schema, &row))
                .collect::<Result<Vec<_>>>()?;
            chunk.append_row(projected);
        }
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

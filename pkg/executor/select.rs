use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::eval_filter;
use sealdb_parser::ast::ExprNode;
use sealdb_util::chunk::Chunk;

pub struct TableDualExec {
    opened: bool,
    emitted: bool,
}

impl TableDualExec {
    pub fn new() -> Self {
        Self {
            opened: false,
            emitted: false,
        }
    }
}

impl Default for TableDualExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for TableDualExec {
    fn open(&mut self) -> Result<()> {
        self.opened = true;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.opened || self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(0, 1);
        chunk.append_row(Vec::new());
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        self.opened = false;
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

pub struct MemTableExec {
    schema: Vec<String>,
    rows: Vec<Vec<sealdb_types::Datum>>,
    emitted: bool,
}

impl MemTableExec {
    pub fn new(schema: Vec<String>, rows: Vec<Vec<sealdb_types::Datum>>) -> Self {
        Self {
            schema,
            rows,
            emitted: false,
        }
    }
}

impl Executor for MemTableExec {
    fn open(&mut self) -> Result<()> {
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

pub struct SelectionExec {
    filter: ExprNode,
    child: Box<dyn Executor>,
}

impl SelectionExec {
    pub fn new(filter: ExprNode, child: Box<dyn Executor>) -> Self {
        Self { filter, child }
    }
}

impl Executor for SelectionExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            let Some(chunk) = self.child.next()? else {
                return Ok(None);
            };
            let schema = self.child.schema();
            let mut out = Chunk::new(chunk.num_cols(), chunk.num_rows());
            for i in 0..chunk.num_rows() {
                let row = chunk.get_row(i);
                if eval_filter(&self.filter, schema, &row)? {
                    out.append_row(row);
                }
            }
            if out.num_rows() > 0 {
                return Ok(Some(out));
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        self.child.schema()
    }
}

pub struct LimitExec {
    limit: Option<u64>,
    offset: u64,
    skipped: u64,
    emitted: u64,
    child: Box<dyn Executor>,
}

impl LimitExec {
    pub fn new(limit: Option<u64>, offset: u64, child: Box<dyn Executor>) -> Self {
        Self {
            limit,
            offset,
            skipped: 0,
            emitted: 0,
            child,
        }
    }
}

impl Executor for LimitExec {
    fn open(&mut self) -> Result<()> {
        self.skipped = 0;
        self.emitted = 0;
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.limit == Some(0) {
            return Ok(None);
        }
        loop {
            let Some(chunk) = self.child.next()? else {
                return Ok(None);
            };
            let schema_len = chunk.num_cols();
            let mut out = Chunk::new(schema_len, chunk.num_rows());
            for i in 0..chunk.num_rows() {
                if self.skipped < self.offset {
                    self.skipped += 1;
                    continue;
                }
                if let Some(limit) = self.limit
                    && self.emitted >= limit
                {
                    break;
                }
                out.append_row(chunk.get_row(i));
                self.emitted += 1;
            }
            if out.num_rows() > 0 {
                return Ok(Some(out));
            }
            if self.limit.is_some_and(|limit| self.emitted >= limit) {
                return Ok(None);
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        self.child.schema()
    }
}

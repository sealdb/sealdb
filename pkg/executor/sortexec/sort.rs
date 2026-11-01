use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub struct SortExec {
    by: Vec<ByItem>,
    child: Box<dyn Executor>,
    emitted: bool,
}

impl SortExec {
    pub fn new(by: Vec<ByItem>, child: Box<dyn Executor>) -> Self {
        Self {
            by,
            child,
            emitted: false,
        }
    }
}

impl Executor for SortExec {
    fn open(&mut self) -> Result<()> {
        self.emitted = false;
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let schema = self.child.schema().to_vec();
        let mut rows = Vec::new();
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                rows.push(chunk.get_row(i));
            }
        }
        rows.sort_by(|a, b| compare_rows(a, b, &self.by, &schema));
        let mut out = Chunk::new(schema.len(), rows.len());
        for row in rows {
            out.append_row(row);
        }
        Ok(Some(out))
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        self.child.schema()
    }
}

fn compare_rows(
    left: &[Datum],
    right: &[Datum],
    by: &[ByItem],
    schema: &[String],
) -> std::cmp::Ordering {
    for item in by {
        let lv = eval_expr_with_row(&item.expr, schema, left).unwrap_or(Datum::Null);
        let rv = eval_expr_with_row(&item.expr, schema, right).unwrap_or(Datum::Null);
        let ord = lv.compare(&rv);
        if ord != std::cmp::Ordering::Equal {
            return if item.desc { ord.reverse() } else { ord };
        }
    }
    std::cmp::Ordering::Equal
}

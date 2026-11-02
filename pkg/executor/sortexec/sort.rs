use crate::internal::exec::Executor;
use crate::sortexec::parallel_sort_worker::parallel_sort_rows;
use crate::sortexec::sort_spill::{merge_runs, spill_sorted_run};
use sealdb_errno::Result;
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use sealdb_util::chunk::{Chunk, DataInDiskByRows, mem_row_limit};

pub struct SortExec {
    by: Vec<ByItem>,
    child: Box<dyn Executor>,
    mem_limit: usize,
    spilled: bool,
    emitted: bool,
}

impl SortExec {
    pub fn new(by: Vec<ByItem>, child: Box<dyn Executor>) -> Self {
        Self::with_mem_limit(by, child, mem_row_limit())
    }

    pub fn with_mem_limit(by: Vec<ByItem>, child: Box<dyn Executor>, mem_limit: usize) -> Self {
        Self {
            by,
            child,
            mem_limit,
            spilled: false,
            emitted: false,
        }
    }

    pub fn spilled(&self) -> bool {
        self.spilled
    }
}

impl Executor for SortExec {
    fn open(&mut self) -> Result<()> {
        self.emitted = false;
        self.spilled = false;
        self.child.open()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let schema = self.child.schema().to_vec();
        let mut buf = Vec::new();
        let mut runs: Vec<DataInDiskByRows> = Vec::new();
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                buf.push(chunk.get_row(i));
                if buf.len() >= self.mem_limit {
                    runs.push(spill_sorted_run(&mut buf, &self.by, &schema)?);
                    self.spilled = true;
                }
            }
        }
        let rows = if runs.is_empty() {
            parallel_sort_rows(&mut buf, &self.by, &schema)?;
            buf
        } else {
            if !buf.is_empty() {
                runs.push(spill_sorted_run(&mut buf, &self.by, &schema)?);
            }
            merge_runs(&mut runs, &self.by, &schema)?
        };
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

pub(crate) fn compare_rows(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::exec::VecExec;
    use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode};
    use sealdb_types::Datum;

    fn by_id() -> Vec<ByItem> {
        vec![ByItem {
            expr: ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "id".to_string(),
            }),
            desc: false,
        }]
    }

    #[test]
    fn test_sort_spill_matches_in_memory() {
        let rows = vec![
            vec![Datum::I64(3), Datum::Bytes(b"c".to_vec())],
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())],
            vec![Datum::I64(0), Datum::Bytes(b"z".to_vec())],
        ];
        let schema = vec!["id".to_string(), "name".to_string()];
        let mut mem = SortExec::with_mem_limit(
            by_id(),
            Box::new(VecExec::new(schema.clone(), rows.clone())),
            usize::MAX,
        );
        mem.open().unwrap();
        let mem_rows = mem.next().unwrap().unwrap();
        let mut spilled =
            SortExec::with_mem_limit(by_id(), Box::new(VecExec::new(schema, rows)), 1);
        spilled.open().unwrap();
        let spilled_rows = spilled.next().unwrap().unwrap();
        assert!(spilled.spilled());
        assert_eq!(mem_rows.num_rows(), spilled_rows.num_rows());
        for i in 0..mem_rows.num_rows() {
            assert_eq!(mem_rows.get_row(i), spilled_rows.get_row(i));
        }
        assert_eq!(mem_rows.get_row(0)[0], Datum::I64(0));
        assert_eq!(mem_rows.get_row(3)[0], Datum::I64(3));
    }
}

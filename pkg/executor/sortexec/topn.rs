// F05 leftover: TopN heap + spill (TiDB pkg/executor/sortexec/topn.go).
// Completeness wave / not this feature: parallel TopN workers.
use crate::internal::exec::Executor;
use crate::sortexec::sort::compare_rows;
use crate::sortexec::sort_spill::{merge_runs_n, spill_sorted_run};
use sealdb_errno::Result;
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use sealdb_util::chunk::{Chunk, DataInDiskByRows, mem_row_limit};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

pub struct TopNExec {
    by: Vec<ByItem>,
    limit: u64,
    offset: u64,
    child: Box<dyn Executor>,
    mem_limit: usize,
    spilled: bool,
    emitted: bool,
}

impl TopNExec {
    pub fn new(by: Vec<ByItem>, limit: u64, offset: u64, child: Box<dyn Executor>) -> Self {
        Self::with_mem_limit(by, limit, offset, child, mem_row_limit())
    }

    pub fn with_mem_limit(
        by: Vec<ByItem>,
        limit: u64,
        offset: u64,
        child: Box<dyn Executor>,
        mem_limit: usize,
    ) -> Self {
        Self {
            by,
            limit,
            offset,
            child,
            mem_limit: mem_limit.max(1),
            spilled: false,
            emitted: false,
        }
    }

    pub fn spilled(&self) -> bool {
        self.spilled
    }

    fn keep(&self) -> usize {
        self.limit.saturating_add(self.offset) as usize
    }
}

impl Executor for TopNExec {
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
        if self.limit == 0 {
            return Ok(Some(Chunk::new(self.child.schema().len(), 0)));
        }
        let schema = self.child.schema().to_vec();
        let keep = self.keep();
        let rows = if keep <= self.mem_limit {
            heap_topn(&mut *self.child, &self.by, &schema, keep)?
        } else {
            self.spilled = true;
            spill_topn(&mut *self.child, &self.by, &schema, keep, self.mem_limit)?
        };
        let taken: Vec<Vec<Datum>> = rows
            .into_iter()
            .skip(self.offset as usize)
            .take(self.limit as usize)
            .collect();
        let mut out = Chunk::new(schema.len(), taken.len());
        for row in taken {
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

fn heap_topn(
    child: &mut dyn Executor,
    by: &[ByItem],
    schema: &[String],
    keep: usize,
) -> Result<Vec<Vec<Datum>>> {
    let by = Arc::new(by.to_vec());
    let schema = Arc::new(schema.to_vec());
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    while let Some(chunk) = child.next()? {
        for i in 0..chunk.num_rows() {
            let row = chunk.get_row(i);
            let item = HeapItem {
                row,
                by: Arc::clone(&by),
                schema: Arc::clone(&schema),
            };
            if heap.len() < keep {
                heap.push(item);
            } else if let Some(worst) = heap.peek()
                && item.cmp(worst).is_lt()
            {
                heap.pop();
                heap.push(item);
            }
        }
    }
    let mut rows: Vec<Vec<Datum>> = heap.into_iter().map(|item| item.row).collect();
    rows.sort_by(|a, b| compare_rows(a, b, &by, &schema));
    Ok(rows)
}

fn spill_topn(
    child: &mut dyn Executor,
    by: &[ByItem],
    schema: &[String],
    keep: usize,
    mem_limit: usize,
) -> Result<Vec<Vec<Datum>>> {
    let mut buf = Vec::new();
    let mut runs: Vec<DataInDiskByRows> = Vec::new();
    while let Some(chunk) = child.next()? {
        for i in 0..chunk.num_rows() {
            buf.push(chunk.get_row(i));
            if buf.len() >= mem_limit {
                runs.push(spill_sorted_run(&mut buf, by, schema)?);
            }
        }
    }
    if runs.is_empty() {
        buf.sort_by(|a, b| compare_rows(a, b, by, schema));
        buf.truncate(keep);
        return Ok(buf);
    }
    if !buf.is_empty() {
        runs.push(spill_sorted_run(&mut buf, by, schema)?);
    }
    merge_runs_n(&mut runs, by, schema, keep)
}

struct HeapItem {
    row: Vec<Datum>,
    by: Arc<Vec<ByItem>>,
    schema: Arc<Vec<String>>,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_rows(&self.row, &other.row, &self.by, &self.schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::exec::VecExec;
    use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode};

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

    fn rows() -> Vec<Vec<Datum>> {
        vec![
            vec![Datum::I64(3), Datum::Bytes(b"c".to_vec())],
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(4), Datum::Bytes(b"d".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())],
            vec![Datum::I64(0), Datum::Bytes(b"z".to_vec())],
        ]
    }

    #[test]
    fn test_topn_heap_and_spill_match() {
        let schema = vec!["id".to_string(), "name".to_string()];
        let mut heap = TopNExec::with_mem_limit(
            by_id(),
            3,
            0,
            Box::new(VecExec::new(schema.clone(), rows())),
            usize::MAX,
        );
        heap.open().unwrap();
        let heap_rows = heap.next().unwrap().unwrap();
        assert!(!heap.spilled());
        let mut spilled =
            TopNExec::with_mem_limit(by_id(), 3, 0, Box::new(VecExec::new(schema, rows())), 1);
        spilled.open().unwrap();
        let spilled_rows = spilled.next().unwrap().unwrap();
        assert!(spilled.spilled());
        assert_eq!(heap_rows.num_rows(), 3);
        assert_eq!(spilled_rows.num_rows(), 3);
        for i in 0..3 {
            assert_eq!(heap_rows.get_row(i), spilled_rows.get_row(i));
        }
        assert_eq!(heap_rows.get_row(0)[0], Datum::I64(0));
        assert_eq!(heap_rows.get_row(2)[0], Datum::I64(2));
    }
}

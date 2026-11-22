use crate::internal::exec::Executor;
use crate::sortexec::parallel_sort_worker::parallel_sort_rows;
use crate::sortexec::sort_spill::{merge_runs, spill_sorted_run};
use sealdb_errno::Result;
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use sealdb_util::chunk::{Chunk, DataInDiskByRows, mem_row_limit};
use sealdb_util::collate::CollateMode;
use std::cell::RefCell;

thread_local! {
    /// Per-`ORDER BY` key collations (column FieldType), scoped around a sort.
    static ORDER_COLLATIONS: RefCell<Vec<CollateMode>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with per-key ORDER BY collations visible to `compare_rows`.
pub fn with_order_collations<T>(modes: Vec<CollateMode>, f: impl FnOnce() -> T) -> T {
    ORDER_COLLATIONS.with(|cell| {
        let prev = cell.replace(modes);
        let out = f();
        *cell.borrow_mut() = prev;
        out
    })
}

/// Snapshot of the current per-key ORDER BY collations (for worker threads).
pub fn order_collations_snapshot() -> Vec<CollateMode> {
    ORDER_COLLATIONS.with(|cell| cell.borrow().clone())
}

fn order_key_collation(idx: usize) -> CollateMode {
    ORDER_COLLATIONS.with(|cell| {
        cell.borrow()
            .get(idx)
            .copied()
            .unwrap_or_else(sealdb_expression::collation_mode)
    })
}

pub struct SortExec {
    by: Vec<ByItem>,
    /// Collation of each `ORDER BY` key (column collation when applicable).
    collations: Vec<CollateMode>,
    child: Box<dyn Executor>,
    mem_limit: usize,
    spilled: bool,
    emitted: bool,
}

impl SortExec {
    pub fn new(by: Vec<ByItem>, child: Box<dyn Executor>) -> Self {
        Self::with_collations(by, Vec::new(), child)
    }

    pub fn with_collations(
        by: Vec<ByItem>,
        collations: Vec<CollateMode>,
        child: Box<dyn Executor>,
    ) -> Self {
        Self::with_mem_limit(by, collations, child, mem_row_limit())
    }

    pub fn with_mem_limit(
        by: Vec<ByItem>,
        collations: Vec<CollateMode>,
        child: Box<dyn Executor>,
        mem_limit: usize,
    ) -> Self {
        Self {
            by,
            collations,
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
        let collations = self.collations.clone();
        with_order_collations(collations, || {
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
        })
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        self.child.schema()
    }
}

fn eval_order_key(expr: &sealdb_parser::ast::ExprNode, schema: &[String], row: &[Datum]) -> Datum {
    // TiDB/MySQL: `ORDER BY n` is 1-based output column ordinal.
    if let sealdb_parser::ast::ExprNode::Value(v) = expr {
        let idx = match &v.datum {
            Datum::I64(n) if *n > 0 => Some((*n as usize).saturating_sub(1)),
            Datum::U64(n) if *n > 0 => Some((*n as usize).saturating_sub(1)),
            _ => None,
        };
        if let Some(i) = idx
            && let Some(cell) = row.get(i)
        {
            return enum_sort_key(cell.clone());
        }
    }
    let value = eval_expr_with_row(expr, schema, row).unwrap_or(Datum::Null);
    // A bare ENUM column sorts by member index (`enum('c','b','a')` sorts
    // `c < b < a`), while an expression that merely yields an ENUM value
    // (e.g. `if(..., c_enum, c_enum)`) has a string type in TiDB and compares
    // by label. Only direct column references keep the numeric ordering.
    if matches!(expr, sealdb_parser::ast::ExprNode::Column(_)) {
        enum_sort_key(value)
    } else {
        value
    }
}

/// Replace a bare `ENUM` value with its member index so sorting uses the
/// declaration order rather than the label text.
fn enum_sort_key(cell: Datum) -> Datum {
    match cell {
        Datum::Enum(e) => Datum::I64(e.number as i64),
        other => other,
    }
}

pub(crate) fn compare_rows(
    left: &[Datum],
    right: &[Datum],
    by: &[ByItem],
    schema: &[String],
) -> std::cmp::Ordering {
    for (idx, item) in by.iter().enumerate() {
        let lv = eval_order_key(&item.expr, schema, left);
        let rv = eval_order_key(&item.expr, schema, right);
        let ord = if matches!((&lv, &rv), (Datum::Bytes(_), Datum::Bytes(_))) {
            sealdb_util::collate::compare_bytes(
                order_key_collation(idx),
                &lv.as_bytes(),
                &rv.as_bytes(),
            )
        } else {
            lv.compare(&rv)
        };
        if ord != std::cmp::Ordering::Equal {
            return if item.desc { ord.reverse() } else { ord };
        }
    }
    std::cmp::Ordering::Equal
}

pub(crate) fn sort_rows_by<T>(
    rows: &mut [T],
    by: &[ByItem],
    schema: &[String],
    values: impl Fn(&T) -> &[Datum] + Copy,
) {
    if by.is_empty() {
        return;
    }
    rows.sort_by(|a, b| compare_rows(values(a), values(b), by, schema));
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
            Vec::new(),
            Box::new(VecExec::new(schema.clone(), rows.clone())),
            usize::MAX,
        );
        mem.open().unwrap();
        let mem_rows = mem.next().unwrap().unwrap();
        let mut spilled =
            SortExec::with_mem_limit(by_id(), Vec::new(), Box::new(VecExec::new(schema, rows)), 1);
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

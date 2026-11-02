// F15 core path: UnionExec for UNION / EXCEPT / INTERSECT
// (TiDB pkg/executor/unionexec/union.go). Parenthesized set operands flatten
// left-to-right. EXISTS / IN / scalar subqueries may hold a ResultSetNode set
// op. Parallel child drain: SEALDB_UNION_CONCURRENCY (default 1).
use crate::internal::exec::Executor;
use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result};
use sealdb_expression::{collation_mode, with_collation};
use sealdb_parser::ast::SetOprType;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};
use std::thread;

pub struct UnionExec {
    left: Box<dyn Executor>,
    right: Box<dyn Executor>,
    op: SetOprType,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl UnionExec {
    pub fn new(left: Box<dyn Executor>, right: Box<dyn Executor>, op: SetOprType) -> Self {
        Self {
            left,
            right,
            op,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for UnionExec {
    fn open(&mut self) -> Result<()> {
        let (left, right) = collect_pair(&mut self.left, &mut self.right, union_concurrency())?;
        self.schema = left.schema;
        if self.schema.is_empty() {
            self.schema = right.schema;
        }
        let width = left
            .rows
            .first()
            .or(right.rows.first())
            .map(|row| row.len())
            .unwrap_or(self.schema.len());
        for row in left.rows.iter().chain(right.rows.iter()) {
            if row.len() != width {
                return Err(column_mismatch());
            }
        }
        self.rows = apply_set_op(self.op, left.rows, right.rows);
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
        self.left.close()?;
        self.right.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

/// Worker count for draining UNION children. Default 1 (serial); set
/// `SEALDB_UNION_CONCURRENCY` >= 2 to open and collect both sides together.
fn union_concurrency() -> usize {
    std::env::var("SEALDB_UNION_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(1)
}

struct ChildRows {
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
}

fn collect_pair(
    left: &mut Box<dyn Executor>,
    right: &mut Box<dyn Executor>,
    workers: usize,
) -> Result<(ChildRows, ChildRows)> {
    if workers <= 1 {
        return drain_serial(left, right);
    }
    let mode = collation_mode();
    thread::scope(|s| {
        let left_h = s.spawn(|| with_collation(mode, || drain_child(left)));
        let right_h = s.spawn(|| with_collation(mode, || drain_child(right)));
        let left = left_h
            .join()
            .map_err(|_| Error::new(ERR_UNKNOWN_ERROR, "parallel union worker panicked"))??;
        let right = right_h
            .join()
            .map_err(|_| Error::new(ERR_UNKNOWN_ERROR, "parallel union worker panicked"))??;
        Ok((left, right))
    })
}

fn drain_serial(
    left: &mut Box<dyn Executor>,
    right: &mut Box<dyn Executor>,
) -> Result<(ChildRows, ChildRows)> {
    left.open()?;
    right.open()?;
    Ok((
        ChildRows {
            schema: left.schema().to_vec(),
            rows: collect_rows(left)?,
        },
        ChildRows {
            schema: right.schema().to_vec(),
            rows: collect_rows(right)?,
        },
    ))
}

fn drain_child(exec: &mut Box<dyn Executor>) -> Result<ChildRows> {
    exec.open()?;
    Ok(ChildRows {
        schema: exec.schema().to_vec(),
        rows: collect_rows(exec)?,
    })
}

fn collect_rows(exec: &mut Box<dyn Executor>) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = exec.next()? {
        for i in 0..chunk.num_rows() {
            rows.push(chunk.get_row(i));
        }
    }
    Ok(rows)
}

fn apply_set_op(op: SetOprType, left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    match op {
        SetOprType::UnionAll => {
            let mut rows = left;
            rows.extend(right);
            rows
        }
        SetOprType::Union => unique_concat(left, right),
        SetOprType::Except => except_distinct(left, right),
        SetOprType::ExceptAll => except_all(left, right),
        SetOprType::Intersect => intersect_distinct(left, right),
        SetOprType::IntersectAll => intersect_all(left, right),
    }
}

fn unique_concat(left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in left.into_iter().chain(right) {
        if seen.insert(row_key(&row)) {
            out.push(row);
        }
    }
    out
}

fn except_distinct(left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let banned: HashSet<_> = right.iter().map(|row| row_key(row)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in left {
        let key = row_key(&row);
        if !banned.contains(&key) && seen.insert(key) {
            out.push(row);
        }
    }
    out
}

fn except_all(left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let mut counts = counts_of(&right);
    let mut out = Vec::new();
    for row in left {
        let key = row_key(&row);
        match counts.get_mut(&key) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(row),
        }
    }
    out
}

fn intersect_distinct(left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let right_keys: HashSet<_> = right.iter().map(|row| row_key(row)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in left {
        let key = row_key(&row);
        if right_keys.contains(&key) && seen.insert(key) {
            out.push(row);
        }
    }
    out
}

fn intersect_all(left: Vec<Vec<Datum>>, right: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let mut counts = counts_of(&right);
    let mut out = Vec::new();
    for row in left {
        let key = row_key(&row);
        if let Some(n) = counts.get_mut(&key)
            && *n > 0
        {
            *n -= 1;
            out.push(row);
        }
    }
    out
}

fn counts_of(rows: &[Vec<Datum>]) -> HashMap<Vec<String>, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(row_key(row)).or_insert(0) += 1;
    }
    counts
}

fn row_key(row: &[Datum]) -> Vec<String> {
    row.iter().map(Datum::display).collect()
}

fn column_mismatch() -> Error {
    Error::new(
        ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT,
        "The used SELECT statements have a different number of columns",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::exec::VecExec;

    fn run_union(op: SetOprType, workers: usize) -> Vec<Vec<String>> {
        let left = Box::new(VecExec::new(
            vec!["n".to_string()],
            vec![vec![Datum::I64(1)], vec![Datum::I64(2)]],
        ));
        let right = Box::new(VecExec::new(
            vec!["n".to_string()],
            vec![vec![Datum::I64(2)], vec![Datum::I64(3)]],
        ));
        let mut exec = UnionExec::new(left, right, op);
        let (left, right) = collect_pair(&mut exec.left, &mut exec.right, workers).unwrap();
        apply_set_op(op, left.rows, right.rows)
            .into_iter()
            .map(|row| row.iter().map(Datum::display).collect())
            .collect()
    }

    #[test]
    fn test_parallel_union_matches_serial() {
        let serial = run_union(SetOprType::Union, 1);
        let parallel = run_union(SetOprType::Union, 2);
        assert_eq!(serial, parallel);
        assert_eq!(
            serial,
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()]
            ]
        );
        let all = run_union(SetOprType::UnionAll, 2);
        assert_eq!(all.len(), 4);
    }
}

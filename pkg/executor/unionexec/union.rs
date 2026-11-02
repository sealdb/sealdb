// F15 core path: UnionExec for UNION / EXCEPT / INTERSECT
// (TiDB pkg/executor/unionexec/union.go). Completeness wave / not this
// feature: parenthesized nested set ops, UNION in subquery, parallel workers.
use crate::internal::exec::Executor;
use sealdb_errno::{ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT, Error, Result};
use sealdb_parser::ast::SetOprType;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};

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
        self.left.open()?;
        self.right.open()?;
        self.schema = self.left.schema().to_vec();
        if self.schema.is_empty() {
            self.schema = self.right.schema().to_vec();
        }
        let left_rows = collect_rows(&mut self.left)?;
        let right_rows = collect_rows(&mut self.right)?;
        let width = left_rows
            .first()
            .or(right_rows.first())
            .map(|row| row.len())
            .unwrap_or(self.schema.len());
        for row in left_rows.iter().chain(right_rows.iter()) {
            if row.len() != width {
                return Err(column_mismatch());
            }
        }
        self.rows = apply_set_op(self.op, left_rows, right_rows);
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

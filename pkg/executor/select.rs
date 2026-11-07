use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::{eval_filter, validate_string_cmp_literals};
use sealdb_parser::ast::ExprNode;
use sealdb_util::chunk::Chunk;

pub struct TableDualExec {
    opened: bool,
    emitted: usize,
    row_count: usize,
    schema: Vec<String>,
}

impl TableDualExec {
    pub fn new(row_count: usize, schema: Vec<String>) -> Self {
        Self {
            opened: false,
            emitted: 0,
            row_count,
            schema,
        }
    }
}

impl Default for TableDualExec {
    fn default() -> Self {
        Self::new(1, Vec::new())
    }
}

impl Executor for TableDualExec {
    fn open(&mut self) -> Result<()> {
        self.opened = true;
        self.emitted = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.opened || self.emitted >= self.row_count {
            return Ok(None);
        }
        self.emitted += 1;
        let mut chunk = Chunk::new(self.schema.len(), 1);
        chunk.append_row(vec![sealdb_types::Datum::Null; self.schema.len()]);
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        self.opened = false;
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
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
    /// Double DST gap warnings when filtering above an index plan (types/time).
    index_dst_double: bool,
}

impl SelectionExec {
    pub fn new(filter: ExprNode, child: Box<dyn Executor>) -> Self {
        Self {
            filter,
            child,
            index_dst_double: false,
        }
    }

    pub fn with_index_dst_double(mut self, on: bool) -> Self {
        self.index_dst_double = on;
        self
    }
}

impl Executor for SelectionExec {
    fn open(&mut self) -> Result<()> {
        validate_string_cmp_literals(&self.filter)?;
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
                let pass = if self.index_dst_double {
                    sealdb_expression::with_index_dst_double(|| {
                        eval_filter(&self.filter, schema, &row)
                    })?
                } else {
                    eval_filter(&self.filter, schema, &row)?
                };
                if pass {
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

/// Dedup child rows by named columns (SemiJoinRewriter inner aggregation).
pub struct DistinctExec {
    keys: Vec<String>,
    fields: Vec<ExprNode>,
    names: Vec<String>,
    child: Box<dyn Executor>,
    schema: Vec<String>,
    rows: Vec<Vec<sealdb_types::Datum>>,
    emitted: bool,
}

impl DistinctExec {
    pub fn new(
        keys: Vec<String>,
        fields: Vec<ExprNode>,
        names: Vec<String>,
        child: Box<dyn Executor>,
    ) -> Self {
        Self {
            keys,
            fields,
            names,
            child,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for DistinctExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.schema = self.child.schema().to_vec();
        let idxs: Vec<usize> = self
            .keys
            .iter()
            .filter_map(|key| {
                self.schema.iter().position(|col| {
                    col.eq_ignore_ascii_case(key)
                        || col
                            .rsplit_once('.')
                            .is_some_and(|(_, name)| name.eq_ignore_ascii_case(key))
                })
            })
            .collect();
        // `SELECT DISTINCT a, b` outputs only the select list (TiDB HashAgg
        // group keys), not the whole child row.
        let project = !self.fields.is_empty() && !self.names.is_empty();
        if project {
            self.schema = self.names.clone();
        }
        let mut seen = std::collections::HashSet::new();
        while let Some(chunk) = self.child.next()? {
            let child_schema = self.child.schema().to_vec();
            for i in 0..chunk.num_rows() {
                let row = chunk.get_row(i);
                let out = if project {
                    self.fields
                        .iter()
                        .map(|expr| sealdb_expression::eval_expr_with_row(expr, &child_schema, &row))
                        .collect::<Result<Vec<_>>>()?
                } else {
                    row.clone()
                };
                let key = if project {
                    row_key(&out, &(0..out.len()).collect::<Vec<_>>())
                } else if idxs.is_empty() {
                    row_key(&out, &(0..out.len()).collect::<Vec<_>>())
                } else {
                    row_key(&out, &idxs)
                };
                if seen.insert(key) {
                    self.rows.push(out);
                }
            }
        }
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
        self.child.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn row_key(row: &[sealdb_types::Datum], idxs: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for idx in idxs {
        match row.get(*idx).unwrap_or(&sealdb_types::Datum::Null) {
            sealdb_types::Datum::Null => out.push(0),
            sealdb_types::Datum::I64(v) => {
                out.push(1);
                out.extend(v.to_le_bytes());
            }
            sealdb_types::Datum::U64(v) => {
                out.push(2);
                out.extend(v.to_le_bytes());
            }
            sealdb_types::Datum::F64(v) => {
                out.push(3);
                out.extend(v.to_le_bytes());
            }
            sealdb_types::Datum::Bytes(b) => {
                out.push(4);
                out.extend((b.len() as u32).to_le_bytes());
                out.extend(b);
            }
            sealdb_types::Datum::Decimal(d) => {
                out.push(5);
                out.extend(d.to_string().into_bytes());
            }
            sealdb_types::Datum::Time(t) => {
                out.push(6);
                out.extend(t.to_string().into_bytes());
            }
            // ENUM/SET key by label text, like a string column.
            sealdb_types::Datum::Enum(e) => {
                out.push(4);
                out.extend((e.label.len() as u32).to_le_bytes());
                out.extend(&e.label);
            }
        }
        out.push(0xff);
    }
    out
}

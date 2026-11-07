use crate::internal::exec::Executor;
use sealdb_errno::Result;
use sealdb_expression::{datum_sql_eq, eval_expr_with_row, truthy};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

/// Row-constructor null-aware anti join (TiDB `PhysicalHashJoin` with
/// `NullAwareAntiJoin`).
///
/// Implements the SQL three-valued semantics of
/// `(p0, p1, …) NOT IN (SELECT b0, b1, …)`:
///
/// * a build row compares `NULL` when a key pair has a NULL operand and no
///   definite mismatch,
/// * `IN` is TRUE if any build row is definitely equal, NULL if none is
///   equal but some compared NULL, FALSE otherwise.
///
/// `left_outer` emits `probe row + 1/0/NULL` (anti left outer semi join);
/// otherwise only definitely-unmatched probe rows are emitted (anti semi).
pub struct NullAwareAntiJoinExec {
    left_outer: bool,
    bool_only: bool,
    null_aware: bool,
    bool_column: String,
    keys: Vec<(ExprNode, ExprNode)>,
    other_cond: Option<ExprNode>,
    probe_table: String,
    build_table: String,
    /// Emit only the first N probe columns (`SELECT *`).
    probe_width: Option<usize>,
    probe: Box<dyn Executor>,
    build: Box<dyn Executor>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl NullAwareAntiJoinExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left_outer: bool,
        bool_only: bool,
        null_aware: bool,
        bool_column: String,
        keys: Vec<(ExprNode, ExprNode)>,
        other_cond: Option<ExprNode>,
        probe_table: String,
        build_table: String,
        probe_width: Option<usize>,
        probe: Box<dyn Executor>,
        build: Box<dyn Executor>,
    ) -> Self {
        Self {
            left_outer,
            bool_only,
            null_aware,
            bool_column,
            keys,
            other_cond,
            probe_table,
            build_table,
            probe_width,
            probe,
            build,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for NullAwareAntiJoinExec {
    fn open(&mut self) -> Result<()> {
        self.probe.open()?;
        self.build.open()?;
        let probe_schema = self.probe.schema().to_vec();
        let build_schema = self.build.schema().to_vec();
        let probe_qual = qualify(&probe_schema, &self.probe_table);
        let build_qual = qualify(&build_schema, &self.build_table);
        let mut full_schema = probe_qual.clone();
        full_schema.extend(build_qual.iter().cloned());

        self.schema = if self.bool_only {
            vec![self.bool_column.clone()]
        } else if self.left_outer {
            let mut schema = probe_schema;
            schema.push(self.bool_column.clone());
            schema
        } else if let Some(width) = self.probe_width {
            probe_schema.into_iter().take(width).collect()
        } else {
            probe_schema
        };

        let build_rows = collect_rows(self.build.as_mut())?;
        let mut build_keys: Vec<Vec<Datum>> = Vec::with_capacity(build_rows.len());
        for row in &build_rows {
            let mut ks = Vec::with_capacity(self.keys.len());
            for (_, build_expr) in &self.keys {
                ks.push(eval_expr_with_row(build_expr, &build_qual, row)?);
            }
            build_keys.push(ks);
        }

        let probe_rows = collect_rows(self.probe.as_mut())?;
        let mut out: Vec<Vec<Datum>> = Vec::new();
        for probe_row in &probe_rows {
            let mut probe_keys = Vec::with_capacity(self.keys.len());
            for (probe_expr, _) in &self.keys {
                probe_keys.push(eval_expr_with_row(probe_expr, &probe_qual, probe_row)?);
            }
            let mut any_true = false;
            let mut any_null = false;
            for (row, build_key) in build_rows.iter().zip(build_keys.iter()) {
                // Three-valued key comparison: `false` is a definite mismatch,
                // `None` means at least one NULL operand (unknown).
                let mut any_false = false;
                let mut saw_null = false;
                for (pk, bk) in probe_keys.iter().zip(build_key.iter()) {
                    if pk.is_null() || bk.is_null() {
                        saw_null = true;
                    } else if !datum_sql_eq(pk, bk)? {
                        any_false = true;
                        break;
                    }
                }
                if any_false {
                    continue;
                }
                if !self.null_aware {
                    // Correlated-equality rewrite: plain anti join on the keys.
                    if saw_null {
                        continue;
                    }
                    if self.other_cond_passes(&full_schema, probe_row, row)? {
                        any_true = true;
                        break;
                    }
                    continue;
                }
                if saw_null {
                    // Keys are only possibly-equal: the row compares NULL unless
                    // the extra join condition is definitely FALSE.
                    if !self.other_cond_passes(&full_schema, probe_row, row)? {
                        continue;
                    }
                    any_null = true;
                    continue;
                }
                match self.other_cond_passes(&full_schema, probe_row, row)? {
                    true => {
                        any_true = true;
                        break;
                    }
                    false => {}
                }
            }
            let not_in = if any_true {
                Some(false)
            } else if any_null && self.null_aware {
                None
            } else {
                Some(true)
            };
            let bool_datum_value = match not_in {
                Some(true) => bool_datum(true),
                Some(false) => bool_datum(false),
                None => Datum::Null,
            };
            if self.bool_only {
                out.push(vec![bool_datum_value]);
            } else if self.left_outer {
                let mut row = probe_row.clone();
                row.push(bool_datum_value);
                out.push(row);
            } else if not_in == Some(true) {
                let row = match self.probe_width {
                    Some(width) => probe_row.iter().take(width).cloned().collect(),
                    None => probe_row.clone(),
                };
                out.push(row);
            }
        }
        self.rows = out;
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
        self.probe.close()?;
        self.build.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

impl NullAwareAntiJoinExec {
    /// `other cond` is a join condition: no match unless it is definitely TRUE.
    fn other_cond_passes(
        &self,
        full_schema: &[String],
        probe_row: &[Datum],
        build_row: &[Datum],
    ) -> Result<bool> {
        let Some(cond) = &self.other_cond else {
            return Ok(true);
        };
        let mut row = probe_row.to_vec();
        row.extend(build_row.iter().cloned());
        Ok(truthy(&eval_expr_with_row(cond, full_schema, &row)?).unwrap_or(false))
    }
}

fn bool_datum(value: bool) -> Datum {
    Datum::I64(i64::from(value))
}

fn collect_rows(exec: &mut dyn Executor) -> Result<Vec<Vec<Datum>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = exec.next()? {
        for i in 0..chunk.num_rows() {
            rows.push(chunk.get_row(i));
        }
    }
    Ok(rows)
}

fn qualify(schema: &[String], table: &str) -> Vec<String> {
    schema
        .iter()
        .map(|col| {
            if col.contains('.') {
                col.clone()
            } else {
                format!("{table}.{col}")
            }
        })
        .collect()
}

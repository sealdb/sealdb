// core path: left-deep Inner/Left/Right/Semi/Anti HashJoin on equality,
// plus CROSS JOIN. Semi emits each left row once on match; Anti emits a left
// row when no match exists. Completeness wave / not this feature: full outer,
// USING/NATURAL, recursive spill/parallel workers, merge/index join,
// non-equality ON. CROSS JOIN materializes both sides in memory (no join-key
// partition spill).
use crate::internal::exec::Executor;
use crate::join::hash_join_spill::{partition_count, partition_rows};
use sealdb_errno::{ERR_BAD_FIELD, Error, Result};
use sealdb_expression::{consume_query_mem, eval_expr_with_row, row_mem_size, rows_mem_size};
use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode, ValueExpr};
use sealdb_parser::ast::{Join, JoinType};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;
use sealdb_util::chunk::{Chunk, mem_row_limit};
use std::collections::HashMap;

/// Two-table hash join. Inner/outer probe on equality keys; Semi/Anti emit
/// left rows only; CROSS JOIN is a full cartesian product and never
/// spill-partitions (no join key).
pub struct HashJoinV1Exec {
    join: Join,
    left: Box<dyn Executor>,
    right: Box<dyn Executor>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    mem_limit: usize,
    spilled: bool,
    emitted: bool,
    /// Qualified USING columns of the null-supplying side that the SELECT
    /// still references (`t2.a` above `t1 LEFT JOIN t2 USING(a)`). The join
    /// output carries them as hidden columns (`\u{1}`-prefixed names) so the
    /// parent filter/ordering sees the raw, non-coalesced value.
    hidden_using: Vec<String>,
    /// Coalesce USING/NATURAL columns into one output column. Disabled when the
    /// SELECT lists qualified stars (`t1.*, t2.*`), where TiDB returns every
    /// table's raw columns.
    coalesce_using: bool,
}

impl HashJoinV1Exec {
    pub fn new(join: Join, left: Box<dyn Executor>, right: Box<dyn Executor>) -> Self {
        Self {
            join,
            left,
            right,
            schema: Vec::new(),
            rows: Vec::new(),
            mem_limit: mem_row_limit(),
            spilled: false,
            emitted: false,
            hidden_using: Vec::new(),
            coalesce_using: true,
        }
    }

    /// Keep every raw column of both sides (qualified `t1.*` star expansion).
    pub fn with_coalesce_using(mut self, coalesce: bool) -> Self {
        self.coalesce_using = coalesce;
        self
    }

    /// Expose raw (non-coalesced) USING columns from the dropped side.
    pub fn with_hidden_using(mut self, cols: Vec<String>) -> Self {
        self.hidden_using = cols;
        self
    }

    pub fn with_mem_limit(mut self, mem_limit: usize) -> Self {
        self.mem_limit = mem_limit;
        self
    }

    pub fn spilled(&self) -> bool {
        self.spilled
    }
}

impl Executor for HashJoinV1Exec {
    fn open(&mut self) -> Result<()> {
        self.left.open()?;
        self.right.open()?;
        let left_raw = self.left.schema().to_vec();
        let right_raw = self.right.schema().to_vec();
        // Qualified copies are only for join-key / ON expression eval.
        let left_schema = qualify_schema(&left_raw, &self.join.left.name);
        let right_schema = qualify_schema(&right_raw, &self.join.right.name);
        let left_width = left_schema.len();
        let right_width = right_schema.len();
        validate_using_cols(&self.join.using_cols, &left_raw, &right_raw)?;
        // Semi/Anti keep the probe (left) schema so SELECT * FROM t1 WHERE EXISTS
        // still returns t1 columns rather than a concatenated join row.
        // Always keep `alias.col` on inner/cross joins so nested joins and
        // ORDER BY `cte_as_3.col_19` retain table identity (qualifying the whole
        // left with join.left.name would mis-label earlier tables). Display
        // strips to bare names in ExecutorBuilder::run (TiDB SELECT * headers).
        // USING coalesces shared names: emit left once, drop matching right cols.
        self.schema = if self.join.tp.is_semi_anti() {
            left_raw.clone()
        } else if self.join.using_cols.is_empty() || !self.coalesce_using {
            left_schema
                .iter()
                .cloned()
                .chain(right_schema.iter().cloned())
                .collect()
        } else if matches!(self.join.tp, JoinType::RightOuter) {
            coalesce_using_schema_right(&left_schema, &right_schema, &self.join.using_cols)
        } else {
            coalesce_using_schema(&left_schema, &right_schema, &self.join.using_cols)
        };
        let coalesce_right = matches!(self.join.tp, JoinType::RightOuter);
        // Hidden raw USING columns from the null-supplying side (`t2.a` above
        // `t1 LEFT JOIN t2 USING(a)`): parent filters need the pre-coalesced
        // value, but bare-name resolution and result headers must not see them.
        let mut hidden_idx: Vec<usize> = Vec::new();
        if self.coalesce_using && !self.join.using_cols.is_empty() {
            for qual in &self.hidden_using {
                let Some((_, bare)) = qual.rsplit_once('.') else {
                    continue;
                };
                let (raw, base) = if coalesce_right {
                    (&left_raw, 0usize)
                } else {
                    (&right_raw, left_width)
                };
                if let Some(i) = raw.iter().position(|c| {
                    c.rsplit_once('.')
                        .map(|(_, n)| n)
                        .unwrap_or(c)
                        .eq_ignore_ascii_case(bare)
                }) {
                    hidden_idx.push(base + i);
                    self.schema.push(format!("\u{1}{qual}"));
                }
            }
        }
        let left_rows = collect_rows(self.left.as_mut())?;
        let right_rows = collect_rows(self.right.as_mut())?;
        self.spilled = false;
        // `NOT IN` anti join: three-valued semantics — a NULL on either side
        // never proves the row absent (unlike NOT EXISTS anti joins).
        if self.join.tp == JoinType::Anti && contains_not_in_marker(&self.join.on) {
            let on = strip_not_in_marker(self.join.on.clone());
            let mut pair_schema = left_schema.clone();
            pair_schema.extend(right_schema.iter().cloned());
            self.rows = probe_anti_not_in(&left_rows, &right_rows, &pair_schema, &on)?;
            return Ok(());
        }
        // CROSS JOIN / non-equality ON (e.g. `ON (sub.col)` truthiness) →
        // cartesian product then filter; no hash keys.
        if self.join.tp == JoinType::Cross || !join_on_is_equijoin(&self.join.on) {
            let full_schema: Vec<String> = left_schema
                .iter()
                .cloned()
                .chain(right_schema.iter().cloned())
                .collect();
            // Semi/Anti keep only the probe columns: fold the cartesian product
            // back to the left row instead of emitting joined rows.
            if self.join.tp.is_semi_anti() {
                self.rows = probe_semi_anti_non_equi(
                    self.join.tp,
                    &left_rows,
                    &right_rows,
                    &full_schema,
                    &self.join.on,
                    left_width,
                )?;
                return Ok(());
            }
            if matches!(self.join.tp, JoinType::LeftOuter | JoinType::RightOuter)
                && !is_trivial_on_true(&self.join.on)
            {
                // No equality keys: evaluate ON per candidate pair, then
                // null-extend the preserved side when nothing matched.
                self.rows = probe_outer_non_equi(
                    self.join.tp,
                    &left_rows,
                    &right_rows,
                    &full_schema,
                    &self.join.on,
                    left_width,
                    right_width,
                )?;
            } else {
                self.rows = probe_cross_tracked(&left_rows, &right_rows)?;
                if !is_trivial_on_true(&self.join.on) {
                    self.rows = filter_rows_by_on(
                        std::mem::take(&mut self.rows),
                        &full_schema,
                        &self.join.on,
                    )?;
                }
            }
            if self.coalesce_using && !self.join.using_cols.is_empty() {
                self.rows = if coalesce_right {
                    project_using_rows_right(
                        std::mem::take(&mut self.rows),
                        &left_raw,
                        &right_raw,
                        &self.join.using_cols,
                        &hidden_idx,
                    )
                } else {
                    project_using_rows(
                        std::mem::take(&mut self.rows),
                        left_width,
                        &right_raw,
                        &self.join.using_cols,
                        &hidden_idx,
                    )
                };
            }
            return Ok(());
        }
        // The hash keys are the cross-side equality; every other conjunct
        // (`a.pk > b.pk`, single-side `t.a != 3`, ...) is a residual that must
        // hold for each candidate pair — otherwise an outer join would report
        // key-only matches.
        let (left_key, right_key, residual) = split_on_conditions(
            &self.join.on,
            &self.join.left.name,
            &self.join.right.name,
            &left_schema,
            &right_schema,
        )?;
        if left_rows.len() < self.mem_limit && right_rows.len() < self.mem_limit {
            self.rows = probe_join(
                self.join.tp,
                &left_rows,
                &right_rows,
                &left_schema,
                &right_schema,
                &left_key,
                &right_key,
                &residual,
                left_width,
                right_width,
            )?;
            if self.coalesce_using && !self.join.using_cols.is_empty() {
                self.rows = if coalesce_right {
                    project_using_rows_right(
                        std::mem::take(&mut self.rows),
                        &left_raw,
                        &right_raw,
                        &self.join.using_cols,
                        &hidden_idx,
                    )
                } else {
                    project_using_rows(
                        std::mem::take(&mut self.rows),
                        left_width,
                        &right_raw,
                        &self.join.using_cols,
                        &hidden_idx,
                    )
                };
            }
            // TiDB tracks the hash-join build side (the materialized table),
            // not the streaming probe output.
            consume_query_mem(rows_mem_size(&right_rows))?;
            return Ok(());
        }
        self.spilled = true;
        let mut left_parts = partition_rows(left_rows, &left_schema, &left_key, partition_count())?;
        let mut right_parts =
            partition_rows(right_rows, &right_schema, &right_key, partition_count())?;
        let mut out = Vec::new();
        for (left_part, right_part) in left_parts.iter_mut().zip(right_parts.iter_mut()) {
            let part_left = left_part.read_all()?;
            let part_right = right_part.read_all()?;
            out.extend(probe_join(
                self.join.tp,
                &part_left,
                &part_right,
                &left_schema,
                &right_schema,
                &left_key,
                &right_key,
                &residual,
                left_width,
                right_width,
            )?);
        }
        self.rows = out;
        if self.coalesce_using && !self.join.using_cols.is_empty() {
            self.rows = if coalesce_right {
                project_using_rows_right(
                    std::mem::take(&mut self.rows),
                    &left_raw,
                    &right_raw,
                    &self.join.using_cols,
                    &hidden_idx,
                )
            } else {
                project_using_rows(
                    std::mem::take(&mut self.rows),
                    left_width,
                    &right_raw,
                    &self.join.using_cols,
                    &hidden_idx,
                )
            };
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
        self.left.close()?;
        self.right.close()
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_join(
    tp: JoinType,
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    left_schema: &[String],
    right_schema: &[String],
    left_key: &ExprNode,
    right_key: &ExprNode,
    residual: &[&ExprNode],
    left_width: usize,
    right_width: usize,
) -> Result<Vec<Vec<Datum>>> {
    match tp {
        JoinType::Inner => probe_inner(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
            residual,
        ),
        JoinType::LeftOuter => probe_outer(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
            residual,
            left_width,
            right_width,
            true,
        ),
        // Probe is the preserved right side; output rows stay `[left, right]`,
        // so an unmatched row null-pads the *build* (original left) width.
        JoinType::RightOuter => probe_outer(
            right_rows,
            left_rows,
            right_schema,
            left_schema,
            right_key,
            left_key,
            residual,
            left_width,
            right_width,
            false,
        ),
        JoinType::Cross => Ok(probe_cross(left_rows, right_rows)),
        JoinType::Semi => probe_semi(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
            residual,
        ),
        JoinType::Anti => probe_anti(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
            residual,
        ),
    }
}

/// Emit each left row once when any right row shares the join key (EXISTS / IN).
fn probe_semi(
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    left_schema: &[String],
    right_schema: &[String],
    left_key: &ExprNode,
    right_key: &ExprNode,
    residual: &[&ExprNode],
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(right_rows, right_schema, right_key)?;
    let mut pair_schema = left_schema.to_vec();
    pair_schema.extend(right_schema.iter().cloned());
    let mut out = Vec::new();
    for left in left_rows {
        let key = eval_expr_with_row(left_key, left_schema, left)?;
        if key.is_null() {
            continue;
        }
        let matched = {
            let mut any = false;
            for ri in matched_ids(&index, &key, right_rows, right_schema, right_key)? {
                let right = &right_rows[ri];
                if residual_passes(residual, &pair_schema, left, right)? {
                    any = true;
                    break;
                }
            }
            any
        };
        if matched {
            out.push(left.clone());
        }
    }
    Ok(out)
}

/// `Bytes(NOT_IN_MARKER)` conjunct tagging an anti join as `NOT IN`.
fn is_not_in_marker(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Value(ValueExpr {
            datum: Datum::Bytes(b),
        }) if b.as_slice() == sealdb_planner::core::rule_semi_join::NOT_IN_MARKER.as_bytes()
    )
}

fn contains_not_in_marker(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            contains_not_in_marker(&bin.left) || contains_not_in_marker(&bin.right)
        }
        _ => is_not_in_marker(expr),
    }
}

/// Drop the `NOT IN` marker conjuncts, keeping the executable predicate.
fn strip_not_in_marker(expr: ExprNode) -> ExprNode {
    match expr {
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            let left = strip_not_in_marker(*bin.left);
            let right = strip_not_in_marker(*bin.right);
            match (keep_marker(&left), keep_marker(&right)) {
                (None, None) => ExprNode::Value(ValueExpr {
                    datum: Datum::I64(1),
                }),
                (None, Some(r)) => r,
                (Some(l), None) => l,
                (Some(l), Some(r)) => ExprNode::Binary(BinaryOpExpr {
                    op: Op::LogicAnd,
                    left: Box::new(l),
                    right: Box::new(r),
                    single_cnf_item: false,
                }),
            }
        }
        other => other,
    }
}

fn keep_marker(expr: &ExprNode) -> Option<ExprNode> {
    if is_not_in_marker(expr) {
        None
    } else {
        Some(expr.clone())
    }
}

/// `NOT IN` anti join: keep the left row only when every candidate pair
/// evaluates to FALSE (a NULL comparison or TRUE match drops it).
fn probe_anti_not_in(
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    pair_schema: &[String],
    on: &ExprNode,
) -> Result<Vec<Vec<Datum>>> {
    let mut out = Vec::new();
    for left in left_rows {
        let mut keep = true;
        for right in right_rows {
            let mut row = left.clone();
            row.extend(right.iter().cloned());
            let v = eval_expr_with_row(on, pair_schema, &row)?;
            let trueish = match &v {
                Datum::Null => true,
                Datum::I64(n) => *n != 0,
                Datum::U64(n) => *n != 0,
                Datum::F64(f) => *f != 0.0,
                Datum::Bytes(b) => !b.is_empty(),
                _ => false,
            };
            if trueish {
                keep = false;
                break;
            }
        }
        if keep {
            out.push(left.clone());
        }
    }
    Ok(out)
}

/// Emit a left row when no right row shares the join key (NOT EXISTS).
fn probe_anti(
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    left_schema: &[String],
    right_schema: &[String],
    left_key: &ExprNode,
    right_key: &ExprNode,
    residual: &[&ExprNode],
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(right_rows, right_schema, right_key)?;
    let mut pair_schema = left_schema.to_vec();
    pair_schema.extend(right_schema.iter().cloned());
    let mut out = Vec::new();
    for left in left_rows {
        let key = eval_expr_with_row(left_key, left_schema, left)?;
        let mut matched = false;
        if !key.is_null() {
            for ri in matched_ids(&index, &key, right_rows, right_schema, right_key)? {
                let right = &right_rows[ri];
                if residual_passes(residual, &pair_schema, left, right)? {
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push(left.clone());
        }
    }
    Ok(out)
}

/// Concatenate every left row with every right row (CROSS JOIN / comma join).
///
/// TiDB scans the smaller join side into the hash table (build) and lets the
/// other side (probe) drive the output loop; bucket chains are prepended, so
/// matched build rows come back in reverse insertion order. With equal sizes
/// the inner/right side is the build side.
/// Cartesian product charged against `tidb_mem_quota_query` row by row, so a
/// runaway cross join (TiDB integrationtest `TestCartesianJoinPanic`) aborts
/// with error 8175 instead of exhausting memory.
fn probe_cross_tracked(
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
) -> Result<Vec<Vec<Datum>>> {
    let mut out = Vec::with_capacity(left_rows.len().saturating_mul(right_rows.len()));
    let build_is_left = left_rows.len() < right_rows.len();
    let mut push = |row: Vec<Datum>| -> Result<()> {
        consume_query_mem(row_mem_size(&row))?;
        out.push(row);
        Ok(())
    };
    if build_is_left {
        for right in right_rows {
            for left in left_rows.iter().rev() {
                push(concat_row(left, right))?;
            }
        }
    } else {
        for left in left_rows {
            for right in right_rows.iter().rev() {
                push(concat_row(left, right))?;
            }
        }
    }
    Ok(out)
}

fn probe_cross(left_rows: &[Vec<Datum>], right_rows: &[Vec<Datum>]) -> Vec<Vec<Datum>> {
    let mut out = Vec::with_capacity(left_rows.len().saturating_mul(right_rows.len()));
    let build_is_left = left_rows.len() < right_rows.len();
    if build_is_left {
        for right in right_rows {
            for left in left_rows.iter().rev() {
                out.push(concat_row(left, right));
            }
        }
    } else {
        for left in left_rows {
            for right in right_rows.iter().rev() {
                out.push(concat_row(left, right));
            }
        }
    }
    out
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

fn qualify_schema(schema: &[String], table: &str) -> Vec<String> {
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

fn bare_col(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn validate_using_cols(using: &[String], left: &[String], right: &[String]) -> Result<()> {
    for col in using {
        let in_left = left
            .iter()
            .any(|c| bare_col(c).eq_ignore_ascii_case(col));
        let in_right = right
            .iter()
            .any(|c| bare_col(c).eq_ignore_ascii_case(col));
        if !in_left || !in_right {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{col}' in 'from clause'"),
            ));
        }
    }
    Ok(())
}

fn coalesce_using_schema(left: &[String], right: &[String], using: &[String]) -> Vec<String> {
    let drop_right: std::collections::HashSet<String> = using
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    left.iter()
        .cloned()
        .chain(
            right
                .iter()
                .filter(|c| !drop_right.contains(&bare_col(c).to_ascii_lowercase()))
                .cloned(),
        )
        .collect()
}

/// RIGHT JOIN counterpart: the preserved (right) side owns the USING columns,
/// so its columns come first and the left duplicates are dropped
/// (`t1(a,c) RIGHT JOIN t2(a,d) USING(a)` → `t2.a, t2.d, t1.c`).
fn coalesce_using_schema_right(left: &[String], right: &[String], using: &[String]) -> Vec<String> {
    let drop_left: std::collections::HashSet<String> = using
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    right
        .iter()
        .cloned()
        .chain(
            left.iter()
                .filter(|c| !drop_left.contains(&bare_col(c).to_ascii_lowercase()))
                .cloned(),
        )
        .collect()
}

fn project_using_rows(
    rows: Vec<Vec<Datum>>,
    left_width: usize,
    right: &[String],
    using: &[String],
    hidden: &[usize],
) -> Vec<Vec<Datum>> {
    let drop_right: std::collections::HashSet<String> = using
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let keep: Vec<usize> = (0..left_width)
        .chain((0..right.len()).filter_map(|i| {
            if drop_right.contains(&bare_col(&right[i]).to_ascii_lowercase()) {
                None
            } else {
                Some(left_width + i)
            }
        }))
        .collect();
    rows.into_iter()
        .map(|row| {
            let mut out: Vec<Datum> = keep
                .iter()
                .map(|&i| row.get(i).cloned().unwrap_or(Datum::Null))
                .collect();
            out.extend(hidden.iter().map(|&i| row.get(i).cloned().unwrap_or(Datum::Null)));
            out
        })
        .collect()
}

/// Rows for a RIGHT JOIN over USING columns: preserved side first.
fn project_using_rows_right(
    rows: Vec<Vec<Datum>>,
    left: &[String],
    right: &[String],
    using: &[String],
    hidden: &[usize],
) -> Vec<Vec<Datum>> {
    let drop_left: std::collections::HashSet<String> = using
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let keep: Vec<usize> = (0..right.len())
        .map(|i| left.len() + i)
        .chain((0..left.len()).filter(|&i| {
            !drop_left.contains(&bare_col(&left[i]).to_ascii_lowercase())
        }))
        .collect();
    rows.into_iter()
        .map(|row| {
            let mut out: Vec<Datum> = keep
                .iter()
                .map(|&i| row.get(i).cloned().unwrap_or(Datum::Null))
                .collect();
            out.extend(hidden.iter().map(|&i| row.get(i).cloned().unwrap_or(Datum::Null)));
            out
        })
        .collect()
}

fn join_on_is_equijoin(on: &ExprNode) -> bool {
    match on {
        ExprNode::Binary(bin) if bin.op == Op::Eq => {
            matches!(
                (bin.left.as_ref(), bin.right.as_ref()),
                (ExprNode::Column(_), ExprNode::Column(_))
            )
        }
        ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
            join_on_is_equijoin(&bin.left) || join_on_is_equijoin(&bin.right)
        }
        _ => false,
    }
}

/// Split `ON` into the hash key pair plus the residual conjuncts that must
/// hold for every candidate pair. ExplainInfo byte markers (the decorated
/// semi/anti join info string) are not predicates and are dropped.
fn split_on_conditions<'a>(
    on: &'a ExprNode,
    left_table: &str,
    right_table: &str,
    left_schema: &[String],
    right_schema: &[String],
) -> Result<(ExprNode, ExprNode, Vec<&'a ExprNode>)> {
    let mut conjuncts = Vec::new();
    on_conjuncts(on, &mut conjuncts);
    let mut key = None;
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if key.is_none() {
            // A parenthesized table group on either side exposes several table
            // qualifiers, so classify by the child schemas first.
            if let Ok(keys) =
                split_join_eq_by_schema(conjunct, left_schema, right_schema)
            {
                key = Some(keys);
                continue;
            }
            if let Ok(keys) = split_join_eq(conjunct, left_table, right_table) {
                key = Some(keys);
                continue;
            }
        }
        residual.push(conjunct);
    }
    let (left_key, right_key) = key.ok_or_else(|| {
        Error::unsupported("JOIN ON must compare columns from both tables")
    })?;
    Ok((left_key, right_key, residual))
}

/// `true` = accumulated left side, `false` = the right subtree.
fn side_in_schema(
    expr: &ExprNode,
    left_schema: &[String],
    right_schema: &[String],
) -> Option<bool> {
    let ExprNode::Column(col) = expr else {
        return None;
    };
    if let Some(table) = col.table.as_deref() {
        if schema_has_table(right_schema, table) {
            return Some(false);
        }
        if schema_has_table(left_schema, table) {
            return Some(true);
        }
        return None;
    }
    // Unqualified `ON b1 = a1`: resolve by which side owns the name.
    let in_left = schema_has_column(left_schema, &col.name);
    let in_right = schema_has_column(right_schema, &col.name);
    match (in_left, in_right) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

fn schema_has_column(schema: &[String], column: &str) -> bool {
    schema
        .iter()
        .any(|c| bare_col(c).eq_ignore_ascii_case(column))
}

fn schema_has_table(schema: &[String], table: &str) -> bool {
    schema
        .iter()
        .filter_map(|c| c.rsplit_once('.').map(|(t, _)| t))
        .any(|t| t.eq_ignore_ascii_case(table))
}

fn split_join_eq_by_schema(
    on: &ExprNode,
    left_schema: &[String],
    right_schema: &[String],
) -> Result<(ExprNode, ExprNode)> {
    let ExprNode::Binary(bin) = on else {
        return Err(Error::unsupported("JOIN ON must be equality"));
    };
    if bin.op != Op::Eq {
        return Err(Error::unsupported("JOIN ON must be equality"));
    }
    match (
        side_in_schema(&bin.left, left_schema, right_schema),
        side_in_schema(&bin.right, left_schema, right_schema),
    ) {
        (Some(true), Some(false)) => Ok((*bin.left.clone(), *bin.right.clone())),
        (Some(false), Some(true)) => Ok((*bin.right.clone(), *bin.left.clone())),
        _ => Err(Error::unsupported(
            "JOIN ON must compare columns from both tables",
        )),
    }
}

fn on_conjuncts<'a>(on: &'a ExprNode, out: &mut Vec<&'a ExprNode>) {
    if let ExprNode::Binary(bin) = on
        && bin.op == Op::LogicAnd
    {
        on_conjuncts(&bin.left, out);
        on_conjuncts(&bin.right, out);
        return;
    }
    if matches!(on, ExprNode::Value(v) if matches!(&v.datum, Datum::Bytes(_))) {
        return;
    }
    out.push(on);
}

/// Every residual conjunct must evaluate to true for the candidate pair.
fn residual_passes(
    residual: &[&ExprNode],
    schema: &[String],
    left: &[Datum],
    right: &[Datum],
) -> Result<bool> {
    if residual.is_empty() {
        return Ok(true);
    }
    let row = concat_row(left, right);
    for expr in residual {
        if !row_passes_on(&row, schema, expr)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_trivial_on_true(on: &ExprNode) -> bool {
    matches!(
        on,
        ExprNode::Value(v) if matches!(v.datum, Datum::I64(1) | Datum::U64(1))
    )
}

/// Outer join over a non-equality `ON` (no hash keys): for every preserved
/// row, keep the pairs that satisfy `ON`; when none do, emit the row padded
/// with NULLs for the other side (e.g. `(t1 ⟕ t2) ⟕ (t3 ⟕ t4) ON t2.b = 1`).
fn probe_outer_non_equi(
    tp: JoinType,
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    full_schema: &[String],
    on: &ExprNode,
    left_width: usize,
    right_width: usize,
) -> Result<Vec<Vec<Datum>>> {
    let mut out = Vec::new();
    if tp == JoinType::RightOuter {
        for right in right_rows {
            let mut matched = false;
            for left in left_rows {
                let row = concat_row(left, right);
                if row_passes_on(&row, full_schema, on)? {
                    matched = true;
                    out.push(row);
                }
            }
            if !matched {
                out.push(concat_row(&vec![Datum::Null; left_width], right));
            }
        }
        return Ok(out);
    }
    for left in left_rows {
        let mut matched = false;
        for right in right_rows {
            let row = concat_row(left, right);
            if row_passes_on(&row, full_schema, on)? {
                matched = true;
                out.push(row);
            }
        }
        if !matched {
            out.push(concat_row(left, &vec![Datum::Null; right_width]));
        }
    }
    Ok(out)
}

/// Semi/Anti over a non-equality `ON`: cartesian product, then keep the left
/// row itself (Semi: any right row matches; Anti: none does). TiDB evaluates
/// these as an Apply; the emitted schema is still the probe side only.
fn probe_semi_anti_non_equi(
    tp: JoinType,
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    full_schema: &[String],
    on: &ExprNode,
    left_width: usize,
) -> Result<Vec<Vec<Datum>>> {
    let trivial = is_trivial_on_true(on);
    let mut out = Vec::new();
    for left in left_rows {
        let mut matched = trivial && !right_rows.is_empty();
        if !trivial {
            for right in right_rows {
                let mut row = left.clone();
                row.extend(right.iter().cloned());
                if row_passes_on(&row, full_schema, on)? {
                    matched = true;
                    break;
                }
            }
        }
        let keep = match tp {
            JoinType::Semi => matched,
            JoinType::Anti => !matched,
            _ => false,
        };
        if keep {
            out.push(left[..left_width.min(left.len())].to_vec());
        }
    }
    Ok(out)
}

fn filter_rows_by_on(
    rows: Vec<Vec<Datum>>,
    schema: &[String],
    on: &ExprNode,
) -> Result<Vec<Vec<Datum>>> {
    let mut out = Vec::new();
    for row in rows {
        if row_passes_on(&row, schema, on)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Evaluate a (non-equality) join `ON` predicate against one joined row.
fn row_passes_on(row: &[Datum], schema: &[String], on: &ExprNode) -> Result<bool> {
    let v = eval_expr_with_row(on, schema, row)?;
    if v.is_null() {
        return Ok(false);
    }
    Ok(match v {
        Datum::I64(n) => n != 0,
        Datum::U64(n) => n != 0,
        Datum::Bytes(b) => !b.is_empty(),
        _ => false,
    })
}

/// `JOIN ... ON TRUE` is a comma/cross join (string-rewritten UPDATE joins).
fn join_on_is_true(on: &ExprNode) -> bool {
    matches!(on, ExprNode::Value(v) if matches!(v.datum, Datum::I64(n) if n != 0))
}

fn split_join_eq(
    on: &ExprNode,
    left_table: &str,
    right_table: &str,
) -> Result<(ExprNode, ExprNode)> {
    // Peel LogicAnd wrappers that carry an ExplainInfo Bytes marker alongside Eq.
    if let ExprNode::Binary(bin) = on
        && bin.op == Op::LogicAnd
    {
        if let Ok(keys) = split_join_eq(&bin.left, left_table, right_table) {
            return Ok(keys);
        }
        return split_join_eq(&bin.right, left_table, right_table);
    }
    let ExprNode::Binary(bin) = on else {
        return Err(Error::unsupported("JOIN ON must be equality"));
    };
    if bin.op != Op::Eq {
        return Err(Error::unsupported("JOIN ON must be equality"));
    }
    match (
        side_of(&bin.left, left_table, right_table),
        side_of(&bin.right, left_table, right_table),
    ) {
        (Some(true), Some(false)) => Ok((*bin.left.clone(), *bin.right.clone())),
        (Some(false), Some(true)) => Ok((*bin.right.clone(), *bin.left.clone())),
        _ => Err(Error::unsupported(
            "JOIN ON must compare columns from both tables",
        )),
    }
}

/// `true` = accumulated left side, `false` = the new right table.
/// For n-table joins any table other than `right_table` lives on the left.
fn side_of(expr: &ExprNode, _left_table: &str, right_table: &str) -> Option<bool> {
    let ExprNode::Column(col) = expr else {
        return None;
    };
    match col.table.as_deref() {
        Some(table) if table.eq_ignore_ascii_case(right_table) => Some(false),
        Some(_) => Some(true),
        None => None,
    }
}

fn probe_inner(
    probe_rows: &[Vec<Datum>],
    build_rows: &[Vec<Datum>],
    probe_schema: &[String],
    build_schema: &[String],
    probe_key: &ExprNode,
    build_key: &ExprNode,
    residual: &[&ExprNode],
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(build_rows, build_schema, build_key)?;
    let mut pair_schema = probe_schema.to_vec();
    pair_schema.extend(build_schema.iter().cloned());
    let mut out = Vec::new();
    for probe in probe_rows {
        let key = eval_expr_with_row(probe_key, probe_schema, probe)?;
        if key.is_null() {
            continue;
        }
        for bi in matched_ids(&index, &key, build_rows, build_schema, build_key)? {
            let build = &build_rows[bi];
            if residual_passes(residual, &pair_schema, probe, build)? {
                out.push(concat_row(probe, build));
            }
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn probe_outer(
    outer_rows: &[Vec<Datum>],
    inner_rows: &[Vec<Datum>],
    outer_schema: &[String],
    inner_schema: &[String],
    outer_key: &ExprNode,
    inner_key: &ExprNode,
    residual: &[&ExprNode],
    left_width: usize,
    right_width: usize,
    outer_is_left: bool,
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(inner_rows, inner_schema, inner_key)?;
    // Row order is always `[left, right]`, even when the outer side is the
    // right input, so the residual schema must follow that order too.
    let pair_schema: Vec<String> = if outer_is_left {
        outer_schema.iter().chain(inner_schema).cloned().collect()
    } else {
        inner_schema.iter().chain(outer_schema).cloned().collect()
    };
    let mut out = Vec::new();
    for outer in outer_rows {
        let key = eval_expr_with_row(outer_key, outer_schema, outer)?;
        let mut matched = false;
        if !key.is_null() {
            for ii in matched_ids(&index, &key, inner_rows, inner_schema, inner_key)? {
                let inner = &inner_rows[ii];
                let (left, right) = if outer_is_left {
                    (outer, inner)
                } else {
                    (inner, outer)
                };
                if !residual_passes(residual, &pair_schema, left, right)? {
                    continue;
                }
                matched = true;
                out.push(concat_row(left, right));
            }
        }
        if !matched {
            out.push(if outer_is_left {
                concat_row(outer, &vec![Datum::Null; right_width])
            } else {
                concat_row(&vec![Datum::Null; left_width], outer)
            });
        }
    }
    Ok(out)
}

fn build_index(
    rows: &[Vec<Datum>],
    schema: &[String],
    key: &ExprNode,
) -> Result<HashMap<String, Vec<usize>>> {
    build_index_ids(rows, schema, key)
}

fn concat_row(left: &[Datum], right: &[Datum]) -> Vec<Datum> {
    let mut row = left.to_vec();
    row.extend_from_slice(right);
    row
}

/// Canonical join-key text. Equality on TIMESTAMP/DATETIME/DATE/string columns
/// follows SQL comparison rules (session-local wall clock), so the hash key
/// must not depend on the `Time` type tag nor on whether the value arrived as
/// a string. Only real values key together: `Time` renders as a local
/// `DATETIME`, and bytes that parse as a timestamp literal share that form.
fn key_text(value: &Datum) -> String {
    match value {
        Datum::Time(t) => time_key_text(*t),
        Datum::Bytes(b) => {
            if let Ok(s) = std::str::from_utf8(b) {
                if let Ok(t) = sealdb_types::time::parse_timestamp_literal(s) {
                    return time_key_text(t);
                }
            }
            format!("{value:?}")
        }
        // Numeric keys compare by value, not representation: a join of
        // `int 0` with `decimal(4,2) 0.00` must match.
        Datum::I64(v) => format!("num:{v}"),
        Datum::U64(v) => format!("num:{v}"),
        Datum::F64(v) if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e18 => {
            format!("num:{}", *v as i64)
        }
        Datum::Decimal(d) => {
            let mut s = d.to_string();
            if s.contains('.') {
                while s.ends_with('0') {
                    s.pop();
                }
                if s.ends_with('.') {
                    s.pop();
                }
            }
            if s == "-0" {
                s = "0".to_string();
            }
            format!("num:{s}")
        }
        // ENUM/SET joins by label; the numeric form is added as a secondary
        // key so enum ↔ number comparisons (member index / bitmask) hit too.
        Datum::Enum(e) => key_text(&Datum::Bytes(e.label.clone())),
        _ => format!("{value:?}"),
    }
}

/// Hash keys a build-side value is indexed under. ENUM/SET are indexed by
/// both their label and their numeric form so `enum_col = int_col` matches
/// while enum ↔ enum still compares labels (verified before a pair is joined).
fn index_keys(value: &Datum) -> Vec<String> {
    match value {
        Datum::Enum(e) => vec![
            key_text(&Datum::Bytes(e.label.clone())),
            format!("num:{}", e.number),
        ],
        other => vec![key_text(other)],
    }
}

/// Hash keys to look up for a probe value (same forms as `index_keys`).
fn probe_keys(value: &Datum) -> Vec<String> {
    index_keys(value)
}

/// Unique build-row offsets whose join key may equal `key`. ENUM/SET probes
/// re-check SQL equality so enum labels from different declarations never
/// join just because their member indexes coincide.
fn matched_ids(
    index: &HashMap<String, Vec<usize>>,
    key: &Datum,
    build_rows: &[Vec<Datum>],
    build_schema: &[String],
    build_key: &ExprNode,
) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for k in probe_keys(key) {
        let Some(ids) = index.get(&k) else {
            continue;
        };
        for &i in ids {
            if !seen.insert(i) {
                continue;
            }
            if matches!(key, Datum::Enum(_)) {
                let build_value = eval_expr_with_row(build_key, build_schema, &build_rows[i])?;
                if !sealdb_expression::datum_sql_eq(key, &build_value)? {
                    continue;
                }
            }
            out.push(i);
        }
    }
    Ok(out)
}

fn time_key_text(mut t: sealdb_types::time::Time) -> String {
    if matches!(t.tp, sealdb_types::time::TimeType::Timestamp) {
        if let Ok(local) = sealdb_expression::utc_to_session_local(t) {
            t = local;
        }
    }
    format!(
        "time:{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.micro
    )
}

/// Join-side index pairs for DML. `None` marks an unmatched outer row so UPDATE /
/// DELETE can skip that side. CROSS JOIN returns the full cartesian of indices.
pub(crate) fn hash_join_pairs(
    join: &Join,
    left_schema: &[String],
    right_schema: &[String],
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
) -> Result<Vec<(Option<usize>, Option<usize>)>> {
    if join.tp == JoinType::Cross || join_on_is_true(&join.on) {
        let mut out = Vec::with_capacity(left_rows.len().saturating_mul(right_rows.len()));
        for i in 0..left_rows.len() {
            for j in 0..right_rows.len() {
                out.push((Some(i), Some(j)));
            }
        }
        return Ok(out);
    }
    let left_q = qualify_schema(left_schema, &join.left.name);
    let right_q = qualify_schema(right_schema, &join.right.name);
    let (left_key, right_key) = split_join_eq(&join.on, &join.left.name, &join.right.name)?;
    match join.tp {
        JoinType::Inner => probe_inner_pairs(
            left_rows, right_rows, &left_q, &right_q, &left_key, &right_key,
        )
        .map(|pairs| pairs.into_iter().map(|(l, r)| (Some(l), Some(r))).collect()),
        JoinType::LeftOuter => probe_outer_pairs(
            left_rows, right_rows, &left_q, &right_q, &left_key, &right_key, true,
        ),
        JoinType::RightOuter => probe_outer_pairs(
            right_rows, left_rows, &right_q, &left_q, &right_key, &left_key, false,
        ),
        JoinType::Cross => unreachable!("cross join pairs handled above"),
        JoinType::Semi => probe_semi_pairs(
            left_rows, right_rows, &left_q, &right_q, &left_key, &right_key, true,
        ),
        JoinType::Anti => probe_semi_pairs(
            left_rows, right_rows, &left_q, &right_q, &left_key, &right_key, false,
        ),
    }
}

/// Semi (`matched`) / anti (`!matched`) DML pairs keep the left index only.
fn probe_semi_pairs(
    left_rows: &[Vec<Datum>],
    right_rows: &[Vec<Datum>],
    left_schema: &[String],
    right_schema: &[String],
    left_key: &ExprNode,
    right_key: &ExprNode,
    matched: bool,
) -> Result<Vec<(Option<usize>, Option<usize>)>> {
    let index = build_index_ids(right_rows, right_schema, right_key)?;
    let mut out = Vec::new();
    for (left_i, left) in left_rows.iter().enumerate() {
        let key = eval_expr_with_row(left_key, left_schema, left)?;
        let hit = !key.is_null() && index.contains_key(&key_text(&key));
        if hit == matched {
            out.push((Some(left_i), None));
        }
    }
    Ok(out)
}

/// Inner-join `(probe_i, build_i)` pairs using the same hash index as `probe_inner`.
fn probe_inner_pairs(
    probe_rows: &[Vec<Datum>],
    build_rows: &[Vec<Datum>],
    probe_schema: &[String],
    build_schema: &[String],
    probe_key: &ExprNode,
    build_key: &ExprNode,
) -> Result<Vec<(usize, usize)>> {
    let index = build_index_ids(build_rows, build_schema, build_key)?;
    let mut out = Vec::new();
    for (probe_i, probe) in probe_rows.iter().enumerate() {
        let key = eval_expr_with_row(probe_key, probe_schema, probe)?;
        if key.is_null() {
            continue;
        }
        for bi in matched_ids(&index, &key, build_rows, build_schema, build_key)? {
            out.push((probe_i, bi));
        }
    }
    Ok(out)
}

/// Left or right outer pairs. Unmatched outer rows use `None` on the inner side.
fn probe_outer_pairs(
    outer_rows: &[Vec<Datum>],
    inner_rows: &[Vec<Datum>],
    outer_schema: &[String],
    inner_schema: &[String],
    outer_key: &ExprNode,
    inner_key: &ExprNode,
    outer_is_left: bool,
) -> Result<Vec<(Option<usize>, Option<usize>)>> {
    let index = build_index_ids(inner_rows, inner_schema, inner_key)?;
    let mut out = Vec::new();
    for (outer_i, outer) in outer_rows.iter().enumerate() {
        let key = eval_expr_with_row(outer_key, outer_schema, outer)?;
        let mut matched = false;
        if !key.is_null() {
            for inner_i in matched_ids(&index, &key, inner_rows, inner_schema, inner_key)? {
                matched = true;
                out.push(if outer_is_left {
                    (Some(outer_i), Some(inner_i))
                } else {
                    (Some(inner_i), Some(outer_i))
                });
            }
        }
        if !matched {
            out.push(if outer_is_left {
                (Some(outer_i), None)
            } else {
                (None, Some(outer_i))
            });
        }
    }
    Ok(out)
}

/// Hash index from join-key text to row offsets (for DML pair probes).
fn build_index_ids(
    rows: &[Vec<Datum>],
    schema: &[String],
    key: &ExprNode,
) -> Result<HashMap<String, Vec<usize>>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let value = eval_expr_with_row(key, schema, row)?;
        if value.is_null() {
            continue;
        }
        for k in index_keys(&value) {
            index.entry(k).or_default().push(i);
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::exec::{Executor, VecExec};
    use sealdb_parser::ast::ddl::TableName;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr};

    fn inner_join() -> Join {
        Join {
            left: TableName::new(None, "t1"),
            right: TableName::new(None, "t2"),
            tp: JoinType::Inner,
            on: ExprNode::Binary(BinaryOpExpr {
                op: Op::Eq,
                left: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some("t1".to_string()),
                    name: "id".to_string(),
                })),
                right: Box::new(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some("t2".to_string()),
                    name: "id".to_string(),
                })),
                single_cnf_item: false,
            }),
            using_cols: Vec::new(),
            natural: false,
            straight: false,
        }
    }

    #[test]
    fn test_hash_join_spill_matches_in_memory() {
        let left_rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())],
            vec![Datum::I64(3), Datum::Bytes(b"c".to_vec())],
        ];
        let right_rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"x".to_vec())],
            vec![Datum::I64(3), Datum::Bytes(b"y".to_vec())],
            vec![Datum::I64(9), Datum::Bytes(b"z".to_vec())],
        ];
        let left_schema = vec!["id".to_string(), "name".to_string()];
        let right_schema = vec!["id".to_string(), "tag".to_string()];
        let mut mem = HashJoinV1Exec::new(
            inner_join(),
            Box::new(VecExec::new(left_schema.clone(), left_rows.clone())),
            Box::new(VecExec::new(right_schema.clone(), right_rows.clone())),
        )
        .with_mem_limit(usize::MAX);
        mem.open().unwrap();
        let mem_chunk = mem.next().unwrap().unwrap();
        let mut spilled = HashJoinV1Exec::new(
            inner_join(),
            Box::new(VecExec::new(left_schema, left_rows)),
            Box::new(VecExec::new(right_schema, right_rows)),
        )
        .with_mem_limit(1);
        spilled.open().unwrap();
        let spilled_chunk = spilled.next().unwrap().unwrap();
        assert!(spilled.spilled());
        let mut mem_out = (0..mem_chunk.num_rows())
            .map(|i| mem_chunk.get_row(i))
            .collect::<Vec<_>>();
        let mut spilled_out = (0..spilled_chunk.num_rows())
            .map(|i| spilled_chunk.get_row(i))
            .collect::<Vec<_>>();
        mem_out.sort_by(|a, b| a[0].compare(&b[0]));
        spilled_out.sort_by(|a, b| a[0].compare(&b[0]));
        assert_eq!(mem_out, spilled_out);
        assert_eq!(mem_out.len(), 2);
    }

    fn join_of(tp: JoinType) -> Join {
        let mut join = inner_join();
        join.tp = tp;
        join
    }

    #[test]
    fn test_semi_and_anti_emit_left_only() {
        let left_rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())],
        ];
        let right_rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"x".to_vec())],
            vec![Datum::I64(1), Datum::Bytes(b"z".to_vec())],
            vec![Datum::I64(9), Datum::Bytes(b"y".to_vec())],
        ];
        let left_schema = vec!["id".to_string(), "name".to_string()];
        let right_schema = vec!["id".to_string(), "tag".to_string()];
        let mut semi = HashJoinV1Exec::new(
            join_of(JoinType::Semi),
            Box::new(VecExec::new(left_schema.clone(), left_rows.clone())),
            Box::new(VecExec::new(right_schema.clone(), right_rows.clone())),
        );
        semi.open().unwrap();
        let chunk = semi.next().unwrap().unwrap();
        assert_eq!(semi.schema(), &["id".to_string(), "name".to_string()]);
        assert_eq!(chunk.num_rows(), 1);
        assert_eq!(
            chunk.get_row(0),
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())]
        );
        let mut anti = HashJoinV1Exec::new(
            join_of(JoinType::Anti),
            Box::new(VecExec::new(left_schema, left_rows)),
            Box::new(VecExec::new(right_schema, right_rows)),
        );
        anti.open().unwrap();
        let chunk = anti.next().unwrap().unwrap();
        assert_eq!(chunk.num_rows(), 1);
        assert_eq!(
            chunk.get_row(0),
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())]
        );
    }
}

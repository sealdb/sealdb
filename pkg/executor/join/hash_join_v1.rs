// core path: left-deep Inner/Left/Right/Semi/Anti HashJoin on equality,
// plus CROSS JOIN. Semi emits each left row once on match; Anti emits a left
// row when no match exists. Completeness wave / not this feature: full outer,
// USING/NATURAL, recursive spill/parallel workers, merge/index join,
// non-equality ON. CROSS JOIN materializes both sides in memory (no join-key
// partition spill).
use crate::internal::exec::Executor;
use crate::join::hash_join_spill::{partition_count, partition_rows};
use sealdb_errno::{ERR_BAD_FIELD, Error, Result};
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::expressions::ExprNode;
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
        }
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
        // TiDB SELECT * headers use bare column names (duplicates allowed at
        // top level; derived tables reject them with errno 1060).
        // USING coalesces shared names: emit left once, drop matching right cols.
        self.schema = if self.join.tp.is_semi_anti() {
            left_raw.clone()
        } else if self.join.using_cols.is_empty() {
            left_raw
                .iter()
                .cloned()
                .chain(right_raw.iter().cloned())
                .collect()
        } else {
            coalesce_using_schema(&left_raw, &right_raw, &self.join.using_cols)
        };
        let left_rows = collect_rows(self.left.as_mut())?;
        let right_rows = collect_rows(self.right.as_mut())?;
        self.spilled = false;
        // CROSS JOIN has no equality key, so skip hash index and partition spill.
        if self.join.tp == JoinType::Cross {
            self.rows = probe_cross(&left_rows, &right_rows);
            if !self.join.using_cols.is_empty() {
                self.rows = project_using_rows(
                    std::mem::take(&mut self.rows),
                    left_width,
                    &right_raw,
                    &self.join.using_cols,
                );
            }
            return Ok(());
        }
        let (left_key, right_key) =
            split_join_eq(&self.join.on, &self.join.left.name, &self.join.right.name)?;
        if left_rows.len() < self.mem_limit && right_rows.len() < self.mem_limit {
            self.rows = probe_join(
                self.join.tp,
                &left_rows,
                &right_rows,
                &left_schema,
                &right_schema,
                &left_key,
                &right_key,
                left_width,
                right_width,
            )?;
            if !self.join.using_cols.is_empty() {
                self.rows = project_using_rows(
                    std::mem::take(&mut self.rows),
                    left_width,
                    &right_raw,
                    &self.join.using_cols,
                );
            }
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
                left_width,
                right_width,
            )?);
        }
        self.rows = out;
        if !self.join.using_cols.is_empty() {
            self.rows = project_using_rows(
                std::mem::take(&mut self.rows),
                left_width,
                &right_raw,
                &self.join.using_cols,
            );
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
        ),
        JoinType::LeftOuter => probe_outer(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
            left_width,
            right_width,
            true,
        ),
        JoinType::RightOuter => probe_outer(
            right_rows,
            left_rows,
            right_schema,
            left_schema,
            right_key,
            left_key,
            right_width,
            left_width,
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
        ),
        JoinType::Anti => probe_anti(
            left_rows,
            right_rows,
            left_schema,
            right_schema,
            left_key,
            right_key,
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
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(right_rows, right_schema, right_key)?;
    let mut out = Vec::new();
    for left in left_rows {
        let key = eval_expr_with_row(left_key, left_schema, left)?;
        if key.is_null() {
            continue;
        }
        if index.contains_key(&key_text(&key)) {
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
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(right_rows, right_schema, right_key)?;
    let mut out = Vec::new();
    for left in left_rows {
        let key = eval_expr_with_row(left_key, left_schema, left)?;
        let matched = !key.is_null() && index.contains_key(&key_text(&key));
        if !matched {
            out.push(left.clone());
        }
    }
    Ok(out)
}

/// Concatenate every left row with every right row (CROSS JOIN / comma join).
fn probe_cross(left_rows: &[Vec<Datum>], right_rows: &[Vec<Datum>]) -> Vec<Vec<Datum>> {
    let mut out = Vec::with_capacity(left_rows.len().saturating_mul(right_rows.len()));
    for left in left_rows {
        for right in right_rows {
            out.push(concat_row(left, right));
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

fn project_using_rows(
    rows: Vec<Vec<Datum>>,
    left_width: usize,
    right: &[String],
    using: &[String],
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
        .map(|row| keep.iter().map(|&i| row.get(i).cloned().unwrap_or(Datum::Null)).collect())
        .collect()
}

fn split_join_eq(
    on: &ExprNode,
    left_table: &str,
    right_table: &str,
) -> Result<(ExprNode, ExprNode)> {
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
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(build_rows, build_schema, build_key)?;
    let mut out = Vec::new();
    for probe in probe_rows {
        let key = eval_expr_with_row(probe_key, probe_schema, probe)?;
        if key.is_null() {
            continue;
        }
        if let Some(matches) = index.get(&key_text(&key)) {
            for build in matches {
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
    left_width: usize,
    right_width: usize,
    outer_is_left: bool,
) -> Result<Vec<Vec<Datum>>> {
    let index = build_index(inner_rows, inner_schema, inner_key)?;
    let mut out = Vec::new();
    for outer in outer_rows {
        let key = eval_expr_with_row(outer_key, outer_schema, outer)?;
        let mut matched = false;
        if !key.is_null()
            && let Some(matches) = index.get(&key_text(&key))
        {
            for inner in matches {
                matched = true;
                out.push(if outer_is_left {
                    concat_row(outer, inner)
                } else {
                    concat_row(inner, outer)
                });
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
) -> Result<HashMap<String, Vec<Vec<Datum>>>> {
    let mut index: HashMap<String, Vec<Vec<Datum>>> = HashMap::new();
    for row in rows {
        let value = eval_expr_with_row(key, schema, row)?;
        if value.is_null() {
            continue;
        }
        index.entry(key_text(&value)).or_default().push(row.clone());
    }
    Ok(index)
}

fn concat_row(left: &[Datum], right: &[Datum]) -> Vec<Datum> {
    let mut row = left.to_vec();
    row.extend_from_slice(right);
    row
}

fn key_text(value: &Datum) -> String {
    format!("{value:?}")
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
    if join.tp == JoinType::Cross {
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
        if let Some(matches) = index.get(&key_text(&key)) {
            for &build_i in matches {
                out.push((probe_i, build_i));
            }
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
        if !key.is_null()
            && let Some(matches) = index.get(&key_text(&key))
        {
            for &inner_i in matches {
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
        index.entry(key_text(&value)).or_default().push(i);
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
            }),
            using_cols: Vec::new(),
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

use crate::internal::exec::Executor;
use sealdb_errno::{Error, Result};
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::ast::{Join, JoinType};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashMap;

pub struct HashJoinV1Exec {
    join: Join,
    left: Box<dyn Executor>,
    right: Box<dyn Executor>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
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
            emitted: false,
        }
    }
}

impl Executor for HashJoinV1Exec {
    fn open(&mut self) -> Result<()> {
        self.left.open()?;
        self.right.open()?;
        let left_schema = qualify_schema(self.left.schema(), &self.join.left.name);
        let right_schema = qualify_schema(self.right.schema(), &self.join.right.name);
        let left_width = left_schema.len();
        let right_width = right_schema.len();
        self.schema = left_schema
            .iter()
            .cloned()
            .chain(right_schema.iter().cloned())
            .collect();
        let (left_key, right_key) =
            split_join_eq(&self.join.on, &self.join.left.name, &self.join.right.name)?;
        let left_rows = collect_rows(self.left.as_mut())?;
        let right_rows = collect_rows(self.right.as_mut())?;
        self.rows = match self.join.tp {
            JoinType::Inner => probe_inner(
                &left_rows,
                &right_rows,
                &left_schema,
                &right_schema,
                &left_key,
                &right_key,
            )?,
            JoinType::LeftOuter => probe_outer(
                &left_rows,
                &right_rows,
                &left_schema,
                &right_schema,
                &left_key,
                &right_key,
                left_width,
                right_width,
                true,
            )?,
            JoinType::RightOuter => probe_outer(
                &right_rows,
                &left_rows,
                &right_schema,
                &left_schema,
                &right_key,
                &left_key,
                right_width,
                left_width,
                false,
            )?,
        };
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

fn side_of(expr: &ExprNode, left_table: &str, right_table: &str) -> Option<bool> {
    let ExprNode::Column(col) = expr else {
        return None;
    };
    match col.table.as_deref() {
        Some(table) if table.eq_ignore_ascii_case(left_table) => Some(true),
        Some(table) if table.eq_ignore_ascii_case(right_table) => Some(false),
        None => None,
        Some(_) => None,
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

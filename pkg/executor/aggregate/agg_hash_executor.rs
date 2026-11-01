use crate::aggfuncs::{build_agg_func, is_agg_name};
use crate::internal::exec::Executor;
use sealdb_errno::{Error, Result};
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{ExprNode, FuncCallExpr};
use sealdb_parser::format::RestoreCtx;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::BTreeMap;

enum OutputItem {
    Group(usize),
    Agg(usize),
}

pub struct HashAggExec {
    child: Box<dyn Executor>,
    group_by: Vec<ExprNode>,
    agg_specs: Vec<FuncCallExpr>,
    outputs: Vec<OutputItem>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl HashAggExec {
    pub fn new(select: &SelectStmt, child: Box<dyn Executor>) -> Result<Self> {
        let mut agg_specs = Vec::new();
        let mut outputs = Vec::new();
        let mut schema = Vec::new();
        for field in &select.fields {
            schema.push(
                field
                    .as_name
                    .clone()
                    .unwrap_or_else(|| restore_expr(&field.expr)),
            );
            if let ExprNode::Func(func) = &field.expr
                && is_agg_name(&func.name)
            {
                outputs.push(OutputItem::Agg(agg_specs.len()));
                agg_specs.push(func.clone());
                continue;
            }
            let idx = select
                .group_by
                .iter()
                .position(|g| restore_expr(g).eq_ignore_ascii_case(&restore_expr(&field.expr)))
                .ok_or_else(|| Error::unsupported("select list is not in GROUP BY clause"))?;
            outputs.push(OutputItem::Group(idx));
        }
        Ok(Self {
            child,
            group_by: select.group_by.clone(),
            agg_specs,
            outputs,
            schema,
            rows: Vec::new(),
            emitted: false,
        })
    }
}

impl Executor for HashAggExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        let input_schema = self.child.schema().to_vec();
        let mut groups: BTreeMap<String, GroupState> = BTreeMap::new();
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                let row = chunk.get_row(i);
                let keys = self
                    .group_by
                    .iter()
                    .map(|expr| eval_expr_with_row(expr, &input_schema, &row))
                    .collect::<Result<Vec<_>>>()?;
                let map_key = encode_group_key(&keys);
                if !groups.contains_key(&map_key) {
                    groups.insert(
                        map_key.clone(),
                        GroupState {
                            keys,
                            aggs: self
                                .agg_specs
                                .iter()
                                .map(build_agg_func)
                                .collect::<Result<Vec<_>>>()?,
                        },
                    );
                }
                let group = groups
                    .get_mut(&map_key)
                    .expect("group just inserted or already present");
                for (spec, agg) in self.agg_specs.iter().zip(group.aggs.iter_mut()) {
                    if spec.args.is_empty() {
                        agg.update(None)?;
                    } else {
                        let value = eval_expr_with_row(&spec.args[0], &input_schema, &row)?;
                        agg.update(Some(&value))?;
                    }
                }
            }
        }
        if groups.is_empty() && self.group_by.is_empty() {
            let mut aggs = self
                .agg_specs
                .iter()
                .map(build_agg_func)
                .collect::<Result<Vec<_>>>()?;
            self.rows.push(self.finish_row(&[], &mut aggs)?);
        } else {
            for group in groups.into_values() {
                let GroupState { keys, mut aggs } = group;
                self.rows.push(self.finish_row(&keys, &mut aggs)?);
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

struct GroupState {
    keys: Vec<Datum>,
    aggs: Vec<Box<dyn crate::aggfuncs::AggFunc>>,
}

impl HashAggExec {
    fn finish_row(
        &self,
        keys: &[Datum],
        aggs: &mut [Box<dyn crate::aggfuncs::AggFunc>],
    ) -> Result<Vec<Datum>> {
        self.outputs
            .iter()
            .map(|item| match item {
                OutputItem::Group(idx) => Ok(keys.get(*idx).cloned().unwrap_or(Datum::Null)),
                OutputItem::Agg(idx) => aggs[*idx].finalize(),
            })
            .collect()
    }
}

fn restore_expr(expr: &ExprNode) -> String {
    let mut ctx = RestoreCtx::new();
    let _ = expr.restore(&mut ctx);
    ctx.into_string()
}

fn encode_group_key(keys: &[Datum]) -> String {
    keys.iter()
        .map(|d| format!("{d:?}"))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

// F08 core path: in-memory HashAgg for COUNT/SUM/AVG/MIN/MAX + GROUP BY + HAVING.
// Completeness wave / not this feature: sorted GROUP BY StreamAgg,
// spill / parallel workers, GROUP BY expressions, ROLLUP/CUBE, AVG decimal scale.
use crate::aggfuncs::{build_agg_func, is_agg_name};
use crate::internal::exec::Executor;
use sealdb_errno::{Error, Result};
use sealdb_expression::{eval_expr_with_row, eval_filter};
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, FuncCallExpr};
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
    extra_aggs: usize,
    having: Option<ExprNode>,
    filter_schema: Vec<String>,
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
        let mut extra_aggs = 0;
        let mut filter_schema = schema.clone();
        if let Some(having) = &select.having {
            for agg in collect_aggs(having) {
                let restored = restore_expr(&ExprNode::Func(agg.clone()));
                if filter_schema
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&restored))
                    || agg_specs.iter().any(|existing| {
                        restore_expr(&ExprNode::Func(existing.clone()))
                            .eq_ignore_ascii_case(&restored)
                    })
                {
                    continue;
                }
                extra_aggs += 1;
                outputs.push(OutputItem::Agg(agg_specs.len()));
                agg_specs.push(agg);
                filter_schema.push(restored);
            }
        }
        let having = select
            .having
            .as_ref()
            .map(|expr| rewrite_having(expr, select));
        Ok(Self {
            child,
            group_by: select.group_by.clone(),
            agg_specs,
            outputs,
            extra_aggs,
            having,
            filter_schema,
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
            self.push_group(&[], &mut aggs)?;
        } else {
            for group in groups.into_values() {
                let GroupState { keys, mut aggs } = group;
                self.push_group(&keys, &mut aggs)?;
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
    fn push_group(
        &mut self,
        keys: &[Datum],
        aggs: &mut [Box<dyn crate::aggfuncs::AggFunc>],
    ) -> Result<()> {
        let full = self.finish_row(keys, aggs)?;
        if let Some(having) = &self.having
            && !eval_filter(having, &self.filter_schema, &full)?
        {
            return Ok(());
        }
        let visible = full.len().saturating_sub(self.extra_aggs);
        self.rows.push(full[..visible].to_vec());
        Ok(())
    }

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

fn collect_aggs(expr: &ExprNode) -> Vec<FuncCallExpr> {
    let mut out = Vec::new();
    walk_aggs(expr, &mut out);
    out
}

fn walk_aggs(expr: &ExprNode, out: &mut Vec<FuncCallExpr>) {
    match expr {
        ExprNode::Func(func) if is_agg_name(&func.name) => out.push(func.clone()),
        ExprNode::Func(func) => func.args.iter().for_each(|arg| walk_aggs(arg, out)),
        ExprNode::Binary(bin) => {
            walk_aggs(&bin.left, out);
            walk_aggs(&bin.right, out);
        }
        ExprNode::Unary(unary) => walk_aggs(&unary.expr, out),
        ExprNode::Cast(cast) => walk_aggs(&cast.expr, out),
        ExprNode::InList(list) => {
            walk_aggs(&list.expr, out);
            list.list.iter().for_each(|item| walk_aggs(item, out));
        }
        ExprNode::Between(between) => {
            walk_aggs(&between.expr, out);
            walk_aggs(&between.low, out);
            walk_aggs(&between.high, out);
        }
        ExprNode::Like(like) => {
            walk_aggs(&like.expr, out);
            walk_aggs(&like.pattern, out);
        }
        ExprNode::IsNull(is_null) => walk_aggs(&is_null.expr, out),
        _ => {}
    }
}

fn rewrite_having(expr: &ExprNode, select: &SelectStmt) -> ExprNode {
    match expr {
        ExprNode::Func(func) if is_agg_name(&func.name) => {
            let restored = restore_expr(expr);
            let name = select
                .fields
                .iter()
                .find(|field| restore_expr(&field.expr).eq_ignore_ascii_case(&restored))
                .and_then(|field| field.as_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or(restored);
            ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name,
            })
        }
        ExprNode::Binary(bin) => ExprNode::Binary(sealdb_parser::ast::expressions::BinaryOpExpr {
            op: bin.op,
            left: Box::new(rewrite_having(&bin.left, select)),
            right: Box::new(rewrite_having(&bin.right, select)),
        }),
        ExprNode::Unary(unary) => ExprNode::Unary(sealdb_parser::ast::expressions::UnaryOpExpr {
            op: unary.op,
            expr: Box::new(rewrite_having(&unary.expr, select)),
        }),
        ExprNode::Cast(cast) => ExprNode::Cast(sealdb_parser::ast::expressions::FuncCastExpr {
            expr: Box::new(rewrite_having(&cast.expr, select)),
            target: cast.target.clone(),
        }),
        ExprNode::InList(list) => ExprNode::InList(sealdb_parser::ast::expressions::InListExpr {
            expr: Box::new(rewrite_having(&list.expr, select)),
            not: list.not,
            list: list
                .list
                .iter()
                .map(|item| rewrite_having(item, select))
                .collect(),
        }),
        ExprNode::Between(between) => {
            ExprNode::Between(sealdb_parser::ast::expressions::BetweenExpr {
                expr: Box::new(rewrite_having(&between.expr, select)),
                not: between.not,
                low: Box::new(rewrite_having(&between.low, select)),
                high: Box::new(rewrite_having(&between.high, select)),
            })
        }
        ExprNode::Like(like) => ExprNode::Like(sealdb_parser::ast::expressions::PatternLikeExpr {
            expr: Box::new(rewrite_having(&like.expr, select)),
            not: like.not,
            pattern: Box::new(rewrite_having(&like.pattern, select)),
            ci: like.ci,
        }),
        ExprNode::IsNull(is_null) => {
            ExprNode::IsNull(sealdb_parser::ast::expressions::IsNullExpr {
                expr: Box::new(rewrite_having(&is_null.expr, select)),
                not: is_null.not,
            })
        }
        ExprNode::Func(func) => ExprNode::Func(FuncCallExpr {
            name: func.name.clone(),
            args: func
                .args
                .iter()
                .map(|arg| rewrite_having(arg, select))
                .collect(),
            distinct: func.distinct,
        }),
        other => other.clone(),
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

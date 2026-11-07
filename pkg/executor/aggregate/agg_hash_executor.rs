// core path: in-memory HashAgg for COUNT/SUM/AVG/MIN/MAX + GROUP BY + HAVING.
// Completeness wave / not this feature: sorted GROUP BY StreamAgg,
// recursive spill / parallel workers, GROUP BY expressions, ROLLUP/CUBE, AVG decimal scale.
use crate::aggfuncs::{build_agg_func, is_agg_name};
use crate::aggregate::agg_spill::{add_to_partition, new_partitions};
use crate::internal::exec::Executor;
use sealdb_errno::Result;
use crate::join::apply::SubqueryEvalCtx;
use sealdb_expression::{eval_expr_with_row, eval_filter, truthy};
use sealdb_parser::ast::SelectStmt;
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode, FuncCallExpr};
use sealdb_parser::format::RestoreCtx;
use sealdb_types::Datum;
use sealdb_util::chunk::{Chunk, DataInDiskByRows, mem_row_limit};
use std::collections::BTreeMap;

enum OutputItem {
    Group(usize),
    Agg(usize),
    /// Literal / constant select item (always legal under ONLY_FULL_GROUP_BY).
    Const(Datum),
    /// Non-grouped column allowed when sql_mode lacks ONLY_FULL_GROUP_BY
    /// (MySQL "any value" — first row in the group).
    Any(usize),
}

pub struct HashAggExec {
    child: Box<dyn Executor>,
    group_by: Vec<ExprNode>,
    any_exprs: Vec<ExprNode>,
    agg_specs: Vec<FuncCallExpr>,
    outputs: Vec<OutputItem>,
    extra_aggs: usize,
    having: Option<ExprNode>,
    /// Correlated SELECT-list / HAVING subqueries (`Apply` is flattened here).
    subquery: Option<SubqueryEvalCtx>,
    filter_schema: Vec<String>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    mem_limit: usize,
    spilled: bool,
    emitted: bool,
}

impl HashAggExec {
    pub fn new(select: &SelectStmt, child: Box<dyn Executor>) -> Result<Self> {
        let mut agg_specs = Vec::new();
        let mut any_exprs = Vec::new();
        let mut outputs = Vec::new();
        let mut schema = Vec::new();
        for field in &select.fields {
            // TiDB uses field.Text() / AsName; prefer orig_text over restore (no backticks).
            schema.push(
                field
                    .as_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| field.orig_text.clone())
                    .unwrap_or_else(|| restore_expr(&field.expr)),
            );
            if let ExprNode::Func(func) = &field.expr
                && is_agg_name(&func.name)
            {
                outputs.push(OutputItem::Agg(agg_specs.len()));
                agg_specs.push(func.clone());
                continue;
            }
            if let ExprNode::Value(v) = &field.expr {
                outputs.push(OutputItem::Const(v.datum.clone()));
                continue;
            }
            if let Some(idx) = select
                .group_by
                .iter()
                .position(|g| restore_expr(g).eq_ignore_ascii_case(&restore_expr(&field.expr)))
            {
                outputs.push(OutputItem::Group(idx));
                continue;
            }
            // Loose sql_mode / session already rejected ONLY_FULL_GROUP_BY violations.
            outputs.push(OutputItem::Any(any_exprs.len()));
            any_exprs.push(field.expr.clone());
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
        // MySQL/TiDB: `GROUP BY alias` resolves to the select-list expression.
        let group_by = select
            .group_by
            .iter()
            .map(|g| rewrite_group_by_alias(g, select))
            .collect();
        Ok(Self {
            child,
            group_by,
            any_exprs,
            agg_specs,
            outputs,
            extra_aggs,
            having,
            subquery: None,
            filter_schema,
            schema,
            rows: Vec::new(),
            mem_limit: mem_row_limit(),
            spilled: false,
            emitted: false,
        })
    }

    pub(crate) fn with_subquery(mut self, subquery: SubqueryEvalCtx) -> Self {
        self.subquery = Some(subquery);
        self
    }

    /// Evaluate `expr` against one input row, materializing any subquery first.
    fn eval(&mut self, expr: &ExprNode, schema: &[String], row: &[Datum]) -> Result<Datum> {
        match self.subquery.as_mut() {
            Some(ctx) => ctx.eval(expr, schema, row),
            None => eval_expr_with_row(expr, schema, row),
        }
    }

    fn eval_bool(&mut self, expr: &ExprNode, schema: &[String], row: &[Datum]) -> Result<bool> {
        if self.subquery.is_none() {
            return eval_filter(expr, schema, row);
        }
        let value = self.eval(expr, schema, row)?;
        Ok(truthy(&value).unwrap_or(false))
    }

    pub fn with_mem_limit(mut self, mem_limit: usize) -> Self {
        self.mem_limit = mem_limit;
        self
    }

    pub fn spilled(&self) -> bool {
        self.spilled
    }
}

impl Executor for HashAggExec {
    fn open(&mut self) -> Result<()> {
        self.child.open()?;
        self.spilled = false;
        let input_schema = self.child.schema().to_vec();
        let mut buf: Vec<Vec<Datum>> = Vec::new();
        let mut parts: Option<Vec<DataInDiskByRows>> = None;
        while let Some(chunk) = self.child.next()? {
            for i in 0..chunk.num_rows() {
                let row = chunk.get_row(i);
                if parts.is_none() && buf.len() >= self.mem_limit {
                    let mut files = new_partitions()?;
                    for existing in buf.drain(..) {
                        self.write_partition(&mut files, &input_schema, &existing)?;
                    }
                    parts = Some(files);
                    self.spilled = true;
                }
                if let Some(files) = parts.as_mut() {
                    self.write_partition(files, &input_schema, &row)?;
                } else {
                    buf.push(row);
                }
            }
        }
        if let Some(mut files) = parts {
            for file in &mut files {
                file.finish_write()?;
                let rows = file.read_all()?;
                if !rows.is_empty() {
                    self.aggregate_rows(&input_schema, rows, false)?;
                }
            }
        } else {
            self.aggregate_rows(&input_schema, buf, true)?;
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
    any_vals: Vec<Datum>,
    aggs: Vec<Box<dyn crate::aggfuncs::AggFunc>>,
}

impl HashAggExec {
    fn write_partition(
        &mut self,
        files: &mut [DataInDiskByRows],
        input_schema: &[String],
        row: &[Datum],
    ) -> Result<()> {
        let group_by = self.group_by.clone();
        let keys = group_by
            .iter()
            .map(|expr| self.eval(expr, input_schema, row))
            .collect::<Result<Vec<_>>>()?;
        add_to_partition(files, &keys, row)
    }

    fn aggregate_rows(
        &mut self,
        input_schema: &[String],
        rows: Vec<Vec<Datum>>,
        emit_empty_scalar: bool,
    ) -> Result<()> {
        let mut groups: BTreeMap<String, GroupState> = BTreeMap::new();
        let group_by = self.group_by.clone();
        let any_exprs = self.any_exprs.clone();
        for row in rows {
            let keys = group_by
                .iter()
                .map(|expr| self.eval(expr, input_schema, &row))
                .collect::<Result<Vec<_>>>()?;
            let map_key = encode_group_key(&keys);
            if !groups.contains_key(&map_key) {
                let any_vals = any_exprs
                    .iter()
                    .map(|expr| self.eval(expr, input_schema, &row))
                    .collect::<Result<Vec<_>>>()?;
                groups.insert(
                    map_key.clone(),
                    GroupState {
                        keys,
                        any_vals,
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
            let args: Vec<Option<ExprNode>> = self
                .agg_specs
                .iter()
                .map(|spec| spec.args.first().cloned())
                .collect();
            for (arg, agg) in args.into_iter().zip(group.aggs.iter_mut()) {
                match arg {
                    None => agg.update(None)?,
                    Some(expr) => {
                        let value = self.eval(&expr, input_schema, &row)?;
                        agg.update(Some(&value))?;
                    }
                }
            }
        }
        if groups.is_empty() {
            if emit_empty_scalar && self.group_by.is_empty() {
                let mut aggs = self
                    .agg_specs
                    .iter()
                    .map(build_agg_func)
                    .collect::<Result<Vec<_>>>()?;
                self.push_group(&[], &[], &mut aggs)?;
            }
            return Ok(());
        }
        for group in groups.into_values() {
            let GroupState {
                keys,
                any_vals,
                mut aggs,
            } = group;
            self.push_group(&keys, &any_vals, &mut aggs)?;
        }
        Ok(())
    }

    fn push_group(
        &mut self,
        keys: &[Datum],
        any_vals: &[Datum],
        aggs: &mut [Box<dyn crate::aggfuncs::AggFunc>],
    ) -> Result<()> {
        let full = self.finish_row(keys, any_vals, aggs)?;
        if let Some(having) = self.having.clone() {
            let schema = self.filter_schema.clone();
            if !self.eval_bool(&having, &schema, &full)? {
                return Ok(());
            }
        }
        let visible = full.len().saturating_sub(self.extra_aggs);
        self.rows.push(full[..visible].to_vec());
        Ok(())
    }

    fn finish_row(
        &self,
        keys: &[Datum],
        any_vals: &[Datum],
        aggs: &mut [Box<dyn crate::aggfuncs::AggFunc>],
    ) -> Result<Vec<Datum>> {
        self.outputs
            .iter()
            .map(|item| match item {
                OutputItem::Group(idx) => Ok(keys.get(*idx).cloned().unwrap_or(Datum::Null)),
                OutputItem::Agg(idx) => aggs[*idx].finalize(),
                OutputItem::Const(d) => Ok(d.clone()),
                OutputItem::Any(idx) => Ok(any_vals.get(*idx).cloned().unwrap_or(Datum::Null)),
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
        ExprNode::QuantifiedCmp(cmp) => walk_aggs(&cmp.expr, out),
        _ => {}
    }
}

fn rewrite_group_by_alias(expr: &ExprNode, select: &SelectStmt) -> ExprNode {
    match expr {
        ExprNode::Column(col) if col.table.is_none() => {
            if let Some(field) = select.fields.iter().find(|f| {
                f.as_name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&col.name))
            }) {
                return field.expr.clone();
            }
            expr.clone()
        }
        other => other.clone(),
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
            single_cnf_item: false,
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
        ExprNode::QuantifiedCmp(cmp) => {
            ExprNode::QuantifiedCmp(sealdb_parser::ast::expressions::QuantifiedCmpExpr {
                expr: Box::new(rewrite_having(&cmp.expr, select)),
                op: cmp.op,
                all: cmp.all,
                query: cmp.query.clone(),
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::exec::{Executor, VecExec};
    use sealdb_parser::Parser;
    use sealdb_types::Datum;

    fn grouped_select() -> SelectStmt {
        let stmt = Parser::new()
            .parse_one_stmt("SELECT name, COUNT(*) FROM t GROUP BY name")
            .unwrap();
        stmt.as_select().cloned().unwrap()
    }

    #[test]
    fn test_hash_agg_spill_matches_in_memory() {
        let rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(3), Datum::Bytes(b"b".to_vec())],
            vec![Datum::I64(4), Datum::Bytes(b"b".to_vec())],
        ];
        let schema = vec!["id".to_string(), "name".to_string()];
        let select = grouped_select();
        let mut mem = HashAggExec::new(
            &select,
            Box::new(VecExec::new(schema.clone(), rows.clone())),
        )
        .unwrap()
        .with_mem_limit(usize::MAX);
        mem.open().unwrap();
        let mem_chunk = mem.next().unwrap().unwrap();
        let mut spilled = HashAggExec::new(&select, Box::new(VecExec::new(schema, rows)))
            .unwrap()
            .with_mem_limit(1);
        spilled.open().unwrap();
        let spilled_chunk = spilled.next().unwrap().unwrap();
        assert!(spilled.spilled());
        let mut mem_rows = (0..mem_chunk.num_rows())
            .map(|i| mem_chunk.get_row(i))
            .collect::<Vec<_>>();
        let mut spilled_rows = (0..spilled_chunk.num_rows())
            .map(|i| spilled_chunk.get_row(i))
            .collect::<Vec<_>>();
        mem_rows.sort_by(|a, b| a[0].compare(&b[0]));
        spilled_rows.sort_by(|a, b| a[0].compare(&b[0]));
        assert_eq!(mem_rows, spilled_rows);
        assert_eq!(mem_rows.len(), 2);
    }
}

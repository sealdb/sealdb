//! Access-path helpers for expression indexes.
//!
//! TiDB stores an expression key part as a hidden generated column
//! (`_V$_uk_expr_0`) whose `generated_expr` is the original text, and the
//! ranger matches a predicate against the *expression*, not the column name.
//! SealDB therefore keys index parts by [`ExprNode::expr_key`]:
//!
//! * a plain part reports its column name (unchanged behaviour), and
//! * an expression part reports its canonical text (`tidb_shard(a)`), which is
//!   what `tidb_shard(a) = 8` reports once the shard rewrite ran.
//!
//! A part that the predicate never names directly (the shard part of
//! `uk_expr((tidb_shard(a)), a)` under `a = 100`) is folded by evaluating the
//! stored expression over the values the other parts pin, so the planner sees
//! the same key the DDL stored.

use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
use sealdb_parser::ast::dml::SelectStmt;
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, ColumnNameExpr, ExprNode, FuncCallExpr, FuncCastExpr, InListExpr,
    IsNullExpr, PatternLikeExpr, UnaryOpExpr,
};
use sealdb_parser::opcode::Op;
use sealdb_parser::sqlparser_bridge::lower::parse_expr;
use sealdb_types::Datum;

/// The hidden generated column behind an expression key part.
pub fn hidden_column<'a>(info: &'a TableInfo, part: &IndexColumn) -> Option<&'a ColumnInfo> {
    let name = part.name.o();
    info.columns
        .iter()
        .find(|col| col.hidden && col.name.o().eq_ignore_ascii_case(name))
}

/// Parsed `generated_expr` of an expression key part.
pub fn part_expr(info: &TableInfo, part: &IndexColumn) -> Option<ExprNode> {
    let text = hidden_column(info, part)?.generated_expr.as_deref()?;
    parse_expr(text).ok()
}

/// Canonical key of one index part: the column name, or the expression text.
pub fn part_key(info: &TableInfo, part: &IndexColumn) -> String {
    part_expr(info, part)
        .map(|expr| expr.expr_key())
        .unwrap_or_else(|| part.name.o().to_string())
}

/// Canonical key of every index part, in key order.
pub fn part_keys(info: &TableInfo, index: &IndexInfo) -> Vec<String> {
    index
        .columns
        .iter()
        .map(|part| part_key(info, part))
        .collect()
}

/// Does the index carry at least one expression key part?
pub fn has_expression_part(info: &TableInfo, index: &IndexInfo) -> bool {
    index
        .columns
        .iter()
        .any(|part| hidden_column(info, part).is_some())
}

/// Expression key parts as `(expression key, hidden column name)`.
pub fn expression_parts(info: &TableInfo, index: &IndexInfo) -> Vec<(String, String)> {
    index
        .columns
        .iter()
        .filter_map(|part| {
            let expr = part_expr(info, part)?;
            Some((expr.expr_key(), part.name.o().to_string()))
        })
        .collect()
}

/// Rewrite an expression so every expression key part stands in for the
/// expression it stores: the part is what a covering read can return.
pub fn substitute_parts_expr(info: &TableInfo, index: &IndexInfo, expr: &ExprNode) -> ExprNode {
    let parts = expression_parts(info, index);
    if parts.is_empty() {
        return expr.clone();
    }
    let mut changed = false;
    substitute_expr(expr, &parts, &mut changed)
}

/// Hidden key part that stores the value of a `VIRTUAL` generated column with
/// the same body (`expr_idx((a+1))` answers `SELECT c` when `c` is `(a+1)`).
pub fn part_for_virtual_column(
    info: &TableInfo,
    index: &IndexInfo,
    column: &str,
) -> Option<String> {
    let col = info
        .columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(column))?;
    if col.generated_stored {
        return None;
    }
    let body = parse_expr(col.generated_expr.as_deref()?).ok()?;
    let key = body.expr_key();
    expression_parts(info, index)
        .into_iter()
        .find(|(part, _)| *part == key)
        .map(|(_, name)| name)
}

/// An index part that is a generated column defined as exactly another column
/// (`c1 double AS (c0)`) answers that base column when both share the storage
/// type — TiDB's generated-column substitution for covering reads.
pub fn part_for_identity_generated(
    info: &TableInfo,
    index: &IndexInfo,
    column: &str,
) -> Option<String> {
    let base = info
        .columns
        .iter()
        .find(|col| col.name.o().eq_ignore_ascii_case(column))?;
    index.columns.iter().find_map(|part| {
        let generated = info
            .columns
            .iter()
            .find(|col| col.name.o().eq_ignore_ascii_case(part.name.o()))?;
        let body = parse_expr(generated.generated_expr.as_deref()?).ok()?;
        let ExprNode::Column(referenced) = body else {
            return None;
        };
        if !referenced.name.eq_ignore_ascii_case(column) {
            return None;
        }
        // The index holds the generated column's type; only an identical type
        // round-trips the base value (`FLOAT` vs `DOUBLE` does not).
        if generated.field_type.tp != base.field_type.tp {
            return None;
        }
        if generated.field_type.charset != base.field_type.charset
            || generated.field_type.collate != base.field_type.collate
        {
            return None;
        }
        Some(part.name.o().to_string())
    })
}

/// Map a covering read's projection onto the hidden key columns: the index read
/// cannot provide the base columns an expression spells out (`a + 1`), only the
/// value it stores, and a virtual generated column with the same body likewise.
pub fn substitute_index_parts_for_read(
    info: &TableInfo,
    index: &IndexInfo,
    expr: &ExprNode,
) -> ExprNode {
    let mut map = expression_parts(info, index);
    if map.is_empty() {
        return expr.clone();
    }
    for col in info
        .columns
        .iter()
        .filter(|col| !col.hidden && !col.generated_stored)
    {
        let Some(key) = col
            .generated_expr
            .as_deref()
            .and_then(|text| parse_expr(text).ok())
            .map(|body| body.expr_key())
        else {
            continue;
        };
        if let Some((_, part)) = map.iter().find(|(candidate, _)| *candidate == key) {
            let part = part.clone();
            map.push((col.name.o().to_string(), part));
        }
    }
    let mut changed = false;
    substitute_expr_impl(expr, &map, true, &mut changed)
}

/// Evaluate an index part expression over the constants a predicate pins.
///
/// Returns `None` when the expression reads a column the predicate does not
/// pin (`a + b` under `a = 1` alone) or when the value folds to `NULL`.
pub fn eval_part(info: &TableInfo, expr: &ExprNode, known: &[(String, Datum)]) -> Option<Datum> {
    let schema: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let mut row = vec![Datum::Null; info.columns.len()];
    for (name, value) in known {
        if let Some(pos) = info
            .columns
            .iter()
            .position(|c| c.name.o().eq_ignore_ascii_case(name))
        {
            row[pos] = value.clone();
        }
    }
    let value = sealdb_expression::eval_expr_with_row(expr, &schema, &row).ok()?;
    if value.is_null() { None } else { Some(value) }
}

/// Values of every key part pinned by one conjunction list.
///
/// `a = 100` pins `uk_expr((tidb_shard(a)), a)` completely: the plain part comes
/// from the equality, the shard part folds from it.
pub fn pinned_tuple(
    info: &TableInfo,
    index: &IndexInfo,
    conjuncts: &[ExprNode],
) -> Option<Vec<Datum>> {
    let eqs: Vec<(String, Datum)> = conjuncts
        .iter()
        .filter_map(|conjunct| conjunct.eq_key_const())
        .collect();
    // Start from every equality the arm carries: an expression part may read a
    // column that is not itself a key part (`a = 100` for `(tidb_shard(a), a)`
    // is a key part, but `(a + b)` under `b = 2` is not).
    let mut known = eqs.clone();
    let mut tuple = Vec::with_capacity(index.columns.len());
    for part in &index.columns {
        let key = part_key(info, part);
        let value = match eqs
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(&key))
        {
            Some((_, value)) => value.clone(),
            None => eval_part(info, &part_expr(info, part)?, &known)?,
        };
        known.push((key, value.clone()));
        tuple.push(value);
    }
    Some(tuple)
}

/// Every shard index of `info` (`UNIQUE KEY ((tidb_shard(col)), …)`).
fn shard_index_shapes(info: &TableInfo) -> Vec<ShardIndex> {
    let mut shard_indices: Vec<ShardIndex> = Vec::new();
    for index in &info.indices {
        // TiDB only marks unique secondary indexes with more than one column.
        if !index.unique || index.columns.len() < 2 {
            continue;
        }
        let Some(first) = index.columns.first() else {
            continue;
        };
        let Some(expr) = part_expr(info, first) else {
            continue;
        };
        let ExprNode::Func(func) = &expr else {
            continue;
        };
        if !func.name.eq_ignore_ascii_case("tidb_shard") {
            continue;
        }
        let [ExprNode::Column(column)] = func.args.as_slice() else {
            continue;
        };
        // The stored `generated_expr` text carries the DDL-time qualifier
        // (`` `db`.`a` ``); EXPLAIN prints the predicate's column in the
        // selection scope (`db.t.a`), so rebuild it table-qualified.
        let source = column.name.trim_matches('`').to_ascii_lowercase();
        let qualified = ExprNode::Func(sealdb_parser::ast::expressions::FuncCallExpr {
            name: func.name.clone(),
            args: vec![ExprNode::Column(
                sealdb_parser::ast::expressions::ColumnNameExpr {
                    schema: None,
                    table: Some(info.name.o().to_string()),
                    name: column.name.trim_matches('`').to_string(),
                },
            )],
            distinct: func.distinct,
        });
        let keys: Vec<String> = index
            .columns
            .iter()
            .skip(1)
            .map(|part| part_key(info, part))
            .collect();
        shard_indices.push(ShardIndex {
            expr: qualified,
            source,
            keys,
        });
    }
    shard_indices
}

/// Does the table declare a shard index?
///
/// TiDB's shard rewrite may add a `tidb_shard(col)` prefix at any point, so the
/// DataSource of such a table carries the hidden generated column even when the
/// chosen access path is a table scan (`explain_shard_index` `a = b`).
pub fn has_shard_index(info: &TableInfo) -> bool {
    !shard_index_shapes(info).is_empty()
}

/// Add `tidb_shard(col) = <bucket>` next to every equality on the shard source
/// column: TiDB's shard rewrite (`a = 100` → `tidb_shard(a) = 8 AND a = 100`).
///
/// TiDB `utilfuncp.AddPrefix4ShardIndexes` runs over a condition list:
///
/// * a single `OR` condition goes through the DNF path, where each arm is
///   rewritten on its own (`a = 1 OR a = 10` → `(shard(a)=214 AND a=1) OR
///   (shard(a)=142 AND a=10)`), and
/// * anything else uses the CNF path, which needs *every* non-shard key part
///   pinned by an equality in the list before the prefix is added.
///
/// The bucket comes from evaluating the stored `tidb_shard` expression over the
/// pinned value, and an arm that already carries the shard predicate is left
/// alone.
pub fn shard_index_rewrite(select: &SelectStmt, info: &TableInfo) -> Option<SelectStmt> {
    select.selection.as_ref()?;
    let shard_indices = shard_index_shapes(info);
    if shard_indices.is_empty() {
        return None;
    }

    let conds = select.selection.as_ref()?.split_cnf_all();
    let mut changed = false;
    let rewritten = if conds.len() == 1 && is_or_cond(&conds[0]) {
        let arms = conds[0].split_dnf();
        let arms: Vec<ExprNode> = arms
            .into_iter()
            .map(|arm| {
                let arm_conds = arm.split_cnf_all();
                match shard_prefix(&shard_indices, &arm_conds) {
                    Some(prefix) => {
                        changed = true;
                        with_prefix(prefix, arm_conds).unwrap_or(arm)
                    }
                    None => arm,
                }
            })
            .collect();
        // TiDB composes DNF items right-nested: `or(a, or(b, c))`.
        let mut items: Vec<ExprNode> = arms;
        let mut acc = items.pop()?;
        while let Some(item) = items.pop() {
            acc = or(item, acc);
        }
        acc
    } else {
        let prefix = shard_prefix(&shard_indices, &conds)?;
        changed = true;
        with_prefix(prefix, conds)?
    };
    if !changed {
        return None;
    }
    let mut out = select.clone();
    out.selection = Some(rewritten);
    Some(out)
}

/// One shard index shape: the `tidb_shard(<col>)` key part, the source column
/// name, and the canonical keys of the remaining key parts.
struct ShardIndex {
    expr: ExprNode,
    source: String,
    keys: Vec<String>,
}

fn is_or_cond(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::Binary(bin) if bin.op == Op::LogicOr)
}

/// TiDB `AddExpr4EqAndInCondition`: every non-shard key part pinned by an
/// equality, the shard part untouched, all values constant.
fn shard_prefix(indices: &[ShardIndex], conjuncts: &[ExprNode]) -> Option<ExprNode> {
    for index in indices {
        // An explicit `tidb_shard(a) = 8` already constrains the prefix.
        if conjuncts
            .iter()
            .any(|cond| cond.expr_key().eq_ignore_ascii_case(&index.expr.expr_key()))
        {
            continue;
        }
        let pins: Vec<(String, Datum)> = conjuncts
            .iter()
            .filter_map(|cond| cond.eq_key_const())
            .collect();
        let mut complete = true;
        for key in &index.keys {
            if !pins.iter().any(|(pin, _)| pin.eq_ignore_ascii_case(key)) {
                complete = false;
                break;
            }
        }
        if !complete {
            continue;
        }
        let Some((_, value)) = pins
            .iter()
            .find(|(pin, _)| pin.eq_ignore_ascii_case(&index.source))
        else {
            continue;
        };
        let bucket = sealdb_expression::eval_expr_with_row(
            &index.expr,
            std::slice::from_ref(&index.source),
            std::slice::from_ref(value),
        )
        .ok()
        .filter(|value| !value.is_null())?;
        return Some(shard_equality(&index.expr, &bucket));
    }
    None
}

/// `tidb_shard(...) = bucket` prints first, like TiDB.
fn with_prefix(prefix: ExprNode, conjuncts: Vec<ExprNode>) -> Option<ExprNode> {
    let mut rewritten = Vec::with_capacity(conjuncts.len() + 1);
    rewritten.push(prefix);
    rewritten.extend(conjuncts);
    ExprNode::compose_cnf(rewritten)
}

/// `tidb_shard(<col>) = <bucket>` with the bucket already folded.
fn shard_equality(expr: &ExprNode, bucket: &Datum) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::Eq,
        left: Box::new(expr.clone()),
        right: Box::new(ExprNode::Value(
            sealdb_parser::ast::expressions::ValueExpr {
                datum: bucket.clone(),
            },
        )),
        single_cnf_item: false,
    })
}

fn or(left: ExprNode, right: ExprNode) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op: Op::LogicOr,
        left: Box::new(left),
        right: Box::new(right),
        single_cnf_item: false,
    })
}

/// Visible columns whose `GENERATED ALWAYS AS (expr)` body can stand in for the
/// expression text, keyed by [`ExprNode::expr_key`].
///
/// Only a generated column that a secondary index keys on is substituted: TiDB
/// `gcSubstituter` rewrites the expression so the access path can use that
/// index, and leaves the text alone otherwise (`a+1` must keep matching an
/// expression index `expr_idx((a+1))` when no index stores the column).
fn generated_column_keys(info: &TableInfo) -> Vec<(String, String)> {
    let indexed = |name: &str| {
        info.indices.iter().any(|index| {
            index
                .columns
                .iter()
                .any(|part| part.name.o().eq_ignore_ascii_case(name))
        })
    };
    info.columns
        .iter()
        .filter(|col| !col.hidden && indexed(col.name.o()))
        .filter_map(|col| {
            let body = parse_expr(col.generated_expr.as_deref()?).ok()?;
            Some((body.expr_key(), col.name.o().to_string()))
        })
        .collect()
}

/// TiDB `gcSubstituter`: replace a subexpression that spells out a generated
/// column's body with the column itself, so `WHERE a+1 = 3` seeks `idx_c(c)`.
///
/// A select field that is exactly the body (`SELECT a+1`) is rewritten too and
/// keeps its written text as the output name, which lets a covering index read
/// the stored column instead of re-evaluating the expression. Predicates and
/// `ORDER BY` are rewritten recursively; aggregation stays as written.
pub fn substitute_generated_columns(info: &TableInfo, select: &SelectStmt) -> Option<SelectStmt> {
    let columns = generated_column_keys(info);
    if columns.is_empty() {
        return None;
    }
    // `IGNORE INDEX(idx)` on a generated column turns `gcSubstituter` off for
    // that expression: TiDB keeps the written body in the filter instead of
    // reading the indexed column back. `IGNORE INDEX()` disables every index.
    if let Some(from) = select.from.as_ref()
        && from.ignore_index_hint
        && (from.ignore_indexes.is_empty()
            || info.indices.iter().any(|idx| {
                from.ignore_indexes
                    .iter()
                    .any(|name| idx.name.o().eq_ignore_ascii_case(name))
                    && idx.columns.iter().any(|col| {
                        columns
                            .iter()
                            .any(|(_, generated)| generated.eq_ignore_ascii_case(col.name.o()))
                    })
            }))
    {
        return None;
    }
    let mut changed = false;
    let mut rewritten = select.clone();
    for (field, field_out) in select.fields.iter().zip(rewritten.fields.iter_mut()) {
        if matches!(field.expr, ExprNode::Column(_)) {
            continue;
        }
        let key = field.expr.expr_key();
        if let Some((_, name)) = columns.iter().find(|(candidate, _)| *candidate == key) {
            changed = true;
            if field_out.as_name.as_deref().is_none_or(str::is_empty) {
                field_out.as_name = Some(field.expr.name());
            }
            field_out.expr = ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: name.clone(),
            });
        }
    }
    if let Some(selection) = &select.selection {
        rewritten.selection = Some(substitute_expr(selection, &columns, &mut changed));
    }
    for item in &mut rewritten.order_by {
        item.expr = substitute_expr(&item.expr, &columns, &mut changed);
    }
    changed.then_some(rewritten)
}

/// [`substitute_generated_columns`] for a bare predicate. The executor's ranger
/// rebuilds key ranges from the statement's own `WHERE`, so it has to apply the
/// same rewrite the planner used or it scans a different range than `EXPLAIN`.
pub fn substitute_generated_expr(info: &TableInfo, expr: &ExprNode) -> Option<ExprNode> {
    let columns = generated_column_keys(info);
    if columns.is_empty() {
        return None;
    }
    let mut changed = false;
    let rewritten = substitute_expr(expr, &columns, &mut changed);
    changed.then_some(rewritten)
}

/// Recursive rewrite behind [`substitute_generated_columns`]. Subqueries keep
/// their own scope, so they are left as written.
fn substitute_expr(expr: &ExprNode, columns: &[(String, String)], changed: &mut bool) -> ExprNode {
    substitute_expr_impl(expr, columns, false, changed)
}

/// `columns_too` also matches bare column references by name, which a covering
/// expression-index read needs (`SELECT c` → the hidden key column).
fn substitute_expr_impl(
    expr: &ExprNode,
    columns: &[(String, String)],
    columns_too: bool,
    changed: &mut bool,
) -> ExprNode {
    if let ExprNode::Column(col) = expr {
        if columns_too
            && let Some((_, name)) = columns
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(&col.name))
        {
            *changed = true;
            return ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: name.clone(),
            });
        }
    } else {
        let key = expr.expr_key();
        if let Some((_, name)) = columns.iter().find(|(candidate, _)| *candidate == key) {
            *changed = true;
            return ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: name.clone(),
            });
        }
    }
    let substitute = |child: &ExprNode, changed: &mut bool| {
        substitute_expr_impl(child, columns, columns_too, changed)
    };
    match expr {
        ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
            op: bin.op,
            left: Box::new(substitute(&bin.left, changed)),
            right: Box::new(substitute(&bin.right, changed)),
            single_cnf_item: bin.single_cnf_item,
        }),
        ExprNode::Unary(unary) => ExprNode::Unary(UnaryOpExpr {
            op: unary.op,
            expr: Box::new(substitute(&unary.expr, changed)),
        }),
        ExprNode::Func(func) => ExprNode::Func(FuncCallExpr {
            name: func.name.clone(),
            args: func
                .args
                .iter()
                .map(|arg| substitute(arg, changed))
                .collect(),
            distinct: func.distinct,
        }),
        ExprNode::Cast(cast) => ExprNode::Cast(FuncCastExpr {
            expr: Box::new(substitute(&cast.expr, changed)),
            target: cast.target.clone(),
            json: cast.json,
        }),
        ExprNode::InList(list) => ExprNode::InList(InListExpr {
            expr: Box::new(substitute(&list.expr, changed)),
            not: list.not,
            list: list
                .list
                .iter()
                .map(|item| substitute(item, changed))
                .collect(),
        }),
        ExprNode::Between(between) => ExprNode::Between(BetweenExpr {
            expr: Box::new(substitute(&between.expr, changed)),
            not: between.not,
            low: Box::new(substitute(&between.low, changed)),
            high: Box::new(substitute(&between.high, changed)),
        }),
        ExprNode::Like(like) => ExprNode::Like(PatternLikeExpr {
            expr: Box::new(substitute(&like.expr, changed)),
            not: like.not,
            pattern: Box::new(substitute(&like.pattern, changed)),
            ci: like.ci,
        }),
        ExprNode::IsNull(is_null) => ExprNode::IsNull(IsNullExpr {
            expr: Box::new(substitute(&is_null.expr, changed)),
            not: is_null.not,
        }),
        other => other.clone(),
    }
}

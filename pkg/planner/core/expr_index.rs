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
use sealdb_parser::ast::expressions::{BinaryOpExpr, ExprNode};
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

/// Evaluate an index part expression over the constants a predicate pins.
///
/// Returns `None` when the expression reads a column the predicate does not
/// pin (`a + b` under `a = 1` alone) or when the value folds to `NULL`.
pub fn eval_part(
    info: &TableInfo,
    expr: &ExprNode,
    known: &[(String, Datum)],
) -> Option<Datum> {
    let schema: Vec<String> = info.columns.iter().map(|c| c.name.o().to_string()).collect();
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
        match shard_prefix(&shard_indices, &conds) {
            Some(prefix) => {
                changed = true;
                with_prefix(prefix, conds)?
            }
            None => return None,
        }
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

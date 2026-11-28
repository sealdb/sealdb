//! Constant folding (TiDB `expression.FoldConstant` / `foldConstant`).
//!
//! TiDB evaluates deterministic sub-expressions at plan time, so the plan and
//! its `EXPLAIN` output never carry a literal-only function call:
//!
//! ```text
//! SELECT ifnull(NULL, c1) FROM t  ->  Projection c1
//! SELECT if(10, c1, c2) FROM t    ->  Projection c1
//! SELECT * FROM t WHERE 1 = 1     ->  TableReader (no Selection)
//! ```
//!
//! The rewriting runs bottom-up and only replaces a node when evaluation
//! succeeds, so runtime-only errors keep their original timing.

use sealdb_expression::{eval_expr, truthy};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::ValueExpr;
use sealdb_parser::ast::{ExprNode, SelectStmt};
use sealdb_types::Datum;

/// Functions whose result depends on session state, time or randomness;
/// TiDB keeps them unfolded in the plan (`now()` must stay `now()`).
fn is_volatile_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "rand"
            | "random"
            | "now"
            | "current_timestamp"
            | "utc_timestamp"
            | "sysdate"
            | "curdate"
            | "curtime"
            | "current_date"
            | "current_time"
            | "localtime"
            | "localtimestamp"
            | "unix_timestamp"
            | "uuid"
            | "uuid_short"
            | "sleep"
            | "last_insert_id"
            | "connection_id"
            | "database"
            | "schema"
            | "user"
            | "current_user"
            | "session_user"
            | "system_user"
            | "version"
            | "found_rows"
            | "row_count"
            | "get_lock"
            | "release_lock"
            | "default"
            | "values"
            // `@v := expr` / eval-time `@v` read: the read must see the row's
            // own assignment, so folding would double the side effect.
            | "__sealdb_assign"
            | "__sealdb_getvar"
            // `expr COLLATE name`: the wrapper is collation metadata that
            // pruning, statistics and the coprocessor filter read; folding it
            // into a plain literal would silently fall back to the column's
            // collation.
            | "__collate"
            | "__convert_using"
    )
}

/// Aggregate / window names never fold even when their arguments are literal
/// (`count(1)` is computed over rows, not over the literal).
fn is_aggregate_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "group_concat"
            | "bit_and"
            | "bit_or"
            | "bit_xor"
            | "std"
            | "stddev"
            | "stddev_pop"
            | "stddev_samp"
            | "var_pop"
            | "var_samp"
            | "variance"
            | "json_arrayagg"
            | "json_objectagg"
            | "firstrow"
    )
}

/// `true` when every leaf of `expr` is a literal and no function is volatile /
/// aggregate. Subqueries are never constant.
fn is_constant(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(_) => true,
        ExprNode::Column(_) => false,
        ExprNode::Binary(b) => is_constant(&b.left) && is_constant(&b.right),
        ExprNode::Unary(u) => is_constant(&u.expr),
        ExprNode::Func(f) => {
            // An `INTERVAL 'XXX' unit` amount has no numeric prefix: keep it
            // unfolded so an INSERT under strict mode raises 1292 at execution
            // instead of folding the warning away at plan time.
            if f.name.eq_ignore_ascii_case("__sealdb_interval")
                && let Some(ExprNode::Value(v)) = f.args.first()
                && let Datum::Bytes(bytes) = &v.datum
                && !bytes.iter().any(u8::is_ascii_digit)
            {
                return false;
            }
            !f.distinct
                && !is_volatile_func(&f.name)
                && !is_aggregate_func(&f.name)
                && f.args.iter().all(is_constant)
        }
        ExprNode::Cast(c) => is_constant(&c.expr),
        ExprNode::InList(i) => is_constant(&i.expr) && i.list.iter().all(is_constant),
        ExprNode::Between(b) => is_constant(&b.expr) && is_constant(&b.low) && is_constant(&b.high),
        ExprNode::Like(l) => is_constant(&l.expr) && is_constant(&l.pattern),
        ExprNode::IsNull(i) => is_constant(&i.expr),
        ExprNode::Case(c) => {
            c.operand.as_ref().map(|e| is_constant(e)).unwrap_or(true)
                && c.when_then
                    .iter()
                    .all(|(w, t)| is_constant(w) && is_constant(t))
                && c.else_expr.as_ref().map(|e| is_constant(e)).unwrap_or(true)
        }
        ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_)
        | ExprNode::Window(_)
        | ExprNode::Row(_) => false,
    }
}

fn value(datum: Datum) -> ExprNode {
    ExprNode::Value(ValueExpr { datum })
}

fn as_const(expr: &ExprNode) -> Option<&Datum> {
    match expr {
        ExprNode::Value(v) => Some(&v.datum),
        _ => None,
    }
}

/// TiDB `ifnull(NULL, x)` / `if(<const>, x, y)` / `coalesce` short-circuit.
fn fold_control(func: &sealdb_parser::ast::expressions::FuncCallExpr) -> Option<ExprNode> {
    let name = func.name.to_ascii_lowercase();
    match name.as_str() {
        // `ifnull(const, x)` keeps the constant when it is not NULL.
        "ifnull" | "nvl" if func.args.len() == 2 => {
            let first = as_const(&func.args[0])?;
            Some(if first.is_null() {
                func.args[1].clone()
            } else {
                func.args[0].clone()
            })
        }
        "if" if func.args.len() == 3 => {
            let cond = as_const(&func.args[0])?;
            let taken = if truthy(cond)? {
                &func.args[1]
            } else {
                &func.args[2]
            };
            Some(taken.clone())
        }
        "coalesce" => {
            for arg in &func.args {
                let datum = as_const(arg)?;
                if !datum.is_null() {
                    return Some(arg.clone());
                }
            }
            None
        }
        _ => None,
    }
}

/// Bottom-up constant folding. Returns `None` when nothing changed so callers
/// can skip cloning the containing plan node.
pub fn fold_constants(expr: &ExprNode) -> Option<ExprNode> {
    fold_constants_opts(
        expr,
        true,
        FoldCtx {
            select: None,
            is: None,
            current_db: "",
        },
    )
}

/// Statement context for the collation-aware contradiction check: two
/// equalities on one column only conflict when the values differ under that
/// column's collation (`a = 'a' AND a = 'A'` is satisfiable in
/// `utf8mb4_general_ci`).
#[derive(Clone, Copy)]
struct FoldCtx<'a> {
    select: Option<&'a SelectStmt>,
    is: Option<&'a InfoSchema>,
    current_db: &'a str,
}

/// Like [`fold_constants`], but when `collapse_conflicts` is false keeps
/// contradictory equalities (`a = 1 AND a = 2`) intact so memtable extractors
/// can turn them into `skip_request` / `skip_inspection`.
fn fold_constants_opts(
    expr: &ExprNode,
    collapse_conflicts: bool,
    ctx: FoldCtx<'_>,
) -> Option<ExprNode> {
    let mut changed = false;
    let rebuilt = fold_children(expr, &mut changed, collapse_conflicts, ctx);
    // `ifnull(NULL, x)` / `if(<const>, x, y)` / `coalesce`: TiDB replaces the
    // function with the decided branch even when that branch is not constant.
    if let ExprNode::Func(f) = &rebuilt
        && let Some(branch) = fold_control(f)
    {
        return Some(fold_constants_opts(&branch, collapse_conflicts, ctx).unwrap_or(branch));
    }
    if !changed && !is_constant(&rebuilt) {
        return None;
    }
    // A JSON cast is part of the predicate TiDB prints (`cast(1, json BINARY)`);
    // folding it to a JSON literal would lose the comparison type information.
    // A `cast(… AS BINARY)` constant is the same: the binary charset is what
    // makes `col = cast('a' AS BINARY)` case-sensitive, so the node has to
    // survive until evaluation (and EXPLAIN prints the plain literal).
    if let ExprNode::Cast(cast) = &rebuilt
        && (cast.target.to_ascii_lowercase().contains("json")
            || sealdb_parser::ast::expressions::is_binary_cast_target(&cast.target))
    {
        return changed.then_some(rebuilt);
    }
    if let Some(folded) = try_eval(&rebuilt) {
        return Some(folded);
    }
    changed.then_some(rebuilt)
}

fn fold_children(
    expr: &ExprNode,
    changed: &mut bool,
    collapse_conflicts: bool,
    ctx: FoldCtx<'_>,
) -> ExprNode {
    fn fold(
        e: &ExprNode,
        changed: &mut bool,
        collapse_conflicts: bool,
        ctx: FoldCtx<'_>,
    ) -> ExprNode {
        match fold_constants_opts(e, collapse_conflicts, ctx) {
            Some(folded) => {
                *changed = true;
                folded
            }
            None => e.clone(),
        }
    }
    use sealdb_parser::ast::expressions as ast;
    match expr {
        ExprNode::Binary(b) => {
            let left = fold(&b.left, changed, collapse_conflicts, ctx);
            let right = fold(&b.right, changed, collapse_conflicts, ctx);
            // Short-circuit boolean folds TiDB applies even with a non-constant
            // sibling: `x AND false` → `0`, `x AND NULL` → `NULL`, `x OR true` → `1`.
            if b.op == sealdb_parser::opcode::Op::LogicAnd {
                for side in [&left, &right] {
                    if let ExprNode::Value(v) = side {
                        if v.datum.is_null() {
                            *changed = true;
                            return ExprNode::Value(ValueExpr { datum: Datum::Null });
                        }
                        if truthy(&v.datum) == Some(false) {
                            *changed = true;
                            return ExprNode::Value(ValueExpr {
                                datum: Datum::I64(0),
                            });
                        }
                    }
                }
            }
            if b.op == sealdb_parser::opcode::Op::LogicOr {
                for side in [&left, &right] {
                    if let ExprNode::Value(v) = side {
                        if !v.datum.is_null() && truthy(&v.datum) == Some(true) {
                            *changed = true;
                            return ExprNode::Value(ValueExpr {
                                datum: Datum::I64(1),
                            });
                        }
                    }
                }
            }
            // `col = NULL` / `col <=> NULL` with a non-null-safe `=`: SQL
            // UNKNOWN, which TiDB folds to a zero-row plan under WHERE.
            if matches!(
                b.op,
                sealdb_parser::opcode::Op::Eq
                    | sealdb_parser::opcode::Op::Ne
                    | sealdb_parser::opcode::Op::Lt
                    | sealdb_parser::opcode::Op::Le
                    | sealdb_parser::opcode::Op::Gt
                    | sealdb_parser::opcode::Op::Ge
            ) {
                let null_side = matches!(&left, ExprNode::Value(v) if v.datum.is_null())
                    || matches!(&right, ExprNode::Value(v) if v.datum.is_null());
                if null_side {
                    *changed = true;
                    return ExprNode::Value(ValueExpr { datum: Datum::Null });
                }
            }
            let rebuilt = ExprNode::Binary(ast::BinaryOpExpr {
                left: Box::new(left),
                op: b.op,
                right: Box::new(right),
                single_cnf_item: b.single_cnf_item,
            });
            // Nested `a = 2 AND a = 3` (e.g. inside an OR arm) is unsatisfiable:
            // fold it to `0` so TiDB shapes like `or(eq(a, 1), 0)` appear.
            // Memtable extractors need the conflict intact → skip when asked.
            if collapse_conflicts
                && b.op == sealdb_parser::opcode::Op::LogicAnd
                && has_conflicting_equalities(&rebuilt, ctx.select, ctx.is, ctx.current_db)
            {
                *changed = true;
                return ExprNode::Value(ValueExpr {
                    datum: Datum::I64(0),
                });
            }
            rebuilt
        }
        ExprNode::Unary(u) => ExprNode::Unary(ast::UnaryOpExpr {
            op: u.op,
            expr: Box::new(fold(&u.expr, changed, collapse_conflicts, ctx)),
        }),
        ExprNode::Func(f) => ExprNode::Func(ast::FuncCallExpr {
            name: f.name.clone(),
            args: f
                .args
                .iter()
                .map(|a| fold(a, changed, collapse_conflicts, ctx))
                .collect(),
            distinct: f.distinct,
        }),
        ExprNode::Cast(c) => ExprNode::Cast(ast::FuncCastExpr {
            expr: Box::new(fold(&c.expr, changed, collapse_conflicts, ctx)),
            target: c.target.clone(),
            json: c.json,
        }),
        ExprNode::InList(i) => ExprNode::InList(ast::InListExpr {
            expr: Box::new(fold(&i.expr, changed, collapse_conflicts, ctx)),
            not: i.not,
            list: i
                .list
                .iter()
                .map(|a| fold(a, changed, collapse_conflicts, ctx))
                .collect(),
        }),
        ExprNode::Between(b) => ExprNode::Between(ast::BetweenExpr {
            expr: Box::new(fold(&b.expr, changed, collapse_conflicts, ctx)),
            not: b.not,
            low: Box::new(fold(&b.low, changed, collapse_conflicts, ctx)),
            high: Box::new(fold(&b.high, changed, collapse_conflicts, ctx)),
        }),
        ExprNode::Like(l) => ExprNode::Like(ast::PatternLikeExpr {
            expr: Box::new(fold(&l.expr, changed, collapse_conflicts, ctx)),
            not: l.not,
            pattern: Box::new(fold(&l.pattern, changed, collapse_conflicts, ctx)),
            ci: l.ci,
        }),
        ExprNode::IsNull(i) => ExprNode::IsNull(ast::IsNullExpr {
            expr: Box::new(fold(&i.expr, changed, collapse_conflicts, ctx)),
            not: i.not,
        }),
        ExprNode::Case(c) => ExprNode::Case(ast::CaseExpr {
            operand: c
                .operand
                .as_ref()
                .map(|e| Box::new(fold(e, changed, collapse_conflicts, ctx))),
            when_then: c
                .when_then
                .iter()
                .map(|(w, t)| {
                    (
                        fold(w, changed, collapse_conflicts, ctx),
                        fold(t, changed, collapse_conflicts, ctx),
                    )
                })
                .collect(),
            else_expr: c
                .else_expr
                .as_ref()
                .map(|e| Box::new(fold(e, changed, collapse_conflicts, ctx))),
        }),
        other => other.clone(),
    }
}

/// A literal `NULL` anywhere in the tree (used to keep TiDB's "inlined view
/// column" shape: TiDB substitutes the view's `NULL AS col` *after* its
/// constant-fold pass, so the enclosing expression stays unfolded).
fn has_null_literal(expr: &ExprNode) -> bool {
    match expr {
        ExprNode::Value(v) => v.datum.is_null(),
        ExprNode::Binary(b) => has_null_literal(&b.left) || has_null_literal(&b.right),
        ExprNode::Unary(u) => has_null_literal(&u.expr),
        ExprNode::Func(f) => f.args.iter().any(has_null_literal),
        ExprNode::Cast(c) => has_null_literal(&c.expr),
        ExprNode::InList(i) => has_null_literal(&i.expr) || i.list.iter().any(has_null_literal),
        ExprNode::Between(b) => {
            has_null_literal(&b.expr) || has_null_literal(&b.low) || has_null_literal(&b.high)
        }
        ExprNode::Like(l) => has_null_literal(&l.expr) || has_null_literal(&l.pattern),
        ExprNode::IsNull(i) => has_null_literal(&i.expr),
        ExprNode::Case(c) => {
            c.operand.as_ref().is_some_and(|e| has_null_literal(e))
                || c.when_then
                    .iter()
                    .any(|(w, t)| has_null_literal(w) || has_null_literal(t))
                || c.else_expr.as_ref().is_some_and(|e| has_null_literal(e))
        }
        _ => false,
    }
}

/// Evaluate a rebuilt expression that is fully constant. `None` means "leave
/// it to the executor".
fn try_eval(expr: &ExprNode) -> Option<ExprNode> {
    if !is_constant(expr) {
        return None;
    }
    if has_null_literal(expr) {
        return None;
    }
    // TiDB's parser produces a negative literal directly; SealDB keeps the
    // `unary minus` node so the planner sees the same AST shape (folding it
    // would change how range/NDV code reads `-9223372036854775808`).
    if matches!(expr, ExprNode::Unary(_)) {
        return None;
    }
    // Never fold a bare literal into itself.
    if matches!(expr, ExprNode::Value(_)) {
        return None;
    }
    eval_expr(expr).ok().map(value)
}

/// `true` when two constant equalities on the same column disagree
/// (`a = 1 AND a = 2`), which no row can satisfy.
///
/// The column's table qualifier is part of the identity: the same CTE read
/// twice (`FROM q, q q1 WHERE q.a = 1 AND q1.a = 2`) constrains two different
/// scans, not one column twice.
fn has_conflicting_equalities(
    expr: &ExprNode,
    select: Option<&SelectStmt>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> bool {
    let equalities: Vec<(Option<String>, String, Datum)> = expr
        .split_cnf()
        .into_iter()
        .filter_map(|conjunct| qualified_column_const(&conjunct))
        .collect();
    equalities
        .iter()
        .enumerate()
        .any(|(i, (table, column, value))| {
            equalities[i + 1..]
                .iter()
                .any(|(other_table, other_column, other_value)| {
                    let same_table = match (table, other_table) {
                        // An unqualified name resolves to the single table in
                        // scope; a qualified one to its alias.
                        (None, None) => true,
                        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                        _ => false,
                    };
                    if !same_table || !column.eq_ignore_ascii_case(other_column) {
                        return false;
                    }
                    // `a = 'a' AND a = 'A'` is satisfiable under a
                    // case-insensitive collation (`utf8mb4_general_ci`).
                    let collation =
                        column_collation(select, table.as_deref(), column, is, current_db);
                    match collation {
                        Some(collation) => !eq_under_collation(&collation, value, other_value),
                        None => value.compare(other_value) != std::cmp::Ordering::Equal,
                    }
                })
        })
}

/// Collation of a `col = const` equality's column, resolved through the
/// statement's FROM table (only string columns carry one).
fn column_collation(
    select: Option<&SelectStmt>,
    table: Option<&str>,
    column: &str,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<String> {
    let is = is?;
    let resolve = |name: &str| -> Option<String> {
        let info = is.table_by_name(current_db, name)?;
        let col = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(column))?;
        let ft = &col.field_type;
        (ft.eval_type() == sealdb_types::field_type::EvalType::String
            && !ft.charset.is_empty()
            && !ft.charset.eq_ignore_ascii_case("binary")
            && !ft.collate.eq_ignore_ascii_case("binary"))
        .then(|| ft.collate.clone())
    };
    if let Some(table) = table {
        return resolve(table);
    }
    // Unqualified: the single table in FROM (verified by the column lookup).
    let from = select?.from.as_ref()?;
    resolve(from.resolve_name())
}

/// Datum equality using `collation` (MySQL string comparison) instead of raw
/// bytes: `'a'` and `'A'` are equal under `utf8mb4_general_ci`.
fn eq_under_collation(collation: &str, left: &Datum, right: &Datum) -> bool {
    let strings = |d: &Datum| matches!(d, Datum::Bytes(_) | Datum::Bit(_));
    if !strings(left) || !strings(right) {
        return left.compare(right).is_eq();
    }
    let mode = sealdb_expression::CollateMode::from_name(collation);
    sealdb_expression::with_collation(mode, || {
        sealdb_expression::datum_sql_cmp(left, right).is_ok_and(|ord| ord.is_eq())
    })
}

/// `col = const` keeping the written table qualifier (`None` when unqualified).
fn qualified_column_const(expr: &ExprNode) -> Option<(Option<String>, String, Datum)> {
    let ExprNode::Binary(bin) = expr else {
        return None;
    };
    if !matches!(
        bin.op,
        sealdb_parser::opcode::Op::Eq | sealdb_parser::opcode::Op::NullSafeEq
    ) {
        return None;
    }
    let (column, other) = match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(column), other) | (other, ExprNode::Column(column)) => (column, other),
        _ => return None,
    };
    other.const_datum().map(|value| {
        (
            column.table.clone(),
            column.name.to_ascii_lowercase(),
            value,
        )
    })
}

/// Fold a select statement's select list and filters. Returns `None` when the
/// statement is unchanged.
pub fn fold_constant_select(
    select: &SelectStmt,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> Option<SelectStmt> {
    let mut changed = false;

    // Inspection / cluster memtable extractors absorb contradictory equalities
    // into `skip_inspection` / `skip_request` on MemTableScan — do not collapse
    // those conflicts to `0` during constant fold.
    let keep_conflict_for_extractor = select.from.as_ref().is_some_and(|from| {
        let name = from.resolve_name().to_ascii_lowercase();
        matches!(
            name.as_str(),
            "inspection_result"
                | "inspection_rules"
                | "inspection_summary"
                | "statistics"
                | "tables"
                | "key_column_usage"
                | "referential_constraints"
                | "columns"
                | "schemata"
        )
    });
    let collapse_conflicts = !keep_conflict_for_extractor;
    let fold_ctx = FoldCtx {
        select: Some(select),
        is,
        current_db,
    };

    let fields = select
        .fields
        .iter()
        .map(
            |field| match fold_constants_opts(&field.expr, collapse_conflicts, fold_ctx) {
                Some(expr) => {
                    changed = true;
                    let mut out = field.clone();
                    out.expr = expr;
                    out
                }
                None => field.clone(),
            },
        )
        .collect::<Vec<_>>();

    let fold_filter = |filter: &Option<ExprNode>, changed: &mut bool| match filter {
        Some(expr) => {
            let (folded, did_fold) = match fold_constants_opts(expr, collapse_conflicts, fold_ctx) {
                Some(folded) => (folded, true),
                None => (expr.clone(), false),
            };
            // `WHERE <true>` is not a filter at all (including a constant `1`
            // already produced by an earlier rewrite); a false/`NULL`
            // predicate stays so the planner can prune to `TableDual`.
            match &folded {
                ExprNode::Value(v) if !v.datum.is_null() && truthy(&v.datum) == Some(true) => {
                    *changed = true;
                    None
                }
                _ => {
                    if did_fold {
                        *changed = true;
                    }
                    Some(folded)
                }
            }
        }
        None => None,
    };
    let selection = fold_filter(&select.selection, &mut changed);
    // TiDB `PredicateSimplification`: conflicting constant equalities on one
    // column (`a = 1 AND a = 2`) are unsatisfiable, so the WHERE folds to
    // `0` and the plan is a zero-row `TableDual` (unless a memtable extractor
    // needs the conflict for `skip_request` / `skip_inspection`).
    let selection = match selection {
        Some(expr)
            if collapse_conflicts
                && has_conflicting_equalities(&expr, Some(select), is, current_db) =>
        {
            changed = true;
            Some(ExprNode::Value(ValueExpr {
                datum: Datum::I64(0),
            }))
        }
        Some(expr) => {
            // Deduplicate identical CNF conjuncts (`a = 1 AND a = 1` → `a = 1`).
            let mut seen = Vec::new();
            let mut uniq = Vec::new();
            for conjunct in expr.split_cnf_all() {
                let key = format!("{conjunct:?}");
                if seen.iter().any(|s: &String| s == &key) {
                    changed = true;
                    continue;
                }
                seen.push(key);
                uniq.push(conjunct);
            }
            if uniq.len() != expr.split_cnf_all().len() {
                ExprNode::compose_cnf(uniq)
            } else {
                Some(expr)
            }
        }
        other => other,
    };
    let having = fold_filter(&select.having, &mut changed);

    // Fold join ON predicates too (`ON t1.a = 1 OR (t1.a = 2 AND t1.a = 3)`
    // → `or(eq(t1.a, 1), 0)`), matching TiDB's FoldConstant over the join.
    let joins: Vec<_> = select
        .joins
        .iter()
        .map(
            |join| match fold_constants_opts(&join.on, collapse_conflicts, fold_ctx) {
                Some(on) => {
                    changed = true;
                    let mut out = join.clone();
                    out.on = on;
                    out
                }
                None => join.clone(),
            },
        )
        .collect();

    if !changed {
        return None;
    }
    let mut out = select.clone();
    out.fields = fields;
    out.selection = selection;
    out.having = having;
    out.joins = joins;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold_sql(sql: &str) -> SelectStmt {
        let parser = sealdb_parser::Parser::new();
        let stmt = parser.parse_one_stmt(sql).expect("parse");
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!("expected select");
        };
        fold_constant_select(&select).expect("changed")
    }

    fn first_expr(select: &SelectStmt) -> &ExprNode {
        &select.fields[0].expr
    }

    #[test]
    fn folds_ifnull_with_null_first_arg() {
        let select = fold_sql("select ifnull(null, a) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "a"));
    }

    #[test]
    fn folds_decided_if_branch() {
        let select = fold_sql("select if(10, a, b) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "a"));
        let select = fold_sql("select if(0, a, b) from t");
        assert!(matches!(first_expr(&select), ExprNode::Column(c) if c.name == "b"));
    }

    #[test]
    fn folds_constant_arithmetic() {
        let select = fold_sql("select 1 + 2 * 3");
        assert!(matches!(first_expr(&select), ExprNode::Value(v) if v.datum == Datum::I64(7)));
    }

    #[test]
    fn keeps_volatile_functions() {
        let parser = sealdb_parser::Parser::new();
        let stmt = parser.parse_one_stmt("select now(), rand()").unwrap();
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!();
        };
        assert!(fold_constant_select(&select).is_none());
    }

    #[test]
    fn drops_true_filter_and_keeps_false_one() {
        assert!(fold_sql("select a from t where 1 = 1").selection.is_none());
        let select = fold_sql("select a from t where 1 = 0");
        assert!(matches!(
            select.selection,
            Some(ExprNode::Value(v)) if v.datum == Datum::I64(0)
        ));
    }
}

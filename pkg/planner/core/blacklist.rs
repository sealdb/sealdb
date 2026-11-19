//! TiDB optimizer/expression blacklists.
//!
//! `mysql.opt_rule_blacklist` disables whole logical rules (`predicate_push_down`)
//! and `mysql.expr_pushdown_blacklist` keeps selected expressions out of the
//! coprocessor. TiDB only picks up edits after `ADMIN RELOAD`, so the session
//! scans the two tables on reload (and lazily on first use) and publishes the
//! snapshot here before planning.
//!
//! Names are TiDB `ScalarFunction` names (`lt`, `gt`, `eq`, `plus`, …). A row may
//! also store the operator spelling (`<`), which TiDB parses into the same
//! function name with `EvalAstExpr`, so the snapshot normalizes both forms.

use std::cell::RefCell;
use std::collections::HashSet;

use sealdb_parser::opcode::Op;

/// Snapshot of the two blacklist tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlannerBlacklist {
    /// `mysql.opt_rule_blacklist.name` (lower-case).
    pub opt_rules: HashSet<String>,
    /// `mysql.expr_pushdown_blacklist.name` for stores SealDB pushes to
    /// (`tikv` / `tidb`), normalized to TiDB function names.
    pub exprs: HashSet<String>,
}

thread_local! {
    static PLANNER_BLACKLIST: RefCell<PlannerBlacklist> =
        RefCell::new(PlannerBlacklist::default());
}

/// Run `f` with the session's blacklist snapshot visible to the planner.
pub fn with_planner_blacklist<T>(blacklist: PlannerBlacklist, f: impl FnOnce() -> T) -> T {
    let prev = PLANNER_BLACKLIST.with(|cell| cell.replace(blacklist));
    let out = f();
    PLANNER_BLACKLIST.with(|cell| cell.replace(prev));
    out
}

/// Is the logical rule `name` (`predicate_push_down`) disabled?
pub fn opt_rule_disabled(name: &str) -> bool {
    PLANNER_BLACKLIST.with(|cell| cell.borrow().opt_rules.contains(name))
}

/// Is the expression `name` (`lt`, `plus`, …) blacklisted from the coprocessor?
pub fn expr_pushdown_disabled(name: &str) -> bool {
    PLANNER_BLACKLIST.with(|cell| {
        let blacklist = cell.borrow();
        !blacklist.exprs.is_empty() && blacklist.exprs.contains(name)
    })
}

/// Is the expression `expr` itself blacklisted from the coprocessor?
pub fn expr_node_pushdown_disabled(expr: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::ExprNode;
    match expr {
        ExprNode::Func(func) => {
            let name = func.name.to_ascii_lowercase();
            expr_pushdown_disabled(&name) || expr_pushdown_disabled(strip_prefix(&name))
        }
        ExprNode::Binary(bin) => expr_pushdown_disabled(op_name(bin.op)),
        ExprNode::Unary(unary) => expr_pushdown_disabled(op_name(unary.op)),
        _ => false,
    }
}

/// Does any node of `expr` name a blacklisted coprocessor function?
///
/// TiDB keeps the whole conjunct on the tidb side when one of its functions is
/// blacklisted, so the residual `Selection` never had a range derived for it.
pub fn expr_contains_blacklisted_pushdown(expr: &sealdb_parser::ast::ExprNode) -> bool {
    use sealdb_parser::ast::ExprNode;
    if expr_node_pushdown_disabled(expr) {
        return true;
    }
    match expr {
        ExprNode::Func(func) => func.args.iter().any(expr_contains_blacklisted_pushdown),
        ExprNode::Binary(bin) => {
            expr_contains_blacklisted_pushdown(&bin.left)
                || expr_contains_blacklisted_pushdown(&bin.right)
        }
        ExprNode::Unary(unary) => expr_contains_blacklisted_pushdown(&unary.expr),
        ExprNode::Cast(cast) => expr_contains_blacklisted_pushdown(&cast.expr),
        ExprNode::InList(list) => {
            expr_contains_blacklisted_pushdown(&list.expr)
                || list.list.iter().any(expr_contains_blacklisted_pushdown)
        }
        ExprNode::Between(between) => {
            expr_contains_blacklisted_pushdown(&between.expr)
                || expr_contains_blacklisted_pushdown(&between.low)
                || expr_contains_blacklisted_pushdown(&between.high)
        }
        ExprNode::IsNull(is_null) => expr_contains_blacklisted_pushdown(&is_null.expr),
        ExprNode::Like(like) => {
            expr_contains_blacklisted_pushdown(&like.expr)
                || expr_contains_blacklisted_pushdown(&like.pattern)
        }
        _ => false,
    }
}

/// `cast_enum` / `cast_*` blacklist rows name a function family; a row may carry
/// a `cast_` prefix even though the expression reports the bare name.
fn strip_prefix(name: &str) -> &str {
    name.strip_prefix("cast_").unwrap_or(name)
}

/// TiDB `ScalarFunction` name for an operator (`<` → `lt`).
pub fn op_name(op: Op) -> &'static str {
    match op {
        Op::Plus => "plus",
        Op::Minus => "minus",
        Op::Mul => "mul",
        Op::Div => "div",
        Op::IntDiv => "intdiv",
        Op::Mod => "mod",
        Op::Eq => "eq",
        Op::NullSafeEq => "nulleq",
        Op::Ne => "ne",
        Op::Lt => "lt",
        Op::Le => "le",
        Op::Gt => "gt",
        Op::Ge => "ge",
        Op::LogicAnd => "and",
        Op::LogicOr => "or",
        Op::LogicXor => "xor",
        Op::LeftShift => "leftshift",
        Op::RightShift => "rightshift",
        Op::BitAnd => "bitand",
        Op::BitOr => "bitor",
        Op::BitXor => "bitxor",
        Op::Not | Op::Not2 => "unarynot",
        Op::BitNeg => "bitneg",
        Op::UnaryMinus => "unaryminus",
    }
}

/// Normalize a blacklist row's `name` the way TiDB's `EvalAstExpr` does:
/// operator spellings become function names, function names are lower-cased.
pub fn normalize_expr_name(name: &str) -> String {
    let trimmed = name.trim().trim_matches(['`', '\'', '"']).trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "<" => "lt",
        "<=" => "le",
        ">" => "gt",
        ">=" => "ge",
        "=" | "==" => "eq",
        "!=" | "<>" => "ne",
        "<=>" => "nulleq",
        "+" => "plus",
        "-" => "minus",
        "*" => "mul",
        "/" => "div",
        "%" => "mod",
        "&" => "bitand",
        "|" => "bitor",
        "^" => "bitxor",
        "<<" => "leftshift",
        ">>" => "rightshift",
        "not" | "!" => "unarynot",
        _ => return lower,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_spellings_become_function_names() {
        assert_eq!(normalize_expr_name("<"), "lt");
        assert_eq!(normalize_expr_name("LT"), "lt");
        assert_eq!(normalize_expr_name("`lt`"), "lt");
        assert_eq!(normalize_expr_name("+"), "plus");
        assert_eq!(normalize_expr_name("enum"), "enum");
    }

    #[test]
    fn snapshot_gates_rules_and_expressions() {
        let blacklist = PlannerBlacklist {
            opt_rules: ["predicate_push_down".to_string()].into_iter().collect(),
            exprs: ["lt".to_string()].into_iter().collect(),
        };
        with_planner_blacklist(blacklist, || {
            assert!(opt_rule_disabled("predicate_push_down"));
            assert!(!opt_rule_disabled("join_reorder"));
            assert!(expr_pushdown_disabled("lt"));
            assert!(!expr_pushdown_disabled("gt"));
        });
        // The snapshot is restored once the scope exits.
        assert!(!opt_rule_disabled("predicate_push_down"));
        assert!(!expr_pushdown_disabled("lt"));
    }

    #[test]
    fn empty_snapshot_disables_nothing() {
        assert!(!expr_pushdown_disabled("lt"));
        assert!(!opt_rule_disabled("predicate_push_down"));
    }
}

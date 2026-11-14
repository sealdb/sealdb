/// partition prune modes (TiDB `tidb_partition_prune_mode`).
/// Static prunes at plan time; dynamic defers to execute using the pushed filter.
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ExprNode;
use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, InListExpr, ValueExpr};

/// TiDB `variable.PartitionPruneMode` (legacy *-only aliases normalize here).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionPruneMode {
    Static,
    Dynamic,
}

impl PartitionPruneMode {
    /// Parse TiDB `tidb_partition_prune_mode` values.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "static" | "static-only" | "static-collect-dynamic" => Some(Self::Static),
            "dynamic" | "dynamic-only" | "default" => Some(Self::Dynamic),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
        }
    }
}

/// Resolved partition set for planning / EXPLAIN.
#[derive(Clone, Debug, Default)]
pub struct PlannedPartitions {
    /// Physical partition ids (empty in dynamic mode so the executor prunes).
    pub ids: Vec<i64>,
    /// Partition names in definition order (`p0`, `p1`, …).
    pub names: Vec<String>,
    /// True when every partition of the table is included.
    pub all: bool,
}

/// Plan-time partition list. Dynamic returns empty ids so the executor prunes
/// with the pushed predicate (TiDB execute-phase prune); names stay for EXPLAIN.
pub fn plan_partitions(
    info: &TableInfo,
    selection: Option<&ExprNode>,
    mode: PartitionPruneMode,
) -> Vec<i64> {
    resolve_partitions(info, selection, &[], mode).ids
}

/// Resolve partitions after predicate prune and optional `PARTITION (pN, …)`.
pub fn resolve_partitions(
    info: &TableInfo,
    selection: Option<&ExprNode>,
    named: &[String],
    mode: PartitionPruneMode,
) -> PlannedPartitions {
    let Some(part) = info.partition.as_ref() else {
        return PlannedPartitions::default();
    };
    if !part.enable || part.definitions.is_empty() {
        return PlannedPartitions::default();
    }

    let pruned = prune_with_expr(info, selection);
    let mut defs: Vec<_> = part
        .definitions
        .iter()
        .filter(|d| pruned.contains(&d.id))
        .collect();
    if !named.is_empty() {
        defs.retain(|d| named.iter().any(|n| d.name.o().eq_ignore_ascii_case(n)));
        // Keep definition order (TiDB EXPLAIN `p1,p2` for `PARTITION (p2,p1)`).
        let mut ordered = Vec::new();
        for d in &part.definitions {
            if defs.iter().any(|x| x.id == d.id) {
                ordered.push(d);
            }
        }
        defs = ordered;
    }

    let names: Vec<String> = defs.iter().map(|d| d.name.o().to_string()).collect();
    let ids: Vec<i64> = defs.iter().map(|d| d.id).collect();
    let all = ids.len() == part.definitions.len() && named.is_empty();

    match mode {
        PartitionPruneMode::Static => PlannedPartitions { ids, names, all },
        // Dynamic: the executor prunes with the pushed predicate, but an
        // explicit `PARTITION (pN)` narrows the scan regardless of the mode.
        PartitionPruneMode::Dynamic => PlannedPartitions {
            ids: if named.is_empty() { Vec::new() } else { ids },
            names,
            all,
        },
    }
}

pub fn prune_partitions(info: &TableInfo, selection: Option<&ExprNode>) -> Vec<i64> {
    prune_with_expr(info, selection)
}

/// TiDB evaluates an expression partition key (`weekday(a)`,
/// `floor(unix_timestamp(a))`) for the constants in the pushed predicate so the
/// derived value can drive `locate`. Meta only stores the expression text; the
/// rewrite happens here where the expression evaluator is available.
fn prune_with_expr(info: &TableInfo, selection: Option<&ExprNode>) -> Vec<i64> {
    let Some(part) = info.partition.as_ref() else {
        return info.prune_partitions(selection);
    };
    if !part.enable || !part.is_expr_key() || part.definitions.is_empty() {
        return info.prune_partitions(selection);
    }
    let Some(selection) = selection else {
        return info.prune_partitions(None);
    };
    let Ok(key_expr) = sealdb_parser::sqlparser_bridge::parse_expr(&part.expr) else {
        return info.prune_partitions(Some(selection));
    };
    let key_columns = expr_key_columns(&key_expr);
    if key_columns.is_empty() {
        return info.prune_partitions(Some(selection));
    }
    // Deriving the key from `col = const` is only sound when the expression
    // depends on that one column: with more columns the rest stay NULL, so the
    // derived value (e.g. `id*2 + b*b …` → NULL for unknown `b`) is not the
    // row's partition key.
    let mut distinct = key_columns.clone();
    distinct.sort_by_key(|c| c.to_ascii_lowercase());
    distinct.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if distinct.len() > 1 {
        return info.prune_partitions(Some(selection));
    }
    let monotonic = expr_is_monotonic(&key_expr);
    let rewritten = rewrite_prune_selection(
        selection,
        &part.expr,
        &key_expr,
        &key_columns,
        monotonic,
        info,
    );
    info.prune_partitions(Some(&rewritten))
}

/// Range pruning is only valid when the partition expression is monotonic in
/// the key column; `weekday(a)` wraps, so `a < d` cannot pin a partition.
fn expr_is_monotonic(expr: &ExprNode) -> bool {
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Column(_) | ExprNode::Value(_) => true,
        ExprNode::Binary(bin) if matches!(bin.op, Op::Plus | Op::Minus) => {
            expr_is_monotonic(&bin.left) && expr_is_monotonic(&bin.right)
        }
        ExprNode::Func(func) => {
            if matches!(
                func.name.to_ascii_uppercase().as_str(),
                "WEEKDAY"
                    | "DAYOFWEEK"
                    | "DAYOFMONTH"
                    | "DAYOFYEAR"
                    | "MONTH"
                    | "YEAR"
                    | "HOUR"
                    | "MINUTE"
                    | "SECOND"
            ) {
                return false;
            }
            func.args.iter().all(expr_is_monotonic)
        }
        ExprNode::Cast(c) => expr_is_monotonic(&c.expr),
        _ => false,
    }
}

fn expr_key_columns(expr: &ExprNode) -> Vec<String> {
    let mut out = Vec::new();
    collect_key_columns(expr, &mut out);
    out
}

fn collect_key_columns(expr: &ExprNode, out: &mut Vec<String>) {
    match expr {
        ExprNode::Column(c) => out.push(c.name.clone()),
        ExprNode::Binary(bin) => {
            collect_key_columns(&bin.left, out);
            collect_key_columns(&bin.right, out);
        }
        ExprNode::Unary(unary) => collect_key_columns(&unary.expr, out),
        ExprNode::Func(func) => {
            for arg in &func.args {
                collect_key_columns(arg, out);
            }
        }
        ExprNode::Cast(c) => collect_key_columns(&c.expr, out),
        _ => {}
    }
}

/// Replace `key_col <op> const` with `<key_expr> <op> key_expr(const)` so the
/// meta pruner can locate the derived partition value.
fn rewrite_prune_selection(
    expr: &ExprNode,
    key_text: &str,
    key_expr: &ExprNode,
    key_columns: &[String],
    monotonic: bool,
    info: &TableInfo,
) -> ExprNode {
    use sealdb_parser::ast::ExprNode;
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Binary(bin) => {
            if matches!(bin.op, Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Ne)
                && (monotonic || bin.op == Op::Eq)
            {
                let lhs_key = key_column_of(&bin.left, key_columns);
                let rhs_key = key_column_of(&bin.right, key_columns);
                if let (Some(col), None, Some(value)) = (lhs_key, rhs_key, const_datum(&bin.right))
                    && let Some(derived) = eval_key_expr(key_expr, info, col, &value)
                {
                    return make_key_cmp(bin.op, key_text, &bin.left, derived);
                }
                if let (None, Some(col), Some(value)) = (lhs_key, rhs_key, const_datum(&bin.left))
                    && let Some(derived) = eval_key_expr(key_expr, info, col, &value)
                {
                    return make_key_cmp(flip_cmp(bin.op), key_text, &bin.right, derived);
                }
            }
            if bin.op == Op::LogicAnd || bin.op == Op::LogicOr {
                return ExprNode::Binary(BinaryOpExpr {
                    op: bin.op,
                    left: Box::new(rewrite_prune_selection(
                        &bin.left,
                        key_text,
                        key_expr,
                        key_columns,
                        monotonic,
                        info,
                    )),
                    right: Box::new(rewrite_prune_selection(
                        &bin.right,
                        key_text,
                        key_expr,
                        key_columns,
                        monotonic,
                        info,
                    )),
                    single_cnf_item: bin.single_cnf_item,
                });
            }
            expr.clone()
        }
        ExprNode::InList(list) if !list.not => {
            let Some(col) = key_column_of(&list.expr, key_columns) else {
                return expr.clone();
            };
            let mut derived = Vec::with_capacity(list.list.len());
            for item in &list.list {
                let Some(value) = const_datum(item) else {
                    return expr.clone();
                };
                let Some(value) = eval_key_expr(key_expr, info, col, &value) else {
                    return expr.clone();
                };
                derived.push(ExprNode::Value(ValueExpr { datum: value }));
            }
            ExprNode::InList(InListExpr {
                expr: Box::new(key_column_expr(key_text, &list.expr)),
                list: derived,
                not: false,
            })
        }
        _ => expr.clone(),
    }
}

fn key_column_of<'a>(expr: &'a ExprNode, key_columns: &[String]) -> Option<&'a str> {
    use sealdb_parser::ast::ExprNode;
    let ExprNode::Column(c) = expr else {
        return None;
    };
    key_columns
        .iter()
        .find(|k| k.eq_ignore_ascii_case(&c.name))
        .map(|_| c.name.as_str())
}

fn make_key_cmp(
    op: sealdb_parser::opcode::Op,
    key_text: &str,
    original: &ExprNode,
    value: sealdb_types::Datum,
) -> ExprNode {
    ExprNode::Binary(BinaryOpExpr {
        op,
        left: Box::new(key_column_expr(key_text, original)),
        right: Box::new(ExprNode::Value(ValueExpr { datum: value })),
        single_cnf_item: false,
    })
}

fn key_column_expr(key_text: &str, original: &ExprNode) -> ExprNode {
    let (schema, table) = match original {
        ExprNode::Column(c) => (c.schema.clone(), c.table.clone()),
        _ => (None, None),
    };
    ExprNode::Column(ColumnNameExpr {
        schema,
        table,
        name: key_text.to_string(),
    })
}

fn const_datum(expr: &ExprNode) -> Option<sealdb_types::Datum> {
    match expr {
        ExprNode::Value(ValueExpr { datum }) if !datum.is_null() => Some(datum.clone()),
        // Negative literals parse as `UnaryMinus(Value)`.
        ExprNode::Unary(u) if u.op == sealdb_parser::opcode::Op::UnaryMinus => {
            match u.expr.as_ref() {
                ExprNode::Value(ValueExpr {
                    datum: sealdb_types::Datum::I64(v),
                }) => Some(sealdb_types::Datum::I64(v.wrapping_neg())),
                ExprNode::Value(ValueExpr {
                    datum: sealdb_types::Datum::U64(v),
                }) => Some(sealdb_types::Datum::I64((*v as i64).wrapping_neg())),
                ExprNode::Value(ValueExpr {
                    datum: sealdb_types::Datum::F64(v),
                }) => Some(sealdb_types::Datum::F64(-*v)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn eval_key_expr(
    key_expr: &ExprNode,
    info: &TableInfo,
    column: &str,
    value: &sealdb_types::Datum,
) -> Option<sealdb_types::Datum> {
    let schema: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let idx = schema.iter().position(|c| c.eq_ignore_ascii_case(column))?;
    let mut row = vec![sealdb_types::Datum::Null; schema.len()];
    row[idx] = value.clone();
    sealdb_expression::eval_expr_with_row(key_expr, &schema, &row).ok()
}

fn flip_cmp(op: sealdb_parser::opcode::Op) -> sealdb_parser::opcode::Op {
    use sealdb_parser::opcode::Op;
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_meta::{ColumnInfo, PartitionDefinition, PartitionInfo, PartitionType, TableInfo};
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ExprNode, ValueExpr};
    use sealdb_parser::opcode::Op;
    use sealdb_types::{Datum, FieldType};

    fn hash_table() -> TableInfo {
        let mut info = TableInfo::new(
            1,
            "th",
            vec![ColumnInfo::new(1, 0, "a", FieldType::new_longlong())],
        );
        info.partition = Some(PartitionInfo::new(
            PartitionType::Hash,
            "a",
            vec![
                PartitionDefinition::new(101, "p0", vec![]),
                PartitionDefinition::new(102, "p1", vec![]),
                PartitionDefinition::new(103, "p2", vec![]),
            ],
        ));
        info
    }

    fn eq_a(v: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "a".into(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(v),
            })),
            single_cnf_item: false,
        })
    }

    #[test]
    fn static_eq_prunes_to_p2_for_neg_two() {
        let info = hash_table();
        let planned = resolve_partitions(&info, Some(&eq_a(-2)), &[], PartitionPruneMode::Static);
        assert_eq!(planned.names, vec!["p2"]);
        assert_eq!(planned.ids, vec![103]);
    }

    #[test]
    fn static_eq_prunes_unary_minus_literal() {
        use sealdb_parser::ast::expressions::UnaryOpExpr;
        let info = hash_table();
        let expr = ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "a".into(),
            })),
            right: Box::new(ExprNode::Unary(UnaryOpExpr {
                op: Op::UnaryMinus,
                expr: Box::new(ExprNode::Value(ValueExpr {
                    datum: Datum::I64(2),
                })),
            })),
            single_cnf_item: false,
        });
        let planned = resolve_partitions(&info, Some(&expr), &[], PartitionPruneMode::Static);
        assert_eq!(planned.names, vec!["p2"]);
    }

    #[test]
    fn partition_clause_orders_by_definition() {
        let info = hash_table();
        let planned = resolve_partitions(
            &info,
            None,
            &["p2".into(), "p1".into()],
            PartitionPruneMode::Static,
        );
        assert_eq!(planned.names, vec!["p1", "p2"]);
    }

    #[test]
    fn expr_key_div_inlist_prunes() {
        use sealdb_parser::ast::expressions::InListExpr;
        let mut info = TableInfo::new(
            1,
            "t",
            vec![ColumnInfo::new(1, 0, "col1", FieldType::new_longlong())],
        );
        info.partition = Some(
            PartitionInfo::new(
                PartitionType::Hash,
                "COL1 DIV 80",
                (0..6)
                    .map(|i| PartitionDefinition::new(100 + i, format!("p{i}"), vec![]))
                    .collect(),
            )
            .with_expr_key("COL1 DIV 80"),
        );
        let expr = ExprNode::InList(InListExpr {
            expr: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "col1".into(),
            })),
            list: vec![
                // Parser produces `UnaryMinus(128)` for negative literals.
                ExprNode::Unary(sealdb_parser::ast::expressions::UnaryOpExpr {
                    op: Op::UnaryMinus,
                    expr: Box::new(ExprNode::Value(ValueExpr {
                        datum: Datum::I64(128),
                    })),
                }),
                ExprNode::Value(ValueExpr {
                    datum: Datum::I64(107),
                }),
            ],
            not: false,
        });
        let planned = resolve_partitions(&info, Some(&expr), &[], PartitionPruneMode::Static);
        assert_eq!(planned.names, vec!["p1"]);
    }
}

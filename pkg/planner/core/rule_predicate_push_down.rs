/// predicate pushdown (TiDB `rule_predicate_push_down.go`).
/// CNF conjuncts without subqueries attach to TableReader / IndexLookUp /
/// IndexReader (coprocessor-filter analog). Join-local predicates push to each side;
/// outer-join inner-side filters stay residual. Completeness wave: TiKV
/// Coprocessor DAG / CopClient.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;

pub fn predicate_push_down(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    push_into(plan, filter.split_cnf(), is, current_db)
}

pub fn eq_column_consts(selection: Option<&ExprNode>) -> Vec<(String, Datum)> {
    selection
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.eq_column_const())
                .collect()
        })
        .unwrap_or_default()
}

fn push_into(
    plan: PhysicalPlan,
    conjuncts: Vec<ExprNode>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if conjuncts.is_empty() {
        return plan;
    }
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    // Newer conjuncts first (TiDB: pushed ON/like before view WHERE).
                    pushed: and_opt(ExprNode::compose_cnf(pushable), pushed),
                },
                residual,
            )
        }
        PhysicalPlan::PartitionUnion { children } => PhysicalPlan::PartitionUnion {
            children: children
                .into_iter()
                .map(|c| push_into(c, conjuncts.clone(), is, current_db))
                .collect(),
        },
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            row_count,
            selection_info,
            probe_selection_info,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    row_count,
                    selection_info,
                    probe_selection_info,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            row_count,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    row_count,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::IndexMerge {
            table,
            indexes,
            partitions,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexMerge {
                    table,
                    indexes,
                    partitions,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::PointGet {
            table,
            handle,
            index,
        } => {
            let residual = conjuncts
                .into_iter()
                .filter(|expr| handle.is_none_or(|value| !is_handle_eq(expr, value)))
                .collect();
            wrap_residual(
                PhysicalPlan::PointGet {
                    table,
                    handle,
                    index,
                },
                residual,
            )
        }
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
        } => {
            let residual = conjuncts
                .into_iter()
                .filter(|expr| !is_in_list_on(expr, &column))
                .collect();
            wrap_residual(
                PhysicalPlan::BatchPointGet {
                    table,
                    column,
                    index,
                    n_keys,
                },
                residual,
            )
        }
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::Distinct { keys, child } => PhysicalPlan::Distinct {
            keys,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left_cols = table_columns(is, current_db, &left_table);
            let right_cols = table_columns(is, current_db, &right_table);
            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut residual = Vec::new();
            for expr in conjuncts {
                match classify(
                    &expr,
                    &left_table,
                    &right_table,
                    &left_cols,
                    &right_cols,
                    tp,
                ) {
                    Side::Left => left_preds.push(expr),
                    Side::Right => right_preds.push(expr),
                    Side::Residual => residual.push(expr),
                }
            }
            wrap_residual(
                PhysicalPlan::HashJoin {
                    tp,
                    left: Box::new(push_into(*left, left_preds, is, current_db)),
                    right: Box::new(push_into(*right, right_preds, is, current_db)),
                    left_table,
                    right_table,
                    on,
                },
                residual,
            )
        }
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline,
            recursive,
        } if inline => {
            // Output `columns` (AS aliases / col_name_list) may differ from seed
            // internal names (`col0`, dual literals). Keep Selection above CTE.
            if columns.is_empty() {
                PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed: Box::new(push_into(*seed, conjuncts, is, current_db)),
                    recursive_plan,
                    inline,
                    recursive,
                }
            } else {
                wrap_residual(
                    PhysicalPlan::CTE {
                        name,
                        alias,
                        columns,
                        seed,
                        recursive_plan,
                        inline,
                        recursive,
                    },
                    conjuncts,
                )
            }
        }
        other => wrap_residual(other, conjuncts),
    }
}

fn wrap_residual(plan: PhysicalPlan, residual: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(residual) else {
        return plan;
    };
    if filter.has_subquery() {
        PhysicalPlan::Apply {
            child: Box::new(plan),
            filter,
            probe: None,
            info: None,
        }
    } else {
        PhysicalPlan::Selection {
            child: Box::new(plan),
            filter,
        }
    }
}

fn and_opt(left: Option<ExprNode>, right: Option<ExprNode>) -> Option<ExprNode> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) => ExprNode::compose_cnf(vec![left, right]),
    }
}

fn is_handle_eq(expr: &ExprNode, handle: i64) -> bool {
    expr.eq_column_const()
        .and_then(|(_, value)| datum_as_i64(&value))
        .is_some_and(|v| v == handle)
}

fn datum_as_i64(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => Some(*v as i64),
        Datum::F64(v) if v.fract() == 0.0 && *v >= i64::MIN as f64 && *v <= i64::MAX as f64 => {
            Some(*v as i64)
        }
        Datum::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn is_in_list_on(expr: &ExprNode, column: &str) -> bool {
    expr.column_in_consts()
        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(column))
}

enum Side {
    Left,
    Right,
    Residual,
}

fn classify(
    expr: &ExprNode,
    left_table: &str,
    right_table: &str,
    left_cols: &[String],
    right_cols: &[String],
    tp: JoinType,
) -> Side {
    if expr.has_subquery() {
        return Side::Residual;
    }
    let cols = expr.referenced_columns();
    if cols.is_empty() {
        return Side::Residual;
    }
    let mut left = true;
    let mut right = true;
    for (table, name) in cols {
        let on_left = column_on(table.as_deref(), &name, left_table, left_cols);
        let on_right = column_on(table.as_deref(), &name, right_table, right_cols);
        if !on_left {
            left = false;
        }
        if !on_right {
            right = false;
        }
        if !on_left && !on_right {
            return Side::Residual;
        }
    }
    match (left, right, tp) {
        (true, false, _) => Side::Left,
        (false, true, JoinType::Inner | JoinType::Cross | JoinType::Semi | JoinType::Anti) => {
            Side::Right
        }
        (false, true, JoinType::RightOuter) => Side::Right,
        (false, true, JoinType::LeftOuter) => Side::Residual,
        (true, true, _) => Side::Residual,
        (false, false, _) => Side::Residual,
    }
}

fn column_on(table: Option<&str>, name: &str, owner: &str, cols: &[String]) -> bool {
    if let Some(table) = table {
        return table.eq_ignore_ascii_case(owner);
    }
    if cols.is_empty() {
        return false;
    }
    cols.iter().any(|col| col.eq_ignore_ascii_case(name))
}

fn table_columns(is: Option<&InfoSchema>, current_db: &str, table: &str) -> Vec<String> {
    let Some(is) = is else {
        return Vec::new();
    };
    is.table_by_name(current_db, table)
        .or_else(|| {
            is.all_schemas()
                .into_iter()
                .find_map(|db| is.table_by_name(db.name.o(), table))
        })
        .map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};
    use sealdb_parser::opcode::Op;

    fn col_eq(table: &str, name: &str, value: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(table.to_string()),
                name: name.to_string(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(value),
            })),
        })
    }

    #[test]
    fn test_push_into_table_reader() {
        let plan = predicate_push_down(
            PhysicalPlan::table_reader("t", Vec::new()),
            &col_eq("t", "id", 1),
            None,
            "db1",
        );
        match plan {
            PhysicalPlan::TableReader {
                pushed: Some(filter),
                ..
            } => assert!(filter.restore_string().contains("id")),
            other => panic!("expected pushed TableReader, got {other:?}"),
        }
    }

    #[test]
    fn test_inner_join_splits_predicates() {
        let join = PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left: Box::new(PhysicalPlan::table_reader("t1", Vec::new())),
            right: Box::new(PhysicalPlan::table_reader("t2", Vec::new())),
            left_table: "t1".to_string(),
            right_table: "t2".to_string(),
            on: col_eq("t1", "id", 1),
        };
        let filter =
            ExprNode::compose_cnf(vec![col_eq("t1", "id", 1), col_eq("t2", "id", 2)]).expect("and");
        let plan = predicate_push_down(join, &filter, None, "db1");
        let PhysicalPlan::HashJoin { left, right, .. } = plan else {
            panic!("expected hash join, got {plan:?}");
        };
        assert!(matches!(
            *left,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
        assert!(matches!(
            *right,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
    }
}

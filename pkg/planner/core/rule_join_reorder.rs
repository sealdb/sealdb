use super::physical_plans::{JoinType, PhysicalPlan};

/// Two-table greedy JoinReorder: Inner HashJoin builds the smaller side.
/// Completeness wave: n-table DP/greedy, outer-join assoc, bushy trees.
#[allow(clippy::too_many_arguments)]
pub fn reorder_hash_join(
    tp: JoinType,
    left: PhysicalPlan,
    right: PhysicalPlan,
    left_table: String,
    right_table: String,
    left_rows: f64,
    right_rows: f64,
    on: sealdb_parser::ast::ExprNode,
) -> PhysicalPlan {
    let _ = super::plan_cost::hash_join_cost(left_rows, right_rows, left_rows, right_rows);
    let _ = hash_join_probe_build(tp, left_rows, right_rows);
    if matches!(tp, JoinType::Inner | JoinType::Cross) && left_rows + f64::EPSILON < right_rows {
        PhysicalPlan::HashJoin {
            tp,
            left: Box::new(right),
            right: Box::new(left),
            left_table: right_table,
            right_table: left_table,
            on,
        }
    } else {
        PhysicalPlan::HashJoin {
            tp,
            left: Box::new(left),
            right: Box::new(right),
            left_table,
            right_table,
            on,
        }
    }
}

fn hash_join_probe_build(tp: JoinType, left_rows: f64, right_rows: f64) -> (f64, f64) {
    if matches!(tp, JoinType::Inner | JoinType::Cross) && left_rows < right_rows {
        (right_rows, left_rows)
    } else {
        (left_rows, right_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inner_builds_smaller_side() {
        let left = PhysicalPlan::table_reader("small", Vec::new());
        let right = PhysicalPlan::table_reader("big", Vec::new());
        let plan = reorder_hash_join(
            JoinType::Inner,
            left,
            right,
            "small".to_string(),
            "big".to_string(),
            1.0,
            100.0,
            sealdb_parser::ast::ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
                datum: sealdb_types::Datum::I64(1),
            }),
        );
        let PhysicalPlan::HashJoin {
            left_table,
            right_table,
            ..
        } = plan
        else {
            panic!("expected hash join");
        };
        assert_eq!(left_table, "big");
        assert_eq!(right_table, "small");
    }
}

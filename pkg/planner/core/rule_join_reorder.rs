use super::physical_plans::{JoinType, PhysicalPlan};
use super::plan_cost_ver2::{hash_join_cost, table_scan_cost};

/// Two-table greedy JoinReorder: Inner HashJoin builds the cheaper side by
/// Cost Model v2 `hash_join_cost`. Semi/Anti keep the outer (left) as probe so
/// existence semantics stay left-only.
/// N-table Inner equality graphs use `rule_join_reorder_dp` (left/right-deep
/// DP). Completeness: full bushy subset DP, outer-join assoc.
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
    if !matches!(tp, JoinType::Inner | JoinType::Cross) {
        return PhysicalPlan::HashJoin {
            tp,
            left: Box::new(left),
            right: Box::new(right),
            left_table,
            right_table,
            on,
        };
    }
    let left_cost = table_scan_cost(left_rows);
    let right_cost = table_scan_cost(right_rows);
    // Cost of plan with `left` as probe and `right` as build.
    let keep_cost = hash_join_cost(left_rows, right_rows, left_cost, right_cost);
    // Cost after swapping so the original left builds (probe the larger side).
    let swap_cost = hash_join_cost(right_rows, left_rows, right_cost, left_cost);
    // Prefer the cheaper orientation; on a tie keep building the smaller side
    // on the right (TiDB-style greedy JoinReorder).
    let swap = if (swap_cost - keep_cost).abs() <= f64::EPSILON {
        left_rows + f64::EPSILON < right_rows
    } else {
        swap_cost + f64::EPSILON < keep_cost
    };
    if swap {
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
        // Probe the larger side, build the smaller (right after swap = small).
        assert_eq!(left_table, "big");
        assert_eq!(right_table, "small");
    }

    #[test]
    fn test_join_cost_keeps_cheaper_orientation() {
        let left = PhysicalPlan::table_reader("a", Vec::new());
        let right = PhysicalPlan::table_reader("b", Vec::new());
        let plan = reorder_hash_join(
            JoinType::Inner,
            left,
            right,
            "a".to_string(),
            "b".to_string(),
            50.0,
            50.0,
            sealdb_parser::ast::ExprNode::Value(sealdb_parser::ast::expressions::ValueExpr {
                datum: sealdb_types::Datum::I64(1),
            }),
        );
        let PhysicalPlan::HashJoin { left_table, .. } = plan else {
            panic!("expected hash join");
        };
        assert_eq!(left_table, "a");
    }
}

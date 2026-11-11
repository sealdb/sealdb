//! TiDB `AggregationPushDownSolver` subset: split a `GROUP BY` over an inner
//! join into a partial aggregation on one join side plus a final aggregation
//! (or a plain projection when the group keys already pin one row) above it.
//!
//! Only the decisions live here; `planbuilder::build_join_agg_plan` renders the
//! resulting nodes. The shape TiDB produces for
//!
//! ```sql
//! SELECT count(b.c2) FROM t1 a, t2 b WHERE a.c1 = b.c2 GROUP BY a.c1;
//! ```
//!
//! is `Projection → HashJoin(HashAgg(Build) ⋈ …)` because
//!
//! * `count(b.c2)` decomposes into `count` over the build side, and
//! * `a.c1` is the primary key of the probe side, so each join row is its own
//!   group and the outer aggregation collapses to a projection.
//!
//! The solver must not fire when the aggregation's arguments straddle both
//! join sides (nothing can be pushed) or when the aggregate cannot be split
//! (`count(distinct …)`, ordered aggregates).

use sealdb_meta::TableInfo;
use sealdb_parser::ast::expressions::ExprNode;

/// Which join side an aggregation can be pushed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggSide {
    Left,
    Right,
    /// Arguments reference columns of both sides: not decomposable.
    Both,
    /// No column references (`count(1)`): TiDB pushes it to the build side.
    Neither,
}

/// TiDB `aggregation.IsAggPushDownEnabled` + `AggFuncDesc.CanUse3Stage` subset:
/// the aggregate has a partial form that TiKV can compute (`Partial1Mode`) and a
/// final form that consumes it (`FinalMode`).
pub fn is_decomposable(name: &str, distinct: bool, order_by_items: usize) -> bool {
    !distinct
        && order_by_items == 0
        && matches!(
            name.to_ascii_lowercase().as_str(),
            "count" | "sum" | "min" | "max" | "bit_and" | "bit_or" | "bit_xor"
        )
}

/// Side of the join an expression's columns come from.
///
/// `left` / `right` are the table names (aliases included) each side owns. An
/// expression with no column reference (a constant or `count(1)`) maps to
/// [`AggSide::Neither`], which TiDB assigns to the build side.
pub fn expr_side(expr: &ExprNode, left: &[String], right: &[String]) -> AggSide {
    let mut seen_left = false;
    let mut seen_right = false;
    for (qualifier, _name) in expr.referenced_columns() {
        let matches = |names: &[String]| {
            qualifier.as_deref().is_some_and(|q| {
                names.iter().any(|n| n.eq_ignore_ascii_case(q))
            })
        };
        if matches(left) {
            seen_left = true;
        } else if matches(right) {
            seen_right = true;
        }
    }
    match (seen_left, seen_right) {
        (true, true) => AggSide::Both,
        (true, false) => AggSide::Left,
        (false, true) => AggSide::Right,
        (false, false) => AggSide::Neither,
    }
}

/// TiDB `tryToEliminateAggregation`: the group keys contain every column of a
/// unique key of the table, so each group holds at most one row and the
/// aggregation degenerates to a projection of its arguments.
///
/// `columns` are bare column names from the `GROUP BY` list.
pub fn covers_unique_key(info: &TableInfo, columns: &[String]) -> bool {
    info.indices.iter().any(|idx| {
        idx.unique
            && !idx.columns.is_empty()
            && idx
                .columns
                .iter()
                .all(|c| columns.iter().any(|col| col.eq_ignore_ascii_case(c.name.o())))
    })
}

/// `EXPLAIN` name of the final aggregation for a partial output column.
///
/// TiDB keeps the original function name in both phases; the partial column is
/// the argument of the final call.
pub fn final_func(func: &str, partial_col: usize, out_col: usize) -> String {
    format!("{}(Column#{partial_col})->Column#{out_col}", func.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_meta::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
    use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode};
    use sealdb_types::FieldType;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn qualified(table: Option<&str>, name: &str) -> ExprNode {
        ExprNode::Column(ColumnNameExpr {
            schema: None,
            table: table.map(|t| t.to_string()),
            name: name.to_string(),
        })
    }

    #[test]
    fn decomposable_aggregates_match_tidb() {
        assert!(is_decomposable("count", false, 0));
        assert!(is_decomposable("SUM", false, 0));
        assert!(is_decomposable("max", false, 0));
        assert!(!is_decomposable("count", true, 0));
        assert!(!is_decomposable("sum", false, 1));
        assert!(!is_decomposable("group_concat", false, 0));
    }

    #[test]
    fn expr_side_tracks_qualifiers() {
        let left = names(&["a"]);
        let right = names(&["b"]);
        assert_eq!(
            expr_side(&qualified(None, "c1"), &left, &right),
            AggSide::Neither
        );
        assert_eq!(
            expr_side(&qualified(Some("a"), "c1"), &left, &right),
            AggSide::Left
        );
        assert_eq!(
            expr_side(&qualified(Some("b"), "c1"), &left, &right),
            AggSide::Right
        );
    }

    fn table_with_pk(column_name: &str) -> TableInfo {
        let mut info = TableInfo::new(
            1,
            "t1",
            vec![ColumnInfo::new(
                1,
                0,
                column_name,
                FieldType::new_longlong(),
            )],
        );
        info.indices.push(
            IndexInfo::new(
                1,
                "PRIMARY",
                "t1",
                vec![IndexColumn {
                    name: sealdb_parser::model::CIStr::new(column_name),
                    offset: 0,
                    length: -1,
                }],
            )
            .with_flags(true, true),
        );
        info
    }

    #[test]
    fn unique_key_coverage_needs_every_key_column() {
        let info = table_with_pk("c1");
        assert!(covers_unique_key(&info, &names(&["c1"])));
        assert!(covers_unique_key(&info, &names(&["c1", "c2"])));
        assert!(!covers_unique_key(&info, &names(&["c2"])));
    }

    #[test]
    fn final_func_names_the_partial_column() {
        assert_eq!(final_func("COUNT", 8, 7), "count(Column#8)->Column#7");
    }
}

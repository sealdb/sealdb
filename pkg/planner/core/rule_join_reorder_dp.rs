//! Left-deep / right-deep n-table JoinReorder DP (TiDB `rule_join_reorder_dp`).
//!
//! For 3+ Inner equality-connected tables, bitmask DP picks a lower-cost join
//! order using `hash_join_cost` / `table_scan_cost`. Each extension may probe
//! the new leaf and build the accumulated plan (right-deep) when cheaper —
//! bushy-lite without full subset splits.

use super::plan_cost::{hash_join_cost, table_scan_cost};
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_parser::opcode::Op;

const MAX_DP_TABLES: usize = 8;

#[derive(Clone, Debug)]
pub struct JoinEqEdge {
    pub left: usize,
    pub right: usize,
    pub on: ExprNode,
}

/// One join step when materializing the DP result.
#[derive(Clone, Debug, PartialEq)]
pub struct DpJoinStep {
    /// Table attached at this step (singleton leaf).
    pub table: usize,
    /// When true, the new table probes and the previous plan builds.
    pub swap: bool,
}

#[derive(Clone, Debug)]
struct DpNode {
    cost: f64,
    rows: f64,
    /// Table attached last.
    last: usize,
    /// Predecessor subset (`0` for singletons).
    prev: u32,
    /// Orientation vs predecessor (`false` for singletons).
    swap: bool,
}

/// Equality edge between two table-qualified columns, else `None`.
pub fn eq_join_edge(on: &ExprNode, names: &[String]) -> Option<(usize, usize)> {
    let ExprNode::Binary(bin) = on else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    let left = column_table_index(&bin.left, names)?;
    let right = column_table_index(&bin.right, names)?;
    if left == right {
        return None;
    }
    Some((left, right))
}

fn column_table_index(expr: &ExprNode, names: &[String]) -> Option<usize> {
    let ExprNode::Column(col) = expr else {
        return None;
    };
    let table = col.table.as_deref()?;
    names.iter().position(|n| n.eq_ignore_ascii_case(table))
}

/// DP join steps in attachment order (first entry is the seed table with
/// `swap == false`). `None` when DP does not apply.
pub fn dp_join_steps(rows: &[f64], edges: &[JoinEqEdge]) -> Option<Vec<DpJoinStep>> {
    let n = rows.len();
    if !(3..=MAX_DP_TABLES).contains(&n) || edges.is_empty() {
        return None;
    }
    let full = (1u32 << n) - 1;
    let mut adj = vec![0u32; n];
    for e in edges {
        adj[e.left] |= 1 << e.right;
        adj[e.right] |= 1 << e.left;
    }
    if !connected(n, &adj) {
        return None;
    }

    let mut best: Vec<Option<DpNode>> = vec![None; 1 << n];
    for i in 0..n {
        best[1 << i] = Some(DpNode {
            cost: table_scan_cost(rows[i]),
            rows: rows[i],
            last: i,
            prev: 0,
            swap: false,
        });
    }

    for mask in 1u32..=full {
        let Some(node) = best[mask as usize].clone() else {
            continue;
        };
        let mut nbrs = 0u32;
        for (i, links) in adj.iter().enumerate().take(n) {
            if mask & (1 << i) != 0 {
                nbrs |= links;
            }
        }
        nbrs &= !mask;
        let mut rest = nbrs;
        while rest != 0 {
            let bit = rest.isolate_lowest_one();
            rest ^= bit;
            let j = bit.trailing_zeros() as usize;
            let new_mask = mask | bit;
            let build_scan = table_scan_cost(rows[j]);
            let keep = hash_join_cost(node.rows, rows[j], node.cost, build_scan);
            let swap = hash_join_cost(rows[j], node.rows, build_scan, node.cost);
            let (cost, do_swap) = if swap + f64::EPSILON < keep {
                (swap, true)
            } else {
                (keep, false)
            };
            let out_rows = node.rows.min(rows[j]).max(1.0);
            let cand = DpNode {
                cost,
                rows: out_rows,
                last: j,
                prev: mask,
                swap: do_swap,
            };
            match &best[new_mask as usize] {
                Some(old) if old.cost <= cand.cost + f64::EPSILON => {}
                _ => best[new_mask as usize] = Some(cand),
            }
        }
    }

    let mut steps = Vec::with_capacity(n);
    let mut cur = best[full as usize].as_ref()?;
    loop {
        steps.push(DpJoinStep {
            table: cur.last,
            swap: cur.swap,
        });
        if cur.prev == 0 {
            break;
        }
        cur = best[cur.prev as usize].as_ref()?;
    }
    steps.reverse();
    // Seed step never swaps.
    if let Some(first) = steps.first_mut() {
        first.swap = false;
    }
    if steps.len() != n {
        return None;
    }
    Some(steps)
}

/// ON predicate linking `new_table` to some table in `used`, if any.
pub fn edge_for_extension<'a>(
    edges: &'a [JoinEqEdge],
    used: &[usize],
    new_table: usize,
) -> Option<&'a JoinEqEdge> {
    edges.iter().find(|e| {
        let (a, b) = (e.left, e.right);
        (a == new_table && used.contains(&b)) || (b == new_table && used.contains(&a))
    })
}

fn connected(n: usize, adj: &[u32]) -> bool {
    if n == 0 {
        return true;
    }
    let mut seen = 1u32;
    let mut stack = vec![0usize];
    while let Some(i) = stack.pop() {
        let mut bits = adj[i];
        while bits != 0 {
            let bit = bits.isolate_lowest_one();
            bits ^= bit;
            let j = bit.trailing_zeros() as usize;
            if seen & (1 << j) == 0 {
                seen |= 1 << j;
                stack.push(j);
            }
        }
    }
    seen == (1u32 << n) - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};

    fn col(table: &str, name: &str) -> ExprNode {
        ExprNode::Column(ColumnNameExpr {
            schema: None,
            table: Some(table.to_string()),
            name: name.to_string(),
        })
    }

    fn eq(ltable: &str, rtable: &str) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(col(ltable, "id")),
            right: Box::new(col(rtable, "id")),
        })
    }

    #[test]
    fn test_dp_chain_builds_small_subgraph_first() {
        // SQL order big(100), mid(10), small(1); edges big-mid, mid-small.
        // Cheapest plans join mid+small first, then let big probe that tree.
        let rows = [100.0, 10.0, 1.0];
        let edges = vec![
            JoinEqEdge {
                left: 0,
                right: 1,
                on: eq("big", "mid"),
            },
            JoinEqEdge {
                left: 1,
                right: 2,
                on: eq("mid", "small"),
            },
        ];
        let steps = dp_join_steps(&rows, &edges).expect("dp steps");
        let tables: Vec<_> = steps.iter().map(|s| s.table).collect();
        let big_pos = tables.iter().position(|&t| t == 0).expect("{tables:?}");
        let small_pos = tables.iter().position(|&t| t == 2).expect("{tables:?}");
        assert!(small_pos < big_pos, "{steps:?}");
        assert_eq!(steps.last().map(|s| s.table), Some(0));
        assert!(steps.last().is_some_and(|s| s.swap), "{steps:?}");
    }

    #[test]
    fn test_dp_skips_two_tables() {
        let rows = [10.0, 1.0];
        let edges = vec![JoinEqEdge {
            left: 0,
            right: 1,
            on: eq("a", "b"),
        }];
        assert!(dp_join_steps(&rows, &edges).is_none());
    }

    #[test]
    fn test_eq_join_edge_parses() {
        let names = vec!["t1".into(), "t2".into()];
        assert_eq!(eq_join_edge(&eq("t1", "t2"), &names), Some((0, 1)));
        assert_eq!(
            eq_join_edge(
                &ExprNode::Value(ValueExpr {
                    datum: sealdb_types::Datum::I64(1),
                }),
                &names
            ),
            None
        );
    }
}

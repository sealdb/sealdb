/// partition prune modes (TiDB `tidb_partition_prune_mode`).
/// Static prunes at plan time; dynamic defers to execute using the pushed filter.
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ExprNode;

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

    let pruned = info.prune_partitions(selection);
    let mut defs: Vec<_> = part
        .definitions
        .iter()
        .filter(|d| pruned.contains(&d.id))
        .collect();
    if !named.is_empty() {
        defs.retain(|d| {
            named
                .iter()
                .any(|n| d.name.o().eq_ignore_ascii_case(n))
        });
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
    info.prune_partitions(selection)
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
}

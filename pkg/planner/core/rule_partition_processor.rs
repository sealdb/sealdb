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
            "dynamic" | "dynamic-only" => Some(Self::Dynamic),
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

/// Plan-time partition list. Dynamic returns empty so the executor prunes
/// with the pushed predicate (TiDB execute-phase prune).
pub fn plan_partitions(
    info: &TableInfo,
    selection: Option<&ExprNode>,
    mode: PartitionPruneMode,
) -> Vec<i64> {
    match mode {
        PartitionPruneMode::Static => info.prune_partitions(selection),
        PartitionPruneMode::Dynamic => Vec::new(),
    }
}

pub fn prune_partitions(info: &TableInfo, selection: Option<&ExprNode>) -> Vec<i64> {
    info.prune_partitions(selection)
}

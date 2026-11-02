/// F19 core: static partition prune (TiDB `rule_partition_processor.go`).
/// Completeness wave / not this feature: dynamic prune, RANGE COLUMNS,
/// partition names in FROM.
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ExprNode;

pub fn prune_partitions(info: &TableInfo, selection: Option<&ExprNode>) -> Vec<i64> {
    info.prune_partitions(selection)
}

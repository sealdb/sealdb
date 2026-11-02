use sealdb_parser::ast::TableName;
use sealdb_statistics::{Handle, Table};

pub const SCAN_FACTOR: f64 = 1.0;
pub const NET_FACTOR: f64 = 1.5;
pub const CPU_FACTOR: f64 = 0.5;
pub const HASH_FACTOR: f64 = 1.0;
pub const SORT_FACTOR: f64 = 2.0;

pub fn table_row_count(stats: Option<&Handle>, from: &TableName, current_db: &str) -> f64 {
    let (db, table) = from.qualified(current_db);
    stats
        .and_then(|handle| handle.table(&db, &table))
        .map(|t| t.count as f64)
        .unwrap_or(Table::PSEUDO_ROW_COUNT as f64)
}

pub fn eq_row_count(
    stats: Option<&Handle>,
    from: &TableName,
    current_db: &str,
    column: &str,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    stats
        .and_then(|handle| handle.table(&db, &table))
        .map(|t| t.eq_row_count(column) as f64)
}

/// Equality selectivity using TopN when the constant is known.
pub fn eq_row_count_value(
    stats: Option<&Handle>,
    from: &TableName,
    current_db: &str,
    column: &str,
    value: &sealdb_types::Datum,
) -> Option<f64> {
    let (db, table) = from.qualified(current_db);
    stats
        .and_then(|handle| handle.table(&db, &table))
        .map(|t| t.eq_row_count_value(column, value) as f64)
}

pub fn table_scan_cost(rows: f64) -> f64 {
    rows * SCAN_FACTOR
}

pub fn index_lookup_cost(rows: f64) -> f64 {
    rows * (SCAN_FACTOR + NET_FACTOR)
}

pub fn point_get_cost() -> f64 {
    1.0
}

/// TiDB-style HashJoin cost: `left` probes, `right` builds.
/// Building is charged higher than probing so the cheaper plan builds the
/// smaller side.
pub fn hash_join_cost(probe_rows: f64, build_rows: f64, probe_cost: f64, build_cost: f64) -> f64 {
    probe_cost + build_cost + probe_rows * HASH_FACTOR + build_rows * HASH_FACTOR * 2.0 + CPU_FACTOR
}

/// Full-sort cost ~ N log N.
pub fn sort_cost(rows: f64) -> f64 {
    if rows <= 1.0 {
        return rows * CPU_FACTOR;
    }
    rows * rows.log2().max(1.0) * SORT_FACTOR * CPU_FACTOR
}

/// Limit/offset streaming cost.
pub fn limit_cost(rows: f64, limit: u64) -> f64 {
    rows.min(limit as f64) * CPU_FACTOR
}

/// TopN heap cost ~ N log K (prefer over Sort+Limit when K is small).
pub fn topn_cost(rows: f64, limit: u64) -> f64 {
    let k = (limit as f64).max(1.0);
    if rows <= 1.0 {
        return rows * CPU_FACTOR;
    }
    rows * k.log2().max(1.0) * SORT_FACTOR * CPU_FACTOR
}

/// True when TopN is cheaper than Sort followed by Limit.
pub fn prefer_topn(rows: f64, limit: u64) -> bool {
    topn_cost(rows, limit) <= sort_cost(rows) + limit_cost(rows, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_cost_prefers_cheaper_build() {
        let small = 1.0;
        let big = 100.0;
        let small_cost = table_scan_cost(small);
        let big_cost = table_scan_cost(big);
        // probe big, build small
        let build_small = hash_join_cost(big, small, big_cost, small_cost);
        // probe small, build big
        let build_big = hash_join_cost(small, big, small_cost, big_cost);
        assert!(build_small < build_big);
    }

    #[test]
    fn test_topn_beats_sort_for_small_limit() {
        assert!(prefer_topn(10_000.0, 5));
        assert!(!prefer_topn(10.0, 10_000));
    }
}

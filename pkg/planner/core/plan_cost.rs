use sealdb_parser::ast::TableName;
use sealdb_statistics::{Handle, Table};

pub const SCAN_FACTOR: f64 = 1.0;
pub const NET_FACTOR: f64 = 1.5;
pub const CPU_FACTOR: f64 = 0.5;
pub const HASH_FACTOR: f64 = 1.0;

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

pub fn table_scan_cost(rows: f64) -> f64 {
    rows * SCAN_FACTOR
}

pub fn index_lookup_cost(rows: f64) -> f64 {
    rows * (SCAN_FACTOR + NET_FACTOR)
}

pub fn point_get_cost() -> f64 {
    1.0
}

pub fn hash_join_cost(left_rows: f64, right_rows: f64, left_cost: f64, right_cost: f64) -> f64 {
    left_cost + right_cost + (left_rows + right_rows) * HASH_FACTOR + CPU_FACTOR
}

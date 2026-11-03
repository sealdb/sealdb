//! DistSQL Select: build tipb TableScan CopTasks for Region-aligned ranges.
//!
//! Sending to TiKV Coprocessor RPC is driver-owned (`sealdb-store`); this
//! module only builds task payloads matching TiDB DistSQL.

use sealdb_errno::Result;
use sealdb_store::{
    build_table_scan_tasks, ColumnInfo, CopTask, Selection,
};

/// Build CopTasks for a table-range DistSQL Select (TableScan DAG).
pub fn build_table_select_tasks(
    pd_endpoints: &[String],
    start_ts: u64,
    table_id: i64,
    columns: &[ColumnInfo],
    output_offsets: &[i64],
    start_key: &[u8],
    end_key: &[u8],
    selection: Option<Selection>,
    limit: Option<u64>,
) -> Result<Vec<CopTask>> {
    build_table_scan_tasks(
        pd_endpoints,
        start_ts,
        table_id,
        columns,
        output_offsets,
        start_key,
        end_key,
        selection,
        limit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_table_select_tasks() {
        let cols = [ColumnInfo {
            column_id: 1,
            tp: 8,
            pk_handle: true,
        }];
        let tasks = build_table_select_tasks(
            &[],
            7,
            99,
            &cols,
            &[0],
            &[0x74],
            &[0x75],
            None,
            Some(5),
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].start_ts, 7);
        assert_eq!(tasks[0].tp, sealdb_store::REQ_TYPE_DAG);
    }
}

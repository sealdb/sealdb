//! Cop task: Region-aligned ranges + tipb DAG payload (TiDB DistSQL / copr).

use super::dag::{ColumnInfo, REQ_TYPE_DAG, Selection, encode_table_scan_dag};
use super::{KeyRange, split_key_ranges, split_key_ranges_via_pd};
use sealdb_errno::Result;

/// One Coprocessor unit of work (ranges + encoded tipb DAG).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopTask {
    pub ranges: Vec<KeyRange>,
    /// Encoded tipb.DAGRequest.
    pub data: Vec<u8>,
    /// Coprocessor request type (`REQ_TYPE_DAG`).
    pub tp: i64,
    pub start_ts: u64,
}

impl CopTask {
    pub fn dag(ranges: Vec<KeyRange>, data: Vec<u8>, start_ts: u64) -> Self {
        Self {
            ranges,
            data,
            tp: REQ_TYPE_DAG,
            start_ts,
        }
    }
}

/// Build Region-split Cop tasks for a TableScan DAG over `[start, end)`.
///
/// When `pd_endpoints` is empty, emits a single task covering the full span.
#[allow(clippy::too_many_arguments)]
pub fn build_table_scan_tasks(
    pd_endpoints: &[String],
    start_ts: u64,
    table_id: i64,
    columns: &[ColumnInfo],
    output_offsets: &[i64],
    start: &[u8],
    end: &[u8],
    selection: Option<Selection>,
    limit: Option<u64>,
) -> Result<Vec<CopTask>> {
    let ranges = if pd_endpoints.is_empty() {
        split_key_ranges(start, end)
    } else {
        split_key_ranges_via_pd(pd_endpoints, start, end)
            .unwrap_or_else(|_| split_key_ranges(start, end))
    };
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let data = encode_table_scan_dag(
        start_ts,
        table_id,
        columns,
        output_offsets,
        selection,
        limit,
    );
    Ok(ranges
        .into_iter()
        .map(|range| CopTask::dag(vec![range], data.clone(), start_ts))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_table_scan_tasks_mem() {
        let cols = [ColumnInfo {
            column_id: 1,
            tp: 8,
            pk_handle: true,
        }];
        let tasks =
            build_table_scan_tasks(&[], 1, 10, &cols, &[0], b"t", b"u", None, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].tp, REQ_TYPE_DAG);
        assert_eq!(tasks[0].ranges[0].start, b"t");
        assert!(!tasks[0].data.is_empty());
    }
}

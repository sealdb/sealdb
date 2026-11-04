//! TiDB `pkg/store/copr/store.go` — Cop store handle over PD endpoints.

use super::rpc::execute_cop_task;
use super::task::{CopTask, build_table_scan_tasks};
use super::{ColumnInfo, SelectResponse, Selection};
use sealdb_errno::Result;

/// Lightweight Cop store: builds tasks and sends tipb DAG RPCs.
#[derive(Clone, Debug, Default)]
pub struct CopStore {
    pub pd_endpoints: Vec<String>,
}

impl CopStore {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self { pd_endpoints }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_table_scan_tasks(
        &self,
        start_ts: u64,
        table_id: i64,
        columns: &[ColumnInfo],
        output_offsets: &[i64],
        start: &[u8],
        end: &[u8],
        selection: Option<Selection>,
        limit: Option<u64>,
    ) -> Result<Vec<CopTask>> {
        build_table_scan_tasks(
            &self.pd_endpoints,
            start_ts,
            table_id,
            columns,
            output_offsets,
            start,
            end,
            selection,
            limit,
        )
    }

    pub async fn execute(&self, task: &CopTask) -> Result<SelectResponse> {
        execute_cop_task(&self.pd_endpoints, task).await
    }
}

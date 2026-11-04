//! TiDB `pkg/store/copr/mpp.go` — MPP coordination over TiFlash stores.
//!
//! Store discovery, BatchCop planning, plus DispatchMPPTask /
//! EstablishMPPConnection root-gather MVB (`mpp_exchange`).

use super::KeyRange;
use super::batch_coprocessor::{
    BatchCopTask, BatchStorePrefer, balance_batch_cop_tasks, build_batch_cop_tasks,
};
use super::dag::SelectResponse;
use super::mpp_exchange::{
    EstablishMppConnectionRequest, TaskMeta, build_dispatch_request, dispatch_mpp_task,
    establish_mpp_connection, execute_mpp_root_gather, task_meta_for,
};
use super::mpp_probe::MppProbe;
use super::pd::PdStoreInfo;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_GATHER_ID: AtomicU64 = AtomicU64::new(1);

/// Planned MPP / BatchCop dispatch (no network send yet for TiFlash stream).
#[derive(Clone, Debug, Default)]
pub struct MppDispatchPlan {
    pub tiflash_store_count: usize,
    pub tasks: Vec<BatchCopTask>,
}

/// Minimal MPP client: TiFlash discovery + BatchCop task dispatch planning.
#[derive(Clone, Debug, Default)]
pub struct MppClient {
    probe: MppProbe,
}

impl MppClient {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            probe: MppProbe::new(pd_endpoints),
        }
    }

    pub fn pd_endpoints(&self) -> &[String] {
        &self.probe.pd_endpoints
    }

    pub fn tiflash_stores(&self) -> Result<Vec<PdStoreInfo>> {
        self.probe.list_tiflash_stores()
    }

    pub fn require_tiflash(&self) -> Result<Vec<PdStoreInfo>> {
        let stores = self.tiflash_stores()?;
        if stores.is_empty() {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                "no TiFlash store with label engine=tiflash",
            ));
        }
        Ok(stores)
    }

    pub fn available(&self) -> bool {
        self.probe.tiflash_available()
    }

    /// Build balanced BatchCop tasks assigned to TiFlash stores (plan only).
    ///
    /// Use [`dispatch_stream`] / [`BatchRequestSender`] to open BatchCoprocessor.
    pub fn plan_dispatch(
        &self,
        ranges: &[KeyRange],
        data: Vec<u8>,
        start_ts: u64,
    ) -> Result<MppDispatchPlan> {
        let stores = self.require_tiflash()?;
        let raw = build_batch_cop_tasks(
            &self.probe.pd_endpoints,
            BatchStorePrefer::TiFlash,
            &stores,
            ranges,
            data,
            start_ts,
        )?;
        let tasks = balance_batch_cop_tasks(&stores, raw, true);
        Ok(MppDispatchPlan {
            tiflash_store_count: stores.len(),
            tasks,
        })
    }

    /// Plan TiFlash BatchCop tasks and execute via BatchCoprocessor stream.
    pub fn dispatch_stream_blocking(
        &self,
        ranges: &[KeyRange],
        data: Vec<u8>,
        start_ts: u64,
    ) -> Result<SelectResponse> {
        let plan = self.plan_dispatch(ranges, data, start_ts)?;
        super::batch_request_sender::BatchRequestSender::new().execute_blocking(&plan.tasks)
    }

    /// Plan TiKV-leader BatchCop grouping (no TiFlash required).
    pub fn plan_tikv_batch(
        &self,
        ranges: &[KeyRange],
        data: Vec<u8>,
        start_ts: u64,
    ) -> Result<Vec<BatchCopTask>> {
        build_batch_cop_tasks(
            &self.probe.pd_endpoints,
            BatchStorePrefer::TiKvLeader,
            &[],
            ranges,
            data,
            start_ts,
        )
    }

    /// Allocate a gather id for one MPP query.
    pub fn next_gather_id() -> u64 {
        NEXT_GATHER_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// DispatchMPPTask for every planned TiFlash BatchCop fragment.
    pub async fn dispatch_mpp_tasks(
        &self,
        tasks: &[BatchCopTask],
        gather_id: u64,
        schema_ver: i64,
    ) -> Result<Vec<TaskMeta>> {
        let mut metas = Vec::with_capacity(tasks.len());
        for (i, task) in tasks.iter().enumerate() {
            let task_id = (i + 1) as i64;
            let req = build_dispatch_request(task, task_id, gather_id, schema_ver);
            let _ = dispatch_mpp_task(&task.store_addr, req).await?;
            metas.push(task_meta_for(task, task_id, gather_id));
        }
        Ok(metas)
    }

    /// EstablishMPPConnection root gather (receiver task_id = -1).
    pub async fn establish_root_connection(
        &self,
        store_addr: &str,
        sender: TaskMeta,
        gather_id: u64,
    ) -> Result<SelectResponse> {
        let receiver = TaskMeta {
            start_ts: sender.start_ts,
            task_id: -1,
            address: String::new(),
            gather_id,
            query_ts: sender.query_ts,
            local_query_id: sender.local_query_id,
            server_id: sender.server_id,
            mpp_version: sender.mpp_version,
        };
        establish_mpp_connection(
            store_addr,
            EstablishMppConnectionRequest {
                sender_meta: Some(sender),
                receiver_meta: Some(receiver),
            },
        )
        .await
    }

    /// Full root-gather path: plan → DispatchMPPTask → EstablishMPPConnection.
    pub fn execute_mpp_gather_blocking(
        &self,
        ranges: &[KeyRange],
        data: Vec<u8>,
        start_ts: u64,
        schema_ver: i64,
    ) -> Result<SelectResponse> {
        let plan = self.plan_dispatch(ranges, data, start_ts)?;
        let gather_id = Self::next_gather_id();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tokio runtime: {e}")))?;
        rt.block_on(execute_mpp_root_gather(&plan.tasks, gather_id, schema_ver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_dispatch_requires_tiflash() {
        let client = MppClient::new(vec!["127.0.0.1:2379".to_string()]);
        match client.plan_dispatch(&[KeyRange::new(b"t".to_vec(), b"u".to_vec())], vec![1], 1) {
            Ok(plan) => {
                assert!(plan.tiflash_store_count > 0);
                assert!(plan.tasks.iter().all(|t| t.is_tiflash));
            }
            Err(err) => {
                // Common when cluster has no TiFlash — still a valid probe path.
                let msg = err.to_string();
                assert!(
                    msg.contains("TiFlash") || msg.contains("pd"),
                    "unexpected err: {msg}"
                );
            }
        }
    }

    #[test]
    fn test_plan_tikv_batch_if_pd_available() {
        let client = MppClient::new(vec!["127.0.0.1:2379".to_string()]);
        match client.plan_tikv_batch(&[KeyRange::new(b"t".to_vec(), b"u".to_vec())], vec![2], 3) {
            Ok(tasks) => {
                assert!(!tasks.is_empty());
                assert!(tasks.iter().all(|t| !t.is_tiflash));
            }
            Err(err) => eprintln!("skip plan_tikv_batch: {err}"),
        }
    }

    #[test]
    fn test_execute_mpp_gather_without_tiflash_errors() {
        let client = MppClient::new(vec!["127.0.0.1:2379".to_string()]);
        let err = client
            .execute_mpp_gather_blocking(
                &[KeyRange::new(b"t".to_vec(), b"u".to_vec())],
                vec![1],
                1,
                1,
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("TiFlash") || msg.contains("pd") || msg.contains("tiflash"),
            "unexpected: {msg}"
        );
    }
}

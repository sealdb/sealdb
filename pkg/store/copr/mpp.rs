//! TiDB `pkg/store/copr/mpp.go` — MPP coordination over TiFlash stores.
//!
//! Store discovery + BatchCop task planning / balance. Full MPP exchange /
//! DispatchMPPTask gRPC remains deferred.

use super::batch_coprocessor::{
    balance_batch_cop_tasks, build_batch_cop_tasks, BatchCopTask, BatchStorePrefer,
};
use super::mpp_probe::MppProbe;
use super::pd::PdStoreInfo;
use super::KeyRange;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};

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
    ) -> Result<super::dag::SelectResponse> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_dispatch_requires_tiflash() {
        let client = MppClient::new(vec!["127.0.0.1:2379".to_string()]);
        match client.plan_dispatch(
            &[KeyRange::new(b"t".to_vec(), b"u".to_vec())],
            vec![1],
            1,
        ) {
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
        match client.plan_tikv_batch(
            &[KeyRange::new(b"t".to_vec(), b"u".to_vec())],
            vec![2],
            3,
        ) {
            Ok(tasks) => {
                assert!(!tasks.is_empty());
                assert!(tasks.iter().all(|t| !t.is_tiflash));
            }
            Err(err) => eprintln!("skip plan_tikv_batch: {err}"),
        }
    }
}

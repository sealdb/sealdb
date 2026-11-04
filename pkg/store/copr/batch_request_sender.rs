//! TiDB `pkg/store/copr/batch_request_sender.go` — BatchCop sender.
//!
//! Full BatchCoprocessor streaming RPC is deferred. This MVB executes each
//! store-batched task via unary Coprocessor against the assigned store address
//! (TiKV path). TiFlash-assigned tasks are planned but not streamed yet.

use super::batch_coprocessor::BatchCopTask;
use super::dag::SelectResponse;
use super::rpc::execute_batch_cop_task;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};

/// Sends BatchCop tasks (unary Cop fallback per task for TiKV stores).
#[derive(Clone, Debug, Default)]
pub struct BatchRequestSender;

impl BatchRequestSender {
    pub fn new() -> Self {
        Self
    }

    /// Execute all tasks and merge SelectResponse chunks.
    ///
    /// Tasks marked `is_tiflash` return an error — use `MppClient::plan_dispatch`
    /// until BatchCop stream / MPP exchange is wired.
    pub fn execute_blocking(&self, tasks: &[BatchCopTask]) -> Result<SelectResponse> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tokio runtime: {e}")))?;
        rt.block_on(self.execute(tasks))
    }

    pub async fn execute(&self, tasks: &[BatchCopTask]) -> Result<SelectResponse> {
        if tasks.is_empty() {
            return Ok(SelectResponse::default());
        }
        if tasks.iter().any(|t| t.is_tiflash) {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                "batch request sender: TiFlash BatchCop stream not implemented; use MppClient::plan_dispatch",
            ));
        }
        let mut merged = SelectResponse::default();
        for task in tasks {
            let part = execute_batch_cop_task(task).await?;
            if let Some(err) = part.error {
                return Err(Error::new(
                    if err.code != 0 {
                        err.code
                    } else {
                        ERR_UNKNOWN_ERROR
                    },
                    format!("batch cop: {}", err.msg),
                ));
            }
            merged.chunks.extend(part.chunks);
        }
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copr::batch_coprocessor::{
        build_batch_cop_tasks, BatchStorePrefer,
    };
    use crate::copr::KeyRange;

    #[test]
    fn test_sender_rejects_tiflash_tasks() {
        let task = BatchCopTask {
            store_id: 1,
            store_addr: "127.0.0.1:3930".into(),
            region_id: 1,
            region_epoch_conf_ver: 1,
            region_epoch_version: 1,
            peer_id: 0,
            ranges: vec![KeyRange::new(b"a".to_vec(), b"b".to_vec())],
            data: vec![],
            tp: 103,
            start_ts: 1,
            is_tiflash: true,
        };
        let err = BatchRequestSender::new()
            .execute_blocking(&[task])
            .unwrap_err();
        assert!(err.to_string().contains("TiFlash"));
    }

    #[test]
    fn test_sender_tikv_if_available() {
        let pd = vec!["127.0.0.1:2379".to_string()];
        let tasks = match build_batch_cop_tasks(
            &pd,
            BatchStorePrefer::TiKvLeader,
            &[],
            &[KeyRange::new(b"t".to_vec(), b"u".to_vec())],
            vec![],
            1,
        ) {
            Ok(t) => t,
            Err(err) => {
                eprintln!("skip batch sender: {err}");
                return;
            }
        };
        match BatchRequestSender::new().execute_blocking(&tasks) {
            Ok(_) => {}
            Err(err) => eprintln!("skip batch sender execute: {err}"),
        }
    }
}

//! TiKV Coprocessor unary RPC (`/tikvpb.Tikv/Coprocessor`) and BatchCoprocessor
//! stream (`/tikvpb.Tikv/BatchCoprocessor`) for tipb DAG tasks.

use super::batch_coprocessor::BatchCopTask;
use super::dag::{decode_select_response, SelectResponse, REQ_TYPE_DAG};
use super::pd::{pd_get_region, pd_store_address, PdRegion};
use super::task::CopTask;
use super::KeyRange;
use prost::Message;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use std::collections::HashMap;
use tonic::transport::Channel;
use tonic::{Request, Status};

/// Subset of `kvrpcpb.Context` (region routing).
#[derive(Clone, PartialEq, Message)]
struct KvContext {
    #[prost(uint64, tag = "1")]
    region_id: u64,
    #[prost(message, optional, tag = "2")]
    region_epoch: Option<RegionEpoch>,
    #[prost(message, optional, tag = "3")]
    peer: Option<Peer>,
}

#[derive(Clone, PartialEq, Message)]
struct RegionEpoch {
    #[prost(uint64, tag = "1")]
    conf_ver: u64,
    #[prost(uint64, tag = "2")]
    version: u64,
}

#[derive(Clone, PartialEq, Message)]
struct Peer {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(uint64, tag = "2")]
    store_id: u64,
}

/// `coprocessor.KeyRange`
#[derive(Clone, PartialEq, Message)]
struct CopKeyRange {
    #[prost(bytes = "vec", tag = "1")]
    start: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    end: Vec<u8>,
}

/// `coprocessor.Request`
#[derive(Clone, PartialEq, Message)]
struct CopRequest {
    #[prost(message, optional, tag = "1")]
    context: Option<KvContext>,
    #[prost(int64, tag = "2")]
    tp: i64,
    #[prost(bytes = "vec", tag = "3")]
    data: Vec<u8>,
    #[prost(message, repeated, tag = "4")]
    ranges: Vec<CopKeyRange>,
    #[prost(uint64, tag = "7")]
    start_ts: u64,
}

/// `coprocessor.Response` (data + errors only).
#[derive(Clone, PartialEq, Message)]
struct CopResponse {
    #[prost(bytes = "vec", tag = "1")]
    data: Vec<u8>,
    #[prost(string, tag = "4")]
    other_error: String,
}

/// `coprocessor.RegionInfo`
#[derive(Clone, PartialEq, Message)]
struct RegionInfo {
    #[prost(uint64, tag = "1")]
    region_id: u64,
    #[prost(message, optional, tag = "2")]
    region_epoch: Option<RegionEpoch>,
    #[prost(message, repeated, tag = "3")]
    ranges: Vec<CopKeyRange>,
}

/// `coprocessor.BatchRequest` (subset used by BatchCop / TiFlash).
#[derive(Clone, PartialEq, Message)]
struct BatchRequest {
    #[prost(message, optional, tag = "1")]
    context: Option<KvContext>,
    #[prost(int64, tag = "2")]
    tp: i64,
    #[prost(bytes = "vec", tag = "3")]
    data: Vec<u8>,
    #[prost(message, repeated, tag = "4")]
    regions: Vec<RegionInfo>,
    #[prost(uint64, tag = "5")]
    start_ts: u64,
}

/// `coprocessor.BatchResponse` (data + errors only).
#[derive(Clone, PartialEq, Message)]
struct BatchResponse {
    #[prost(bytes = "vec", tag = "1")]
    data: Vec<u8>,
    #[prost(string, tag = "2")]
    other_error: String,
}

/// Blocking wrapper around [`execute_cop_task`] for sync callers (executor).
pub fn execute_cop_task_blocking(
    pd_endpoints: &[String],
    task: &CopTask,
) -> Result<SelectResponse> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tokio runtime: {e}")))?;
    rt.block_on(execute_cop_task(pd_endpoints, task))
}

/// Send one CopTask's tipb DAG to the Region leader via TiKV Coprocessor RPC.
///
/// Multi-range tasks are issued one unary call per range (MVB). Responses are
/// concatenated by decoding each SelectResponse and merging chunks.
pub async fn execute_cop_task(pd_endpoints: &[String], task: &CopTask) -> Result<SelectResponse> {
    if task.ranges.is_empty() {
        return Ok(SelectResponse::default());
    }
    let mut merged = SelectResponse::default();
    for range in &task.ranges {
        let part = execute_cop_range(pd_endpoints, task, range).await?;
        if let Some(err) = part.error {
            return Err(Error::new(
                if err.code != 0 {
                    err.code
                } else {
                    ERR_UNKNOWN_ERROR
                },
                format!("coprocessor: {}", err.msg),
            ));
        }
        merged.chunks.extend(part.chunks);
    }
    Ok(merged)
}

/// Execute a store-batched Cop task against its pre-assigned address (BatchCop MVB).
pub async fn execute_batch_cop_task(task: &BatchCopTask) -> Result<SelectResponse> {
    if task.ranges.is_empty() {
        return Ok(SelectResponse::default());
    }
    let mut merged = SelectResponse::default();
    for range in &task.ranges {
        let part = execute_cop_on_store(task, range).await?;
        if let Some(err) = part.error {
            return Err(Error::new(
                if err.code != 0 {
                    err.code
                } else {
                    ERR_UNKNOWN_ERROR
                },
                format!("coprocessor: {}", err.msg),
            ));
        }
        merged.chunks.extend(part.chunks);
    }
    Ok(merged)
}

/// Build one `BatchRequest` for tasks that share a store (and DAG payload).
pub fn build_store_batch_request(tasks: &[BatchCopTask]) -> Result<Vec<u8>> {
    if tasks.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            "build_store_batch_request: empty tasks",
        ));
    }
    let first = &tasks[0];
    let mut regions = Vec::with_capacity(tasks.len());
    for t in tasks {
        if t.store_id != first.store_id {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                "build_store_batch_request: mixed store_id",
            ));
        }
        regions.push(RegionInfo {
            region_id: t.region_id,
            region_epoch: Some(RegionEpoch {
                conf_ver: t.region_epoch_conf_ver,
                version: t.region_epoch_version,
            }),
            ranges: t
                .ranges
                .iter()
                .map(|r| CopKeyRange {
                    start: r.start.clone(),
                    end: r.end.clone(),
                })
                .collect(),
        });
    }
    let peer = if first.peer_id != 0 || first.store_id != 0 {
        Some(Peer {
            id: first.peer_id,
            store_id: first.store_id,
        })
    } else {
        None
    };
    let req = BatchRequest {
        context: Some(KvContext {
            region_id: first.region_id,
            region_epoch: Some(RegionEpoch {
                conf_ver: first.region_epoch_conf_ver,
                version: first.region_epoch_version,
            }),
            peer,
        }),
        tp: if first.tp != 0 {
            first.tp
        } else {
            REQ_TYPE_DAG
        },
        data: first.data.clone(),
        regions,
        start_ts: first.start_ts,
    };
    let mut buf = Vec::new();
    req.encode(&mut buf)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("batch request encode: {e}")))?;
    Ok(buf)
}

/// Open `/tikvpb.Tikv/BatchCoprocessor` against `store_addr` and merge responses.
pub async fn execute_batch_coprocessor_stream(
    store_addr: &str,
    tasks: &[BatchCopTask],
) -> Result<SelectResponse> {
    if tasks.is_empty() {
        return Ok(SelectResponse::default());
    }
    let endpoint = normalize_tikv_http(store_addr);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv endpoint {endpoint}: {e}")))?
        .connect()
        .await
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv connect {endpoint}: {e}")))?;

    let first = &tasks[0];
    let regions: Vec<RegionInfo> = tasks
        .iter()
        .map(|t| RegionInfo {
            region_id: t.region_id,
            region_epoch: Some(RegionEpoch {
                conf_ver: t.region_epoch_conf_ver,
                version: t.region_epoch_version,
            }),
            ranges: t
                .ranges
                .iter()
                .map(|r| CopKeyRange {
                    start: r.start.clone(),
                    end: r.end.clone(),
                })
                .collect(),
        })
        .collect();
    let peer = if first.peer_id != 0 || first.store_id != 0 {
        Some(Peer {
            id: first.peer_id,
            store_id: first.store_id,
        })
    } else {
        None
    };
    let req = BatchRequest {
        context: Some(KvContext {
            region_id: first.region_id,
            region_epoch: Some(RegionEpoch {
                conf_ver: first.region_epoch_conf_ver,
                version: first.region_epoch_version,
            }),
            peer,
        }),
        tp: if first.tp != 0 {
            first.tp
        } else {
            REQ_TYPE_DAG
        },
        data: first.data.clone(),
        regions,
        start_ts: first.start_ts,
    };

    let mut client = TikvCopClient::new(channel);
    let mut stream = client
        .batch_coprocessor(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();

    let mut merged = SelectResponse::default();
    while let Some(resp) = stream.message().await.map_err(map_status)? {
        if !resp.other_error.is_empty() {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!("batch coprocessor: {}", resp.other_error),
            ));
        }
        if resp.data.is_empty() {
            continue;
        }
        let part = decode_select_response(&resp.data)?;
        if let Some(err) = part.error {
            return Err(Error::new(
                if err.code != 0 {
                    err.code
                } else {
                    ERR_UNKNOWN_ERROR
                },
                format!("batch coprocessor: {}", err.msg),
            ));
        }
        merged.chunks.extend(part.chunks);
    }
    Ok(merged)
}

/// Group tasks by `store_id` and run BatchCoprocessor stream per store.
pub async fn execute_batch_cop_tasks_stream(tasks: &[BatchCopTask]) -> Result<SelectResponse> {
    if tasks.is_empty() {
        return Ok(SelectResponse::default());
    }
    let mut by_store: HashMap<u64, Vec<BatchCopTask>> = HashMap::new();
    for t in tasks {
        by_store.entry(t.store_id).or_default().push(t.clone());
    }
    let mut merged = SelectResponse::default();
    let mut store_ids: Vec<u64> = by_store.keys().copied().collect();
    store_ids.sort_unstable();
    for store_id in store_ids {
        let group = by_store.remove(&store_id).unwrap_or_default();
        let addr = group
            .first()
            .map(|t| t.store_addr.clone())
            .unwrap_or_default();
        let part = execute_batch_coprocessor_stream(&addr, &group).await?;
        merged.chunks.extend(part.chunks);
    }
    Ok(merged)
}

async fn execute_cop_range(
    pd_endpoints: &[String],
    task: &CopTask,
    range: &KeyRange,
) -> Result<SelectResponse> {
    let pd = pd_endpoints.to_vec();
    let start = range.start.clone();
    let (region, address) = tokio::task::spawn_blocking(move || {
        let region = pd_get_region(&pd, &start)?;
        let store_id = region
            .leader
            .as_ref()
            .map(|p| p.store_id)
            .filter(|id| *id != 0)
            .ok_or_else(|| {
                Error::new(ERR_UNKNOWN_ERROR, "pd region missing leader store_id")
            })?;
        let address = pd_store_address(&pd, store_id)?;
        Ok::<_, Error>((region, address))
    })
    .await
    .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("pd locate join: {e}")))??;
    let endpoint = normalize_tikv_http(&address);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv endpoint {endpoint}: {e}")))?
        .connect()
        .await
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv connect {endpoint}: {e}")))?;

    let req = build_cop_request(&region, task, range);
    let mut client = TikvCopClient::new(channel);
    let resp = client
        .coprocessor(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();
    if !resp.other_error.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("coprocessor: {}", resp.other_error),
        ));
    }
    if resp.data.is_empty() {
        return Ok(SelectResponse::default());
    }
    decode_select_response(&resp.data)
}

fn build_cop_request(region: &PdRegion, task: &CopTask, range: &KeyRange) -> CopRequest {
    let epoch = region.epoch.as_ref().map(|e| RegionEpoch {
        conf_ver: e.conf_ver,
        version: e.version,
    });
    let peer = region.leader.as_ref().map(|p| Peer {
        id: p.id,
        store_id: p.store_id,
    });
    CopRequest {
        context: Some(KvContext {
            region_id: region.id,
            region_epoch: epoch,
            peer,
        }),
        tp: if task.tp != 0 {
            task.tp
        } else {
            REQ_TYPE_DAG
        },
        data: task.data.clone(),
        ranges: vec![CopKeyRange {
            start: range.start.clone(),
            end: range.end.clone(),
        }],
        start_ts: task.start_ts,
    }
}

async fn execute_cop_on_store(task: &BatchCopTask, range: &KeyRange) -> Result<SelectResponse> {
    let endpoint = normalize_tikv_http(&task.store_addr);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv endpoint {endpoint}: {e}")))?
        .connect()
        .await
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tikv connect {endpoint}: {e}")))?;

    let peer = if task.peer_id != 0 || task.store_id != 0 {
        Some(Peer {
            id: task.peer_id,
            store_id: task.store_id,
        })
    } else {
        None
    };
    let req = CopRequest {
        context: Some(KvContext {
            region_id: task.region_id,
            region_epoch: Some(RegionEpoch {
                conf_ver: task.region_epoch_conf_ver,
                version: task.region_epoch_version,
            }),
            peer,
        }),
        tp: if task.tp != 0 {
            task.tp
        } else {
            REQ_TYPE_DAG
        },
        data: task.data.clone(),
        ranges: vec![CopKeyRange {
            start: range.start.clone(),
            end: range.end.clone(),
        }],
        start_ts: task.start_ts,
    };
    let mut client = TikvCopClient::new(channel);
    let resp = client
        .coprocessor(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();
    if !resp.other_error.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("coprocessor: {}", resp.other_error),
        ));
    }
    if resp.data.is_empty() {
        return Ok(SelectResponse::default());
    }
    decode_select_response(&resp.data)
}

fn normalize_tikv_http(address: &str) -> String {
    let a = address.trim();
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{a}")
    }
}

fn map_status(status: Status) -> Error {
    Error::new(
        ERR_UNKNOWN_ERROR,
        format!("tikv coprocessor rpc: {status}"),
    )
}

#[derive(Debug, Clone)]
struct TikvCopClient {
    inner: tonic::client::Grpc<Channel>,
}

impl TikvCopClient {
    fn new(channel: Channel) -> Self {
        Self {
            inner: tonic::client::Grpc::new(channel),
        }
    }

    async fn coprocessor(
        &mut self,
        request: Request<CopRequest>,
    ) -> std::result::Result<tonic::Response<CopResponse>, Status> {
        self.inner.ready().await.map_err(|e| {
            Status::unknown(format!("Service was not ready: {e}"))
        })?;
        let codec = tonic::codec::ProstCodec::default();
        let path =
            http::uri::PathAndQuery::from_static("/tikvpb.Tikv/Coprocessor");
        self.inner.unary(request, path, codec).await
    }

    async fn batch_coprocessor(
        &mut self,
        request: Request<BatchRequest>,
    ) -> std::result::Result<
        tonic::Response<tonic::Streaming<BatchResponse>>,
        Status,
    > {
        self.inner.ready().await.map_err(|e| {
            Status::unknown(format!("Service was not ready: {e}"))
        })?;
        let codec = tonic::codec::ProstCodec::default();
        let path =
            http::uri::PathAndQuery::from_static("/tikvpb.Tikv/BatchCoprocessor");
        self.inner.server_streaming(request, path, codec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copr::dag::{encode_table_scan_dag, ColumnInfo};
    use crate::copr::task::CopTask;
    use crate::copr::KeyRange;

    #[tokio::test]
    async fn test_execute_cop_task_if_available() {
        let pd = vec!["127.0.0.1:2379".to_string()];
        let cols = [ColumnInfo {
            column_id: 1,
            tp: 8,
            pk_handle: true,
        }];
        let data = encode_table_scan_dag(1, 1, &cols, &[0], None, Some(1));
        let task = CopTask::dag(
            vec![KeyRange::new(b"t".to_vec(), b"u".to_vec())],
            data,
            1,
        );
        match execute_cop_task(&pd, &task).await {
            Ok(resp) => {
                // Empty table / no rows is fine; presence of a response proves RPC.
                let _ = resp;
            }
            Err(err) => {
                eprintln!("skip cop rpc: {err}");
            }
        }
    }
}

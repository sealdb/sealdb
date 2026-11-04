//! TiDB MPP exchange RPCs: `DispatchMPPTask`, `EstablishMPPConnection`,
//! `CancelMPPTask`.
//!
//! Subset of `mpp.proto` / `tikvpb.Tikv` used for C6-D root-gather MVB.

use super::batch_coprocessor::BatchCopTask;
use super::dag::{SelectResponse, decode_select_response};
use prost::Message;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use tonic::transport::Channel;
use tonic::{Request, Status};

/// `mpp.TaskMeta` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct TaskMeta {
    #[prost(uint64, tag = "1")]
    pub start_ts: u64,
    #[prost(int64, tag = "2")]
    pub task_id: i64,
    #[prost(string, tag = "4")]
    pub address: String,
    #[prost(uint64, tag = "5")]
    pub gather_id: u64,
    #[prost(uint64, tag = "6")]
    pub query_ts: u64,
    #[prost(uint64, tag = "7")]
    pub local_query_id: u64,
    #[prost(uint64, tag = "8")]
    pub server_id: u64,
    #[prost(int64, tag = "9")]
    pub mpp_version: i64,
}

/// `coprocessor.RegionInfo` (subset for DispatchTaskRequest.regions).
#[derive(Clone, PartialEq, Message)]
struct RegionEpoch {
    #[prost(uint64, tag = "1")]
    conf_ver: u64,
    #[prost(uint64, tag = "2")]
    version: u64,
}

#[derive(Clone, PartialEq, Message)]
struct CopKeyRange {
    #[prost(bytes = "vec", tag = "1")]
    start: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    end: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct RegionInfo {
    #[prost(uint64, tag = "1")]
    region_id: u64,
    #[prost(message, optional, tag = "2")]
    region_epoch: Option<RegionEpoch>,
    #[prost(message, repeated, tag = "3")]
    ranges: Vec<CopKeyRange>,
}

/// `mpp.DispatchTaskRequest` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct DispatchTaskRequest {
    #[prost(message, optional, tag = "1")]
    pub meta: Option<TaskMeta>,
    #[prost(bytes = "vec", tag = "2")]
    pub encoded_plan: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub timeout: i64,
    #[prost(message, repeated, tag = "4")]
    regions: Vec<RegionInfo>,
    #[prost(int64, tag = "5")]
    pub schema_ver: i64,
}

/// `mpp.Error`
#[derive(Clone, PartialEq, Message)]
pub struct MppError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub msg: String,
}

/// `mpp.DispatchTaskResponse` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct DispatchTaskResponse {
    #[prost(message, optional, tag = "1")]
    error: Option<MppError>,
}

/// `mpp.EstablishMPPConnectionRequest`
#[derive(Clone, PartialEq, Message)]
pub struct EstablishMppConnectionRequest {
    #[prost(message, optional, tag = "1")]
    pub sender_meta: Option<TaskMeta>,
    #[prost(message, optional, tag = "2")]
    pub receiver_meta: Option<TaskMeta>,
}

/// `mpp.MPPDataPacket` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct MppDataPacket {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    error: Option<MppError>,
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub chunks: Vec<Vec<u8>>,
}

/// `mpp.CancelTaskRequest` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct CancelTaskRequest {
    #[prost(message, optional, tag = "1")]
    pub meta: Option<TaskMeta>,
    #[prost(message, optional, tag = "2")]
    pub error: Option<MppError>,
}

/// `mpp.CancelTaskResponse` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct CancelTaskResponse {
    #[prost(message, optional, tag = "1")]
    error: Option<MppError>,
}

/// Build TaskMeta for a store-assigned BatchCop fragment.
pub fn task_meta_for(task: &BatchCopTask, task_id: i64, gather_id: u64) -> TaskMeta {
    TaskMeta {
        start_ts: task.start_ts,
        task_id,
        address: task.store_addr.clone(),
        gather_id,
        query_ts: task.start_ts,
        local_query_id: gather_id,
        server_id: 1,
        mpp_version: 1,
    }
}

/// Build a DispatchTaskRequest from one BatchCopTask.
pub fn build_dispatch_request(
    task: &BatchCopTask,
    task_id: i64,
    gather_id: u64,
    schema_ver: i64,
) -> DispatchTaskRequest {
    let regions = vec![RegionInfo {
        region_id: task.region_id,
        region_epoch: Some(RegionEpoch {
            conf_ver: task.region_epoch_conf_ver,
            version: task.region_epoch_version,
        }),
        ranges: task
            .ranges
            .iter()
            .map(|r| CopKeyRange {
                start: r.start.clone(),
                end: r.end.clone(),
            })
            .collect(),
    }];
    DispatchTaskRequest {
        meta: Some(task_meta_for(task, task_id, gather_id)),
        encoded_plan: task.data.clone(),
        timeout: 60,
        regions,
        schema_ver,
    }
}

fn normalize_http(address: &str) -> String {
    let a = address.trim();
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{a}")
    }
}

fn map_status(status: Status) -> Error {
    Error::new(ERR_UNKNOWN_ERROR, format!("tiflash mpp rpc: {status}"))
}

/// Unary `DispatchMPPTask` to a TiFlash store.
pub async fn dispatch_mpp_task(
    store_addr: &str,
    req: DispatchTaskRequest,
) -> Result<DispatchTaskResponse> {
    let endpoint = normalize_http(store_addr);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash endpoint {endpoint}: {e}"),
            )
        })?
        .connect()
        .await
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash connect {endpoint}: {e}"),
            )
        })?;
    let mut client = TikvMppClient::new(channel);
    let resp = client
        .dispatch_mpp_task(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();
    if let Some(err) = &resp.error
        && (err.code != 0 || !err.msg.is_empty())
    {
        return Err(Error::new(
            if err.code != 0 {
                err.code
            } else {
                ERR_UNKNOWN_ERROR
            },
            format!("DispatchMPPTask: {}", err.msg),
        ));
    }
    Ok(resp)
}

/// Unary `CancelMPPTask` to a TiFlash store.
pub async fn cancel_mpp_task(
    store_addr: &str,
    req: CancelTaskRequest,
) -> Result<CancelTaskResponse> {
    let endpoint = normalize_http(store_addr);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash endpoint {endpoint}: {e}"),
            )
        })?
        .connect()
        .await
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash connect {endpoint}: {e}"),
            )
        })?;
    let mut client = TikvMppClient::new(channel);
    let resp = client
        .cancel_mpp_task(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();
    if let Some(err) = &resp.error
        && (err.code != 0 || !err.msg.is_empty())
    {
        return Err(Error::new(
            if err.code != 0 {
                err.code
            } else {
                ERR_UNKNOWN_ERROR
            },
            format!("CancelMPPTask: {}", err.msg),
        ));
    }
    Ok(resp)
}

/// Stream `EstablishMPPConnection` and merge tipb SelectResponse payloads.
pub async fn establish_mpp_connection(
    store_addr: &str,
    req: EstablishMppConnectionRequest,
) -> Result<SelectResponse> {
    let endpoint = normalize_http(store_addr);
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash endpoint {endpoint}: {e}"),
            )
        })?
        .connect()
        .await
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("tiflash connect {endpoint}: {e}"),
            )
        })?;
    let mut client = TikvMppClient::new(channel);
    let mut stream = client
        .establish_mpp_connection(Request::new(req))
        .await
        .map_err(map_status)?
        .into_inner();

    let mut merged = SelectResponse::default();
    while let Some(pkt) = stream.message().await.map_err(map_status)? {
        if let Some(err) = &pkt.error
            && (err.code != 0 || !err.msg.is_empty())
        {
            return Err(Error::new(
                if err.code != 0 {
                    err.code
                } else {
                    ERR_UNKNOWN_ERROR
                },
                format!("MPPDataPacket: {}", err.msg),
            ));
        }
        if !pkt.data.is_empty() {
            let part = decode_select_response(&pkt.data)?;
            if let Some(err) = part.error {
                return Err(Error::new(
                    if err.code != 0 {
                        err.code
                    } else {
                        ERR_UNKNOWN_ERROR
                    },
                    format!("mpp select: {}", err.msg),
                ));
            }
            merged.chunks.extend(part.chunks);
        }
    }
    Ok(merged)
}

/// Merge SelectResponse chunks from multiple gather legs.
pub fn merge_select_responses(parts: impl IntoIterator<Item = SelectResponse>) -> SelectResponse {
    let mut merged = SelectResponse::default();
    for part in parts {
        merged.chunks.extend(part.chunks);
    }
    merged
}

/// EstablishMPPConnection to one dispatched sender (receiver task_id = -1).
pub async fn gather_from_sender(
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

/// Dispatch each BatchCopTask then gather from **all** senders via MPP exchange.
///
/// Previously only the first store was connected; multi-sender merges every
/// EstablishMPPConnection stream into one SelectResponse.
pub async fn execute_mpp_root_gather(
    tasks: &[BatchCopTask],
    gather_id: u64,
    schema_ver: i64,
) -> Result<SelectResponse> {
    if tasks.is_empty() {
        return Ok(SelectResponse::default());
    }
    let mut metas = Vec::with_capacity(tasks.len());
    for (i, task) in tasks.iter().enumerate() {
        let task_id = (i + 1) as i64;
        let req = build_dispatch_request(task, task_id, gather_id, schema_ver);
        let _ = dispatch_mpp_task(&task.store_addr, req).await?;
        metas.push(task_meta_for(task, task_id, gather_id));
    }
    let mut parts = Vec::with_capacity(metas.len());
    for (task, sender) in tasks.iter().zip(metas) {
        parts.push(gather_from_sender(&task.store_addr, sender, gather_id).await?);
    }
    Ok(merge_select_responses(parts))
}

#[derive(Debug, Clone)]
struct TikvMppClient {
    inner: tonic::client::Grpc<Channel>,
}

impl TikvMppClient {
    fn new(channel: Channel) -> Self {
        Self {
            inner: tonic::client::Grpc::new(channel),
        }
    }

    #[allow(clippy::result_large_err)]
    async fn dispatch_mpp_task(
        &mut self,
        request: Request<DispatchTaskRequest>,
    ) -> std::result::Result<tonic::Response<DispatchTaskResponse>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|e| Status::unknown(format!("Service was not ready: {e}")))?;
        let codec = tonic::codec::ProstCodec::default();
        let path = http::uri::PathAndQuery::from_static("/tikvpb.Tikv/DispatchMPPTask");
        self.inner.unary(request, path, codec).await
    }

    #[allow(clippy::result_large_err)]
    async fn establish_mpp_connection(
        &mut self,
        request: Request<EstablishMppConnectionRequest>,
    ) -> std::result::Result<tonic::Response<tonic::Streaming<MppDataPacket>>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|e| Status::unknown(format!("Service was not ready: {e}")))?;
        let codec = tonic::codec::ProstCodec::default();
        let path = http::uri::PathAndQuery::from_static("/tikvpb.Tikv/EstablishMPPConnection");
        self.inner.server_streaming(request, path, codec).await
    }

    #[allow(clippy::result_large_err)]
    async fn cancel_mpp_task(
        &mut self,
        request: Request<CancelTaskRequest>,
    ) -> std::result::Result<tonic::Response<CancelTaskResponse>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|e| Status::unknown(format!("Service was not ready: {e}")))?;
        let codec = tonic::codec::ProstCodec::default();
        let path = http::uri::PathAndQuery::from_static("/tikvpb.Tikv/CancelMPPTask");
        self.inner.unary(request, path, codec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copr::KeyRange;
    use crate::copr::dag::REQ_TYPE_DAG;

    fn sample_task() -> BatchCopTask {
        BatchCopTask {
            store_id: 2,
            store_addr: "127.0.0.1:3930".into(),
            region_id: 9,
            region_epoch_conf_ver: 1,
            region_epoch_version: 2,
            peer_id: 0,
            ranges: vec![KeyRange::new(b"a".to_vec(), b"b".to_vec())],
            data: vec![1, 2, 3],
            tp: REQ_TYPE_DAG,
            start_ts: 42,
            is_tiflash: true,
        }
    }

    #[test]
    fn test_build_dispatch_request_encodes() {
        let req = build_dispatch_request(&sample_task(), 7, 100, 1);
        assert_eq!(req.meta.as_ref().unwrap().task_id, 7);
        assert_eq!(req.encoded_plan, vec![1, 2, 3]);
        assert_eq!(req.regions.len(), 1);
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_task_meta_for() {
        let meta = task_meta_for(&sample_task(), 3, 9);
        assert_eq!(meta.task_id, 3);
        assert_eq!(meta.gather_id, 9);
        assert_eq!(meta.address, "127.0.0.1:3930");
    }

    #[test]
    fn test_cancel_task_request_encodes() {
        let meta = task_meta_for(&sample_task(), 3, 9);
        let req = CancelTaskRequest {
            meta: Some(meta),
            error: Some(MppError {
                code: 1,
                msg: "abort".into(),
            }),
        };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());
        let decoded = CancelTaskRequest::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.meta.unwrap().task_id, 3);
        assert_eq!(decoded.error.unwrap().msg, "abort");
    }

    #[test]
    fn test_merge_select_responses_concatenates_chunks() {
        use crate::copr::dag::Chunk;
        let a = SelectResponse {
            chunks: vec![Chunk {
                rows_data: vec![1],
                rows_meta: vec![],
            }],
            ..Default::default()
        };
        let b = SelectResponse {
            chunks: vec![
                Chunk {
                    rows_data: vec![2],
                    rows_meta: vec![],
                },
                Chunk {
                    rows_data: vec![3],
                    rows_meta: vec![],
                },
            ],
            ..Default::default()
        };
        let merged = merge_select_responses([a, b]);
        assert_eq!(merged.chunks.len(), 3);
        assert_eq!(merged.chunks[0].rows_data, vec![1]);
        assert_eq!(merged.chunks[2].rows_data, vec![3]);
    }
}

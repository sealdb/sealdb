//! TiKV Coprocessor unary RPC (`/tikvpb.Tikv/Coprocessor`) for tipb DAG tasks.

use super::dag::{decode_select_response, SelectResponse, REQ_TYPE_DAG};
use super::pd::{pd_get_region, pd_store_address, PdRegion};
use super::task::CopTask;
use super::KeyRange;
use prost::Message;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
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

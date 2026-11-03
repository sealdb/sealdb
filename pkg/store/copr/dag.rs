//! Minimal tipb DAG request/response for Coprocessor pushdown (C6-C MVB).
//!
//! Field tags match pingcap/tipb so bytes are wire-compatible with TiKV.
//! Full expression pushdown / chunk decode lands in later slices.

use prost::Message;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};

/// TiDB / TiKV Coprocessor request type for a tipb DAG.
pub const REQ_TYPE_DAG: i64 = 103;

/// tipb.ExecType
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExecType {
    TableScan = 0,
    IndexScan = 1,
    Selection = 2,
    Aggregation = 3,
    TopN = 4,
    Limit = 5,
    StreamAgg = 6,
    Projection = 7,
}

/// tipb.ColumnInfo (id-only MVB).
#[derive(Clone, PartialEq, Message)]
pub struct ColumnInfo {
    #[prost(int64, tag = "1")]
    pub column_id: i64,
    #[prost(int32, tag = "2")]
    pub tp: i32,
    #[prost(bool, tag = "12")]
    pub pk_handle: bool,
}

/// tipb.TableScan
#[derive(Clone, PartialEq, Message)]
pub struct TableScan {
    #[prost(int64, tag = "1")]
    pub table_id: i64,
    #[prost(message, repeated, tag = "2")]
    pub columns: Vec<ColumnInfo>,
    #[prost(bool, tag = "3")]
    pub desc: bool,
}

/// tipb.IndexScan
#[derive(Clone, PartialEq, Message)]
pub struct IndexScan {
    #[prost(int64, tag = "1")]
    pub table_id: i64,
    #[prost(int64, tag = "2")]
    pub index_id: i64,
    #[prost(message, repeated, tag = "3")]
    pub columns: Vec<ColumnInfo>,
    #[prost(bool, tag = "4")]
    pub desc: bool,
}

/// tipb.Selection (empty conditions = pass-through).
#[derive(Clone, PartialEq, Message)]
pub struct Selection {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub conditions: Vec<Vec<u8>>,
}

/// tipb.Limit
#[derive(Clone, PartialEq, Message)]
pub struct Limit {
    #[prost(uint64, tag = "1")]
    pub limit: u64,
}

/// tipb.Executor
#[derive(Clone, PartialEq, Message)]
pub struct Executor {
    #[prost(int32, tag = "1")]
    pub tp: i32,
    #[prost(message, optional, tag = "2")]
    pub tbl_scan: Option<TableScan>,
    #[prost(message, optional, tag = "3")]
    pub idx_scan: Option<IndexScan>,
    #[prost(message, optional, tag = "4")]
    pub selection: Option<Selection>,
    #[prost(message, optional, tag = "7")]
    pub limit: Option<Limit>,
}

/// tipb.DAGRequest
#[derive(Clone, PartialEq, Message)]
pub struct DagRequest {
    #[prost(uint64, tag = "1")]
    pub start_ts: u64,
    #[prost(message, repeated, tag = "2")]
    pub executors: Vec<Executor>,
    #[prost(int64, repeated, tag = "3")]
    pub output_offsets: Vec<i64>,
}

/// tipb.Error (subset)
#[derive(Clone, PartialEq, Message)]
pub struct TipbError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub msg: String,
}

/// tipb.Chunk (raw rows_data only)
#[derive(Clone, PartialEq, Message)]
pub struct Chunk {
    #[prost(int32, repeated, packed = "false", tag = "1")]
    pub rows_meta: Vec<i32>,
    #[prost(bytes = "vec", tag = "2")]
    pub rows_data: Vec<u8>,
}

/// tipb.SelectResponse (subset)
#[derive(Clone, PartialEq, Message)]
pub struct SelectResponse {
    #[prost(message, optional, tag = "1")]
    pub error: Option<TipbError>,
    #[prost(message, repeated, tag = "2")]
    pub chunks: Vec<Chunk>,
}

/// Build a TableScan (+ optional Selection/Limit) DAG payload for Coprocessor `data`.
pub fn encode_table_scan_dag(
    start_ts: u64,
    table_id: i64,
    columns: &[ColumnInfo],
    output_offsets: &[i64],
    selection: Option<Selection>,
    limit: Option<u64>,
) -> Vec<u8> {
    let mut executors = vec![Executor {
        tp: ExecType::TableScan as i32,
        tbl_scan: Some(TableScan {
            table_id,
            columns: columns.to_vec(),
            desc: false,
        }),
        ..Executor::default()
    }];
    if let Some(sel) = selection {
        executors.push(Executor {
            tp: ExecType::Selection as i32,
            selection: Some(sel),
            ..Executor::default()
        });
    }
    if let Some(n) = limit {
        executors.push(Executor {
            tp: ExecType::Limit as i32,
            limit: Some(Limit { limit: n }),
            ..Executor::default()
        });
    }
    let dag = DagRequest {
        start_ts,
        executors,
        output_offsets: output_offsets.to_vec(),
    };
    dag.encode_to_vec()
}

/// Decode tipb.SelectResponse; surfaces tipb error as sealdb Error.
pub fn decode_select_response(data: &[u8]) -> Result<SelectResponse> {
    let resp = SelectResponse::decode(data)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tipb SelectResponse: {e}")))?;
    if let Some(err) = &resp.error {
        if err.code != 0 || !err.msg.is_empty() {
            return Err(Error::new(
                if err.code != 0 {
                    err.code
                } else {
                    ERR_UNKNOWN_ERROR
                },
                format!("coprocessor: {}", err.msg),
            ));
        }
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_table_scan_dag_roundtrip() {
        let cols = [ColumnInfo {
            column_id: 1,
            tp: 8,
            pk_handle: true,
        }];
        let bytes = encode_table_scan_dag(42, 100, &cols, &[0], None, Some(10));
        let dag = DagRequest::decode(bytes.as_slice()).unwrap();
        assert_eq!(dag.start_ts, 42);
        assert_eq!(dag.executors.len(), 2); // TableScan + Limit
        assert_eq!(dag.executors[0].tp, ExecType::TableScan as i32);
        assert_eq!(dag.executors[0].tbl_scan.as_ref().unwrap().table_id, 100);
        assert_eq!(dag.executors[1].tp, ExecType::Limit as i32);
        assert_eq!(dag.executors[1].limit.as_ref().unwrap().limit, 10);
        assert_eq!(dag.output_offsets, vec![0]);
    }

    #[test]
    fn test_decode_empty_select_response() {
        let resp = decode_select_response(&SelectResponse::default().encode_to_vec()).unwrap();
        assert!(resp.chunks.is_empty());
    }

    #[test]
    fn test_decode_select_response_error() {
        let raw = SelectResponse {
            error: Some(TipbError {
                code: 1105,
                msg: "boom".into(),
            }),
            chunks: vec![],
        }
        .encode_to_vec();
        let err = decode_select_response(&raw).unwrap_err();
        assert_eq!(err.code, 1105);
    }
}

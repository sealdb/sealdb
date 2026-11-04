//! Minimal tipb DAG request/response for Coprocessor pushdown (C6-C).
//!
//! Field tags match pingcap/tipb so bytes are wire-compatible with TiKV.

use prost::Message;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_types::Datum;
use sealdb_util::codec::{decode_one, encode_value};

/// TiDB / TiKV Coprocessor request type for a tipb DAG.
pub const REQ_TYPE_DAG: i64 = 103;

/// tipb.EncodeType
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum EncodeType {
    TypeDefault = 0,
    TypeChunk = 1,
    TypeCHBlock = 2,
}

/// tipb.ExecType (subset used by SealDB).
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
    // Join = 7 in tipb; Projection is 11.
    Projection = 11,
}

/// tipb.ColumnInfo
#[derive(Clone, PartialEq, Message)]
pub struct ColumnInfo {
    #[prost(int64, tag = "1")]
    pub column_id: i64,
    #[prost(int32, tag = "2")]
    pub tp: i32,
    #[prost(bool, tag = "21")]
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
    pub start_ts_fallback: u64,
    #[prost(message, repeated, tag = "2")]
    pub executors: Vec<Executor>,
    #[prost(uint32, repeated, tag = "5")]
    pub output_offsets: Vec<u32>,
    #[prost(int32, tag = "8")]
    pub encode_type: i32,
}

/// tipb.Error (subset)
#[derive(Clone, PartialEq, Message)]
pub struct TipbError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub msg: String,
}

/// tipb.RowMeta
#[derive(Clone, PartialEq, Message)]
pub struct RowMeta {
    #[prost(int64, tag = "1")]
    pub handle: i64,
    #[prost(int64, tag = "2")]
    pub length: i64,
}

/// tipb.Chunk
#[derive(Clone, PartialEq, Message)]
pub struct Chunk {
    #[prost(bytes = "vec", tag = "3")]
    pub rows_data: Vec<u8>,
    #[prost(message, repeated, tag = "4")]
    pub rows_meta: Vec<RowMeta>,
}

/// tipb.SelectResponse (subset)
#[derive(Clone, PartialEq, Message)]
pub struct SelectResponse {
    #[prost(message, optional, tag = "1")]
    pub error: Option<TipbError>,
    #[prost(message, repeated, tag = "3")]
    pub chunks: Vec<Chunk>,
    #[prost(int32, tag = "9")]
    pub encode_type: i32,
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
        start_ts_fallback: start_ts,
        executors,
        output_offsets: output_offsets.iter().map(|o| *o as u32).collect(),
        encode_type: EncodeType::TypeDefault as i32,
    };
    dag.encode_to_vec()
}

/// Decode tipb.SelectResponse; surfaces tipb error as sealdb Error.
pub fn decode_select_response(data: &[u8]) -> Result<SelectResponse> {
    let resp = SelectResponse::decode(data)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("tipb SelectResponse: {e}")))?;
    if let Some(err) = &resp.error
        && (err.code != 0 || !err.msg.is_empty())
    {
        return Err(Error::new(
            if err.code != 0 {
                err.code
            } else {
                ERR_UNKNOWN_ERROR
            },
            format!("coprocessor: {}", err.msg),
        ));
    }
    Ok(resp)
}

/// Encode one TypeDefault row (column-major EncodeValue stream).
pub fn encode_default_row(values: &[Datum]) -> Vec<u8> {
    let mut buf = Vec::new();
    for v in values {
        encode_value(&mut buf, v);
    }
    buf
}

/// Decode TypeDefault `rows_data` into rows of `col_count` columns each.
pub fn decode_default_rows(rows_data: &[u8], col_count: usize) -> Result<Vec<Vec<Datum>>> {
    if col_count == 0 {
        return Ok(Vec::new());
    }
    let mut rest = rows_data;
    let mut rows = Vec::new();
    while !rest.is_empty() {
        let mut row = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let (next, d) = decode_one(rest)?;
            row.push(d);
            rest = next;
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Decode SelectResponse rows. `col_tps` is MySQL type codes (one per column).
///
/// Dispatches on `encode_type`: TypeDefault uses EncodeValue stream; TypeChunk
/// uses columnar tipb chunk codec. TypeCHBlock is unsupported.
pub fn decode_select_rows(resp: &SelectResponse, col_tps: &[u8]) -> Result<Vec<Vec<Datum>>> {
    match resp.encode_type {
        x if x == EncodeType::TypeCHBlock as i32 => Err(Error::new(
            ERR_UNKNOWN_ERROR,
            "unsupported tipb encode_type TypeCHBlock",
        )),
        x if x == EncodeType::TypeChunk as i32 => {
            let mut out = Vec::new();
            for chunk in &resp.chunks {
                out.extend(sealdb_util::chunk::decode_chunk(col_tps, &chunk.rows_data)?);
            }
            Ok(out)
        }
        _ => {
            // TypeDefault (0) and unknown → EncodeValue layout.
            let mut out = Vec::new();
            for chunk in &resp.chunks {
                out.extend(decode_default_rows(&chunk.rows_data, col_tps.len())?);
            }
            Ok(out)
        }
    }
}

/// Build a SelectResponse carrying TypeDefault-encoded rows (tests / local sim).
pub fn encode_select_response(rows: &[Vec<Datum>]) -> SelectResponse {
    let mut rows_data = Vec::new();
    let mut rows_meta = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let encoded = encode_default_row(row);
        rows_meta.push(RowMeta {
            handle: (i as i64) + 1,
            length: encoded.len() as i64,
        });
        rows_data.extend_from_slice(&encoded);
    }
    SelectResponse {
        error: None,
        chunks: vec![Chunk {
            rows_data,
            rows_meta,
        }],
        encode_type: EncodeType::TypeDefault as i32,
    }
}

/// Build a SelectResponse with TypeChunk columnar encoding.
pub fn encode_select_response_chunk(col_tps: &[u8], rows: &[Vec<Datum>]) -> Result<SelectResponse> {
    let rows_data = sealdb_util::chunk::encode_chunk(col_tps, rows)?;
    Ok(SelectResponse {
        error: None,
        chunks: vec![Chunk {
            rows_data,
            rows_meta: Vec::new(),
        }],
        encode_type: EncodeType::TypeChunk as i32,
    })
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
        assert_eq!(dag.start_ts_fallback, 42);
        assert_eq!(dag.executors.len(), 2); // TableScan + Limit
        assert_eq!(dag.executors[0].tp, ExecType::TableScan as i32);
        assert_eq!(dag.executors[0].tbl_scan.as_ref().unwrap().table_id, 100);
        assert!(dag.executors[0].tbl_scan.as_ref().unwrap().columns[0].pk_handle);
        assert_eq!(dag.executors[1].tp, ExecType::Limit as i32);
        assert_eq!(dag.executors[1].limit.as_ref().unwrap().limit, 10);
        assert_eq!(dag.output_offsets, vec![0]);
        assert_eq!(dag.encode_type, EncodeType::TypeDefault as i32);
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
            encode_type: 0,
        }
        .encode_to_vec();
        let err = decode_select_response(&raw).unwrap_err();
        assert_eq!(err.code, 1105);
    }

    #[test]
    fn test_default_chunk_roundtrip() {
        let rows = vec![
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(2), Datum::Null],
        ];
        let resp = encode_select_response(&rows);
        let tps = [sealdb_types::TYPE_LONG_LONG, sealdb_types::TYPE_VAR_STRING];
        let decoded = decode_select_rows(&resp, &tps).unwrap();
        assert_eq!(decoded, rows);
        let wire = resp.encode_to_vec();
        let parsed = decode_select_response(&wire).unwrap();
        assert_eq!(decode_select_rows(&parsed, &tps).unwrap(), rows);
    }

    #[test]
    fn test_typechunk_select_roundtrip() {
        let tps = [
            sealdb_types::TYPE_LONG_LONG,
            sealdb_types::TYPE_VAR_STRING,
            sealdb_types::TYPE_DOUBLE,
        ];
        let rows = vec![
            vec![Datum::I64(7), Datum::Bytes(b"x".to_vec()), Datum::F64(3.25)],
            vec![Datum::Null, Datum::Bytes(b"yz".to_vec()), Datum::Null],
        ];
        let resp = encode_select_response_chunk(&tps, &rows).unwrap();
        assert_eq!(resp.encode_type, EncodeType::TypeChunk as i32);
        let decoded = decode_select_rows(&resp, &tps).unwrap();
        assert_eq!(decoded[0][0], Datum::I64(7));
        assert_eq!(decoded[0][1], Datum::Bytes(b"x".to_vec()));
        assert_eq!(decoded[1][0], Datum::Null);
        assert_eq!(decoded[1][1], Datum::Bytes(b"yz".to_vec()));
    }
}

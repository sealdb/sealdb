//! TiDB `pkg/distsql/select_result.go` — tipb SelectResponse decode surface.

pub use sealdb_store::{
    decode_default_rows, decode_select_response, decode_select_rows, encode_default_row,
    encode_select_response, encode_select_response_chunk, Chunk, SelectResponse,
};

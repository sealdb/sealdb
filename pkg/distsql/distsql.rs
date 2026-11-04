//! TiDB `pkg/distsql/distsql.go` — DistSQL entry (Select task build).

pub use crate::request_builder::build_table_select_tasks;
pub use crate::select_result::{Chunk, SelectResponse, decode_select_response};

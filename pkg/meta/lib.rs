#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]

pub mod meta;
pub mod model;

pub use meta::{
    Mutator, db_key, encode_hash_data_key, encode_string_data_key, is_db_key, is_table_key,
    parse_db_key, parse_table_key, table_key,
};
pub use model::{
    ColumnInfo, DBInfo, IndexColumn, IndexInfo, Job, JobState, PartitionDefinition, PartitionInfo,
    PartitionType, STATE_DELETE_ONLY, STATE_DELETE_REORGANIZATION, STATE_NONE, STATE_PUBLIC,
    STATE_WRITE_ONLY, STATE_WRITE_REORGANIZATION, SchemaState, TableInfo, ViewInfo,
};

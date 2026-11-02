#![forbid(unsafe_code)]

pub mod chunk;
pub mod codec;
/// TiDB `pkg/util/collate`: collation compare / unicode weights.
pub mod collate;
/// TiDB `pkg/util/rowcodec`: CodecVer 128 table-row values.
pub mod rowcodec;

#![forbid(unsafe_code)]

pub mod handle;
pub mod table;

pub use handle::{Handle, STATS_STORAGE_KEY};
pub use table::{ColumnStats, Histogram, HistogramBucket, Table, TopN, datum_key};

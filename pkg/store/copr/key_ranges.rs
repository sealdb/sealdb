//! TiDB `pkg/store/copr/key_ranges.go` — half-open ranges + Region split.

pub use super::{
    KeyRange, clear_pd_endpoints, set_pd_endpoints, split_key_ranges, split_key_ranges_for_scan,
    split_key_ranges_via_pd,
};

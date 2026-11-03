//! TiDB `pkg/store/copr/region_cache.go` — Region locate via PD HTTP (MVB).
//!
//! Full RegionCache (epoch, buckets, invalidate) remains a later deepen.

pub use super::pd::{
    decode_pd_key, hex_encode, pd_get_region, pd_store_address, PdPeer, PdRegion, PdRegionEpoch,
};

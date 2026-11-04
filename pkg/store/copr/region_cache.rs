//! TiDB `pkg/store/copr/region_cache.go` — Region locate via PD HTTP (MVB).
//!
//! Full RegionCache (epoch, buckets, invalidate) remains a later deepen.

pub use super::pd::{
    PdClient, PdPeer, PdRegion, PdRegionEpoch, PdStoreInfo, decode_pd_key, hex_encode,
    pd_get_region, pd_get_store, pd_list_stores, pd_scan_regions, pd_store_address,
};

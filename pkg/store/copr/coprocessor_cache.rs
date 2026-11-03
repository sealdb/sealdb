//! TiDB `pkg/store/copr/coprocessor_cache.go` — Cop response cache (stub).
//!
//! TiDB caches Coprocessor results by region+dag fingerprint. Not wired yet.

/// Placeholder for a future Coprocessor response cache.
#[derive(Clone, Debug, Default)]
pub struct CoprocessorCache;

impl CoprocessorCache {
    pub fn new() -> Self {
        Self
    }
}

//! Session/domain binding handle (TiDB `pkg/bindinfo/session_handle.go`).
//!
//! MVB: one Domain-shared handle backs CREATE / DROP / SHOW BINDING and
//! SELECT rewrite. Session vs global scope is not split yet.

use crate::binding::{Binding, normalize_sql};
use crate::binding_cache::BindingCache;
use std::sync::Arc;

/// Shared handle for creating, dropping, listing, and matching bindings.
#[derive(Clone, Debug, Default)]
pub struct SessionBindingHandle {
    cache: Arc<BindingCache>,
}

impl SessionBindingHandle {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(BindingCache::new()),
        }
    }

    /// Create or replace a binding keyed by normalized restored original SQL.
    pub fn create_binding(&self, binding: Binding, normalized_original: &str) {
        self.cache.set(normalized_original, binding);
    }

    /// Drop the binding whose original SQL normalizes to `normalized_original`.
    pub fn drop_binding(&self, normalized_original: &str) -> bool {
        self.cache.remove(normalized_original)
    }

    /// Match an enabled binding for restored statement SQL.
    pub fn match_binding(&self, restored_sql: &str) -> Option<Binding> {
        self.cache.get_enabled(&normalize_sql(restored_sql))
    }

    /// All bindings for `SHOW BINDINGS`.
    pub fn all_bindings(&self) -> Vec<Binding> {
        self.cache.all()
    }
}

//! Session/domain binding handle (TiDB `pkg/bindinfo/session_handle.go`).
//!
//! MVB: one Domain-shared handle backs CREATE / DROP / SHOW BINDING and
//! SELECT rewrite. Session vs global scope is not split yet.

use crate::binding::Binding;
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

    /// Drop the binding whose original SQL normalizes to `normalized_original`
    /// in the requested scope.
    pub fn drop_binding(&self, normalized_original: &str, global: bool) -> bool {
        self.cache.remove(normalized_original, global)
    }

    /// Enable or disable an existing binding by its original statement text.
    pub fn set_binding_enabled(&self, original_sql: &str, enabled: bool) -> bool {
        self.cache.set_enabled(original_sql, enabled)
    }

    /// Match an enabled binding for an already-normalized key (the session
    /// builds it with the current database qualification).
    pub fn match_binding(&self, normalized_original: &str) -> Option<Binding> {
        self.cache.get_enabled(normalized_original)
    }

    /// All bindings for `SHOW BINDINGS`.
    pub fn all_bindings(&self) -> Vec<Binding> {
        self.cache.all()
    }
}

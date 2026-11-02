//! In-memory binding cache (TiDB `pkg/bindinfo/binding_cache.go` simplified).

use crate::binding::{Binding, normalize_sql};
use std::collections::HashMap;
use std::sync::Mutex;

/// Thread-safe map from normalized original SQL → binding.
#[derive(Debug, Default)]
pub struct BindingCache {
    inner: Mutex<HashMap<String, Binding>>,
}

impl BindingCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the binding for `normalized_original`.
    pub fn set(&self, normalized_original: impl Into<String>, binding: Binding) {
        self.inner
            .lock()
            .expect("binding cache lock")
            .insert(normalized_original.into(), binding);
    }

    /// Remove the binding keyed by normalized original SQL. Returns true if present.
    pub fn remove(&self, normalized_original: &str) -> bool {
        self.inner
            .lock()
            .expect("binding cache lock")
            .remove(normalized_original)
            .is_some()
    }

    /// Look up an enabled binding by normalized original SQL.
    pub fn get_enabled(&self, normalized_original: &str) -> Option<Binding> {
        self.inner
            .lock()
            .expect("binding cache lock")
            .get(normalized_original)
            .filter(|b| b.is_enabled())
            .cloned()
    }

    /// All bindings currently stored (any status), sorted by original SQL.
    pub fn all(&self) -> Vec<Binding> {
        let mut out: Vec<_> = self
            .inner
            .lock()
            .expect("binding cache lock")
            .values()
            .cloned()
            .collect();
        out.sort_by(|a, b| a.original_sql.cmp(&b.original_sql));
        out
    }
}

/// Convenience: normalize then look up.
pub fn match_sql(cache: &BindingCache, restored_sql: &str) -> Option<Binding> {
    cache.get_enabled(&normalize_sql(restored_sql))
}

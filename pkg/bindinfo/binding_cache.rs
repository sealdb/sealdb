//! In-memory binding cache (TiDB `pkg/bindinfo/binding_cache.go` simplified).

use crate::binding::{Binding, normalize_exact, normalize_sql};
use std::collections::HashMap;
use std::sync::Mutex;

/// Thread-safe map from normalized original SQL → binding.
#[derive(Debug, Default)]
pub struct BindingCache {
    /// Each key keeps its history: re-creating a binding marks the previous
    /// row `deleted` and appends the new one (TiDB `mysql.bind_info` rows).
    inner: Mutex<HashMap<String, Vec<Binding>>>,
}

impl BindingCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a binding for `normalized_original`. Re-creating a binding keeps
    /// the previous row as `deleted` (TiDB inserts a new `mysql.bind_info`
    /// row instead of updating the old one).
    pub fn set(&self, normalized_original: impl Into<String>, binding: Binding) {
        let mut inner = self.inner.lock().expect("binding cache lock");
        let key = normalized_original.into();
        let entry = inner.entry(key).or_default();
        for previous in entry.iter_mut() {
            if previous.status != "deleted" {
                previous.status = "deleted".to_string();
            }
        }
        entry.push(binding);
    }

    /// Remove the binding keyed by normalized original SQL in `global` scope.
    /// Returns true if a matching binding was present.
    pub fn remove(&self, normalized_original: &str, global: bool) -> bool {
        let mut inner = self.inner.lock().expect("binding cache lock");
        let Some(rows) = inner.get_mut(normalized_original) else {
            return false;
        };
        let before = rows.len();
        rows.retain(|binding| binding.global != global);
        let removed = rows.len() != before;
        if rows.is_empty() {
            inner.remove(normalized_original);
        }
        removed
    }

    /// Enable/disable the binding whose *original* SQL (values kept) matches
    /// `original_sql`. TiDB's `SET BINDING` keys on the statement text, so
    /// `a in (1)` does not toggle a binding created for `a in (1,2,3)` even
    /// though both share a normalized binding key.
    pub fn set_enabled(&self, original_sql: &str, enabled: bool) -> bool {
        let wanted = normalize_exact(original_sql);
        let mut inner = self.inner.lock().expect("binding cache lock");
        for rows in inner.values_mut() {
            for binding in rows.iter_mut() {
                if binding.status != "deleted" && normalize_exact(&binding.original_sql) == wanted {
                    binding.status = if enabled { "enabled" } else { "disabled" }.to_string();
                    return true;
                }
            }
        }
        false
    }

    /// Look up an enabled binding by normalized original SQL.
    pub fn get_enabled(&self, normalized_original: &str) -> Option<Binding> {
        self.inner
            .lock()
            .expect("binding cache lock")
            .get(normalized_original)
            .and_then(|rows| rows.iter().rev().find(|b| b.is_enabled()).cloned())
    }

    /// All bindings currently stored (any status), sorted by original SQL.
    pub fn all(&self) -> Vec<Binding> {
        let mut out: Vec<_> = self
            .inner
            .lock()
            .expect("binding cache lock")
            .values()
            .flat_map(|rows| rows.iter().cloned())
            .collect();
        out.sort_by(|a, b| a.original_sql.cmp(&b.original_sql));
        out
    }
}

/// Convenience: normalize then look up.
pub fn match_sql(cache: &BindingCache, restored_sql: &str) -> Option<Binding> {
    cache.get_enabled(&normalize_sql(restored_sql))
}

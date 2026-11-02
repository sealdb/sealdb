//! Instance-level (Domain-shared) plan cache (TiDB `plan_cache_instance.go`, MVB).
//!
//! Held on Domain via `Arc` so sessions sharing a Domain share hits.
//! Exposes a hit counter for tests.

use crate::core::physical_plans::PhysicalPlan;
use crate::core::plan_cache::PlanCacheValue;
use crate::core::plan_cache_lru::{DEFAULT_CAPACITY, LRUPlanCache};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Domain-shared prepared-plan cache.
#[derive(Debug)]
pub struct InstancePlanCache {
    inner: Mutex<LRUPlanCache>,
    hits: AtomicU64,
}

impl InstancePlanCache {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LRUPlanCache::new(capacity)),
            hits: AtomicU64::new(0),
        }
    }

    /// Look up a plan for `key` valid at `schema_ver`. Increments hit counter on hit.
    pub fn get(&self, key: &str, schema_ver: i64) -> Option<PhysicalPlan> {
        let mut cache = self.inner.lock().expect("instance plan cache lock");
        let plan = cache.get(key).and_then(|v| v.plan_if_valid(schema_ver))?;
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(plan)
    }

    /// Insert or replace the plan for `key`.
    pub fn put(&self, key: String, schema_ver: i64, plan: PhysicalPlan) {
        let mut cache = self.inner.lock().expect("instance plan cache lock");
        cache.put(key, PlanCacheValue::new(schema_ver, plan));
    }

    /// Test / metrics hook: number of successful [`Self::get`] hits.
    pub fn hit_count(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        self.inner.lock().expect("instance plan cache lock").clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("instance plan cache lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("instance plan cache lock")
            .is_empty()
    }
}

impl Default for InstancePlanCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::physical_plans::PhysicalPlan;

    #[test]
    fn instance_plan_cache_hit_and_schema_miss() {
        let cache = InstancePlanCache::new();
        let plan = PhysicalPlan::Simple {
            kind: "select".into(),
        };
        cache.put("SELECT 1".into(), 1, plan.clone());
        assert!(cache.get("SELECT 1", 1).is_some());
        assert_eq!(cache.hit_count(), 1);
        assert!(cache.get("SELECT 1", 2).is_none());
        assert_eq!(cache.hit_count(), 1);
    }
}

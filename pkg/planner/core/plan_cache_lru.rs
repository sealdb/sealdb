//! Thin LRU wrapper for the plan cache (TiDB `pkg/planner/core/plan_cache_lru.go`, MVB).
//!
//! Capacity eviction only; Domain-shared storage lives in `plan_cache_instance`.

use crate::core::plan_cache::PlanCacheValue;
use std::collections::{HashMap, VecDeque};

/// Default capacity when callers pass 0 (matches TiDB fallback behaviour).
pub const DEFAULT_CAPACITY: usize = 100;

/// Least-recently-used map of restored SQL → [`PlanCacheValue`].
#[derive(Debug)]
pub struct LRUPlanCache {
    capacity: usize,
    map: HashMap<String, PlanCacheValue>,
    order: VecDeque<String>,
}

impl LRUPlanCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = if capacity < 1 {
            DEFAULT_CAPACITY
        } else {
            capacity
        };
        Self {
            capacity,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&mut self, key: &str) -> Option<&PlanCacheValue> {
        if !self.map.contains_key(key) {
            return None;
        }
        self.touch(key);
        self.map.get(key)
    }

    pub fn put(&mut self, key: String, value: PlanCacheValue) {
        if self.map.contains_key(&key) {
            self.map.insert(key.clone(), value);
            self.touch(&key);
            return;
        }
        while self.map.len() >= self.capacity {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }
}

impl Default for LRUPlanCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

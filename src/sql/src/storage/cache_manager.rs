use common::Result;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use crate::optimizer::OptimizedPlan;
use crate::executor::execution_models::QueryResult;

/// PostgreSQL 风格的缓存管理器
pub struct CacheManager {
    /// 查询计划缓存
    plan_cache: RwLock<HashMap<String, CachedPlan>>,
    /// 结果集缓存
    result_cache: RwLock<HashMap<String, CachedResult>>,
    /// 统计信息缓存
    stats_cache: RwLock<HashMap<String, TableStats>>,
    /// 缓存统计
    stats: Mutex<CacheStats>,
}

impl CacheManager {
    pub fn new() -> Self {
        Self {
            plan_cache: RwLock::new(HashMap::new()),
            result_cache: RwLock::new(HashMap::new()),
            stats_cache: RwLock::new(HashMap::new()),
            stats: Mutex::new(CacheStats::new()),
        }
    }

    /// 缓存查询计划
    pub fn cache_plan(&self, sql: &str, plan: OptimizedPlan) -> Result<()> {
        let mut stats = self.stats.lock().unwrap();
        stats.plan_cache_entries += 1;

        let cached_plan = CachedPlan {
            plan,
            created_at: Instant::now(),
            access_count: 0,
        };

        let mut cache = self.plan_cache.write().unwrap();
        cache.insert(sql.to_string(), cached_plan);

        Ok(())
    }

    /// 获取缓存的查询计划
    pub fn get_cached_plan(&self, sql: &str) -> Option<OptimizedPlan> {
        let mut stats = self.stats.lock().unwrap();
        stats.plan_cache_lookups += 1;

        let mut cache = self.plan_cache.write().unwrap();
        if let Some(cached_plan) = cache.get_mut(sql) {
            cached_plan.access_count += 1;
            stats.plan_cache_hits += 1;
            Some(cached_plan.plan.clone())
        } else {
            stats.plan_cache_misses += 1;
            None
        }
    }

    /// 缓存查询结果
    pub fn cache_result(&self, key: &str, result: QueryResult) -> Result<()> {
        let mut stats = self.stats.lock().unwrap();
        stats.result_cache_entries += 1;

        let cached_result = CachedResult {
            result,
            created_at: Instant::now(),
            access_count: 0,
        };

        let mut cache = self.result_cache.write().unwrap();
        cache.insert(key.to_string(), cached_result);

        Ok(())
    }

    /// 获取缓存的查询结果
    pub fn get_cached_result(&self, key: &str) -> Option<QueryResult> {
        let mut stats = self.stats.lock().unwrap();
        stats.result_cache_lookups += 1;

        let mut cache = self.result_cache.write().unwrap();
        if let Some(cached_result) = cache.get_mut(key) {
            cached_result.access_count += 1;
            stats.result_cache_hits += 1;
            Some(cached_result.result.clone())
        } else {
            stats.result_cache_misses += 1;
            None
        }
    }

    /// 缓存表统计信息
    pub fn cache_table_stats(&self, table_name: &str, stats: TableStats) -> Result<()> {
        let mut cache = self.stats_cache.write().unwrap();
        cache.insert(table_name.to_string(), stats);
        Ok(())
    }

    /// 获取缓存的表统计信息
    pub fn get_cached_table_stats(&self, table_name: &str) -> Option<TableStats> {
        let cache = self.stats_cache.read().unwrap();
        cache.get(table_name).cloned()
    }

    /// 清理过期缓存
    pub fn cleanup_expired_cache(&self, max_age: std::time::Duration) -> Result<()> {
        let now = Instant::now();

        // 清理过期的查询计划缓存
        {
            let mut cache = self.plan_cache.write().unwrap();
            cache.retain(|_, cached_plan| {
                now.duration_since(cached_plan.created_at) < max_age
            });
        }

        // 清理过期的结果缓存
        {
            let mut cache = self.result_cache.write().unwrap();
            cache.retain(|_, cached_result| {
                now.duration_since(cached_result.created_at) < max_age
            });
        }

        // 清理过期的统计信息缓存
        {
            let mut cache = self.stats_cache.write().unwrap();
            cache.retain(|_, table_stats| {
                now.duration_since(table_stats.last_analyzed) < max_age
            });
        }

        Ok(())
    }

    /// 获取缓存统计
    pub fn get_stats(&self) -> CacheStats {
        self.stats.lock().unwrap().clone()
    }

    /// 清空所有缓存
    pub fn clear_all_cache(&self) -> Result<()> {
        {
            let mut cache = self.plan_cache.write().unwrap();
            cache.clear();
        }
        {
            let mut cache = self.result_cache.write().unwrap();
            cache.clear();
        }
        {
            let mut cache = self.stats_cache.write().unwrap();
            cache.clear();
        }
        Ok(())
    }
}

/// 缓存的查询计划
#[derive(Debug, Clone)]
pub struct CachedPlan {
    pub plan: OptimizedPlan,
    pub created_at: Instant,
    pub access_count: u64,
}

/// 缓存的查询结果
#[derive(Debug, Clone)]
pub struct CachedResult {
    pub result: QueryResult,
    pub created_at: Instant,
    pub access_count: u64,
}

/// 表统计信息
#[derive(Debug, Clone)]
pub struct TableStats {
    pub row_count: u64,
    pub page_count: u64,
    pub avg_row_size: f64,
    pub last_analyzed: Instant,
}

impl TableStats {
    pub fn new(row_count: u64, page_count: u64, avg_row_size: f64) -> Self {
        Self {
            row_count,
            page_count,
            avg_row_size,
            last_analyzed: Instant::now(),
        }
    }
}

/// 缓存统计
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub plan_cache_entries: u64,
    pub plan_cache_lookups: u64,
    pub plan_cache_hits: u64,
    pub plan_cache_misses: u64,
    pub result_cache_entries: u64,
    pub result_cache_lookups: u64,
    pub result_cache_hits: u64,
    pub result_cache_misses: u64,
}

impl CacheStats {
    pub fn new() -> Self {
        Self {
            plan_cache_entries: 0,
            plan_cache_lookups: 0,
            plan_cache_hits: 0,
            plan_cache_misses: 0,
            result_cache_entries: 0,
            result_cache_lookups: 0,
            result_cache_hits: 0,
            result_cache_misses: 0,
        }
    }

    pub fn plan_cache_hit_rate(&self) -> f64 {
        if self.plan_cache_lookups == 0 {
            0.0
        } else {
            self.plan_cache_hits as f64 / self.plan_cache_lookups as f64
        }
    }

    pub fn result_cache_hit_rate(&self) -> f64 {
        if self.result_cache_lookups == 0 {
            0.0
        } else {
            self.result_cache_hits as f64 / self.result_cache_lookups as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::OptimizedPlan;

    #[test]
    fn test_cache_manager_new() {
        let cache_manager = CacheManager::new();
        let stats = cache_manager.get_stats();
        assert_eq!(stats.plan_cache_entries, 0);
        assert_eq!(stats.result_cache_entries, 0);
    }

    #[test]
    fn test_cache_plan() {
        let cache_manager = CacheManager::new();

        let plan = OptimizedPlan {
            nodes: vec![],
            estimated_cost: 0.0,
            estimated_rows: 0,
        };

        let result = cache_manager.cache_plan("SELECT * FROM users", plan);
        assert!(result.is_ok());

        let stats = cache_manager.get_stats();
        assert_eq!(stats.plan_cache_entries, 1);
    }

    #[test]
    fn test_get_cached_plan() {
        let cache_manager = CacheManager::new();

        let plan = OptimizedPlan {
            nodes: vec![],
            estimated_cost: 0.0,
            estimated_rows: 0,
        };

        // 缓存查询计划
        cache_manager.cache_plan("SELECT * FROM users", plan).unwrap();

        // 获取缓存的查询计划
        let cached_plan = cache_manager.get_cached_plan("SELECT * FROM users");
        assert!(cached_plan.is_some());

        let stats = cache_manager.get_stats();
        assert_eq!(stats.plan_cache_hits, 1);
        assert_eq!(stats.plan_cache_lookups, 1);
    }

    #[test]
    fn test_cache_result() {
        let cache_manager = CacheManager::new();

        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![vec!["1".to_string(), "Alice".to_string()]],
            affected_rows: 0,
            last_insert_id: None,
        };

        let cache_result = cache_manager.cache_result("users_result", result);
        assert!(cache_result.is_ok());

        let stats = cache_manager.get_stats();
        assert_eq!(stats.result_cache_entries, 1);
    }

    #[test]
    fn test_cache_table_stats() {
        let cache_manager = CacheManager::new();

        let table_stats = TableStats::new(1000, 10, 100.0);
        let result = cache_manager.cache_table_stats("users", table_stats);
        assert!(result.is_ok());

        let cached_stats = cache_manager.get_cached_table_stats("users");
        assert!(cached_stats.is_some());

        let stats = cached_stats.unwrap();
        assert_eq!(stats.row_count, 1000);
        assert_eq!(stats.page_count, 10);
        assert_eq!(stats.avg_row_size, 100.0);
    }

    #[test]
    fn test_clear_all_cache() {
        let cache_manager = CacheManager::new();

        // 添加一些缓存
        let plan = OptimizedPlan {
            nodes: vec![],
            estimated_cost: 0.0,
            estimated_rows: 0,
        };
        cache_manager.cache_plan("SELECT * FROM users", plan).unwrap();

        let result = QueryResult {
            columns: vec![],
            rows: vec![],
            affected_rows: 0,
            last_insert_id: None,
        };
        cache_manager.cache_result("test", result).unwrap();

        // 清空缓存
        let clear_result = cache_manager.clear_all_cache();
        assert!(clear_result.is_ok());

        // 验证缓存已清空
        let cached_plan = cache_manager.get_cached_plan("SELECT * FROM users");
        assert!(cached_plan.is_none());

        let cached_result = cache_manager.get_cached_result("test");
        assert!(cached_result.is_none());
    }

    #[test]
    fn test_cache_stats_hit_rates() {
        let cache_manager = CacheManager::new();

        let plan = OptimizedPlan {
            nodes: vec![],
            estimated_cost: 0.0,
            estimated_rows: 0,
        };

        // 缓存查询计划
        cache_manager.cache_plan("SELECT * FROM users", plan).unwrap();

        // 获取缓存的查询计划（命中）
        let _cached_plan = cache_manager.get_cached_plan("SELECT * FROM users");

        // 获取不存在的查询计划（未命中）
        let _missing_plan = cache_manager.get_cached_plan("SELECT * FROM non_existent");

        let stats = cache_manager.get_stats();
        assert_eq!(stats.plan_cache_hit_rate(), 0.5); // 1命中，1未命中
    }
}
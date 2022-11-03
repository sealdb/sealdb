//! SealDB 存储配置
//!
//! 管理存储相关的配置参数

use serde::{Deserialize, Serialize};

/// 存储配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// 缓冲池配置
    pub buffer_pool_config: BufferPoolConfig,

    /// 缓存配置
    pub cache_config: CacheConfig,

    /// 内存管理配置
    pub memory_config: StorageMemoryConfig,

    /// 工作线程池配置
    pub worker_pool_config: WorkerPoolConfig,

    /// 磁盘配置
    pub disk_config: DiskConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            buffer_pool_config: BufferPoolConfig::default(),
            cache_config: CacheConfig::default(),
            memory_config: StorageMemoryConfig::default(),
            worker_pool_config: WorkerPoolConfig::default(),
            disk_config: DiskConfig::default(),
        }
    }
}

/// 缓冲池配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferPoolConfig {
    /// 缓冲池大小 (MB)
    pub buffer_pool_size_mb: usize,

    /// 页面大小 (KB)
    pub page_size_kb: usize,

    /// 是否启用预取
    pub enable_prefetch: bool,

    /// 预取页面数
    pub prefetch_pages: usize,

    /// 是否启用脏页写回
    pub enable_dirty_page_writeback: bool,

    /// 脏页写回阈值
    pub dirty_page_writeback_threshold: f64,

    /// 是否启用 LRU 替换策略
    pub enable_lru_replacement: bool,

    /// LRU 缓存大小
    pub lru_cache_size: usize,
}

impl Default for BufferPoolConfig {
    fn default() -> Self {
        Self {
            buffer_pool_size_mb: 512,
            page_size_kb: 8,
            enable_prefetch: true,
            prefetch_pages: 4,
            enable_dirty_page_writeback: true,
            dirty_page_writeback_threshold: 0.8,
            enable_lru_replacement: true,
            lru_cache_size: 1000,
        }
    }
}

/// 缓存配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// 是否启用查询计划缓存
    pub enable_query_plan_cache: bool,

    /// 查询计划缓存大小
    pub query_plan_cache_size: usize,

    /// 是否启用结果缓存
    pub enable_result_cache: bool,

    /// 结果缓存大小
    pub result_cache_size: usize,

    /// 缓存过期时间（秒）
    pub cache_ttl_seconds: u64,

    /// 是否启用缓存统计
    pub enable_cache_statistics: bool,

    /// 缓存命中率阈值
    pub cache_hit_rate_threshold: f64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enable_query_plan_cache: true,
            query_plan_cache_size: 1000,
            enable_result_cache: true,
            result_cache_size: 1000,
            cache_ttl_seconds: 3600,
            enable_cache_statistics: true,
            cache_hit_rate_threshold: 0.8,
        }
    }
}

/// 存储内存配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMemoryConfig {
    /// 工作内存大小 (MB)
    pub work_memory_mb: usize,

    /// 共享内存大小 (MB)
    pub shared_memory_mb: usize,

    /// 是否启用内存监控
    pub enable_memory_monitoring: bool,

    /// 内存使用阈值 (0.0-1.0)
    pub memory_usage_threshold: f64,

    /// 是否启用内存压缩
    pub enable_memory_compression: bool,

    /// 内存分配策略
    pub allocation_strategy: StorageAllocationStrategy,
}

impl Default for StorageMemoryConfig {
    fn default() -> Self {
        Self {
            work_memory_mb: 128,
            shared_memory_mb: 512,
            enable_memory_monitoring: true,
            memory_usage_threshold: 0.8,
            enable_memory_compression: false,
            allocation_strategy: StorageAllocationStrategy::Dynamic,
        }
    }
}

/// 存储分配策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageAllocationStrategy {
    Static,
    Dynamic,
    Pooled,
    Slab,
}

/// 工作线程池配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerPoolConfig {
    /// 工作线程数
    pub worker_threads: usize,

    /// 线程池大小
    pub thread_pool_size: usize,

    /// 任务队列大小
    pub task_queue_size: usize,

    /// 是否启用线程监控
    pub enable_thread_monitoring: bool,

    /// 线程空闲超时时间（秒）
    pub thread_idle_timeout_seconds: u64,

    /// 是否启用任务优先级
    pub enable_task_priority: bool,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            worker_threads: 8,
            thread_pool_size: 16,
            task_queue_size: 1000,
            enable_thread_monitoring: true,
            thread_idle_timeout_seconds: 300,
            enable_task_priority: true,
        }
    }
}

/// 磁盘配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    /// 数据目录
    pub data_directory: String,

    /// 临时目录
    pub temp_directory: String,

    /// 是否启用异步 I/O
    pub enable_async_io: bool,

    /// 是否启用直接 I/O
    pub enable_direct_io: bool,

    /// I/O 缓冲区大小 (KB)
    pub io_buffer_size_kb: usize,

    /// 是否启用磁盘监控
    pub enable_disk_monitoring: bool,

    /// 磁盘使用阈值 (0.0-1.0)
    pub disk_usage_threshold: f64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            data_directory: "/var/lib/sealdb/data".to_string(),
            temp_directory: "/var/lib/sealdb/temp".to_string(),
            enable_async_io: true,
            enable_direct_io: false,
            io_buffer_size_kb: 64,
            enable_disk_monitoring: true,
            disk_usage_threshold: 0.9,
        }
    }
}

/// 存储配置管理器
#[derive(Debug, Clone)]
pub struct StorageConfigManager {
    config: StorageConfig,
}

impl StorageConfigManager {
    pub fn new() -> Self {
        Self {
            config: StorageConfig::default(),
        }
    }

    pub fn with_config(config: StorageConfig) -> Self {
        Self { config }
    }

    /// 从文件加载配置
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: StorageConfig = serde_yaml::from_str(&content)?;
        Ok(Self { config })
    }

    /// 保存配置到文件
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = serde_yaml::to_string(&self.config)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 获取配置
    pub fn get_config(&self) -> &StorageConfig {
        &self.config
    }

    /// 更新配置
    pub fn update_config(&mut self, config: StorageConfig) {
        self.config = config;
    }

    /// 验证配置
    pub fn validate(&self) -> Result<(), String> {
        if self.config.buffer_pool_config.buffer_pool_size_mb == 0 {
            return Err("buffer_pool_size_mb must be greater than 0".to_string());
        }

        if self.config.buffer_pool_config.page_size_kb == 0 {
            return Err("page_size_kb must be greater than 0".to_string());
        }

        if self.config.cache_config.query_plan_cache_size == 0 {
            return Err("query_plan_cache_size must be greater than 0".to_string());
        }

        if self.config.memory_config.work_memory_mb == 0 {
            return Err("work_memory_mb must be greater than 0".to_string());
        }

        if self.config.worker_pool_config.worker_threads == 0 {
            return Err("worker_threads must be greater than 0".to_string());
        }

        if self.config.disk_config.io_buffer_size_kb == 0 {
            return Err("io_buffer_size_kb must be greater than 0".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_config_default() {
        let config = StorageConfig::default();
        assert!(config.buffer_pool_config.enable_prefetch);
        assert!(config.cache_config.enable_query_plan_cache);
        assert!(config.memory_config.enable_memory_monitoring);
        assert!(config.worker_pool_config.enable_thread_monitoring);
    }

    #[test]
    fn test_config_manager() {
        let manager = StorageConfigManager::new();
        let config = manager.get_config();
        assert!(config.buffer_pool_config.enable_prefetch);
        assert!(config.cache_config.enable_query_plan_cache);
    }

    #[test]
    fn test_config_validation() {
        let manager = StorageConfigManager::new();
        assert!(manager.validate().is_ok());
    }
}
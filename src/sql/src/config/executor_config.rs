//! SealDB 执行器配置
//! 
//! 管理查询执行器的配置参数

use serde::{Deserialize, Serialize};

/// 执行器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// 是否启用火山模型执行器
    pub enable_volcano_executor: bool,
    
    /// 是否启用流水线执行器
    pub enable_pipeline_executor: bool,
    
    /// 是否启用向量化执行器
    pub enable_vectorized_executor: bool,
    
    /// 是否启用 MPP 执行器
    pub enable_mpp_executor: bool,
    
    /// 火山模型配置
    pub volcano_config: VolcanoConfig,
    
    /// 流水线配置
    pub pipeline_config: PipelineConfig,
    
    /// 向量化配置
    pub vectorized_config: VectorizedConfig,
    
    /// MPP 配置
    pub mpp_config: MppConfig,
    
    /// 内存配置
    pub memory_config: MemoryConfig,
    
    /// 并行配置
    pub parallel_config: ParallelConfig,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            enable_volcano_executor: true,
            enable_pipeline_executor: true,
            enable_vectorized_executor: true,
            enable_mpp_executor: true,
            volcano_config: VolcanoConfig::default(),
            pipeline_config: PipelineConfig::default(),
            vectorized_config: VectorizedConfig::default(),
            mpp_config: MppConfig::default(),
            memory_config: MemoryConfig::default(),
            parallel_config: ParallelConfig::default(),
        }
    }
}

/// 火山模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolcanoConfig {
    /// 是否启用迭代器模型
    pub enable_iterator_model: bool,
    
    /// 是否启用物化模型
    pub enable_materialization_model: bool,
    
    /// 最大迭代器深度
    pub max_iterator_depth: usize,
    
    /// 是否启用延迟计算
    pub enable_lazy_evaluation: bool,
}

impl Default for VolcanoConfig {
    fn default() -> Self {
        Self {
            enable_iterator_model: true,
            enable_materialization_model: false,
            max_iterator_depth: 10,
            enable_lazy_evaluation: true,
        }
    }
}

/// 流水线配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// 流水线阶段数
    pub pipeline_stages: usize,
    
    /// 每个阶段的最大缓冲区大小 (MB)
    pub stage_buffer_size_mb: usize,
    
    /// 是否启用背压控制
    pub enable_backpressure: bool,
    
    /// 背压阈值 (0.0-1.0)
    pub backpressure_threshold: f64,
    
    /// 是否启用流水线并行化
    pub enable_pipeline_parallelism: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            pipeline_stages: 4,
            stage_buffer_size_mb: 64,
            enable_backpressure: true,
            backpressure_threshold: 0.8,
            enable_pipeline_parallelism: true,
        }
    }
}

/// 向量化配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorizedConfig {
    /// 向量大小
    pub vector_size: usize,
    
    /// 是否启用 SIMD 优化
    pub enable_simd: bool,
    
    /// 是否启用向量化连接
    pub enable_vectorized_join: bool,
    
    /// 是否启用向量化聚合
    pub enable_vectorized_aggregation: bool,
    
    /// 是否启用向量化排序
    pub enable_vectorized_sort: bool,
    
    /// 向量化阈值（行数）
    pub vectorization_threshold: usize,
}

impl Default for VectorizedConfig {
    fn default() -> Self {
        Self {
            vector_size: 1024,
            enable_simd: true,
            enable_vectorized_join: true,
            enable_vectorized_aggregation: true,
            enable_vectorized_sort: true,
            vectorization_threshold: 1000,
        }
    }
}

/// MPP 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MppConfig {
    /// 是否启用 MPP 执行
    pub enable_mpp_execution: bool,
    
    /// 节点数量
    pub node_count: usize,
    
    /// 每个节点的并行度
    pub parallelism_per_node: usize,
    
    /// 是否启用数据分片
    pub enable_data_sharding: bool,
    
    /// 分片策略
    pub sharding_strategy: ShardingStrategy,
    
    /// 网络配置
    pub network_config: NetworkConfig,
}

impl Default for MppConfig {
    fn default() -> Self {
        Self {
            enable_mpp_execution: true,
            node_count: 4,
            parallelism_per_node: 4,
            enable_data_sharding: true,
            sharding_strategy: ShardingStrategy::Hash,
            network_config: NetworkConfig::default(),
        }
    }
}

/// 分片策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardingStrategy {
    Hash,
    Range,
    RoundRobin,
    ConsistentHash,
}

/// 网络配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// 网络超时时间（秒）
    pub network_timeout_seconds: u64,
    
    /// 最大网络重试次数
    pub max_network_retries: usize,
    
    /// 网络缓冲区大小 (MB)
    pub network_buffer_size_mb: usize,
    
    /// 是否启用网络压缩
    pub enable_network_compression: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            network_timeout_seconds: 30,
            max_network_retries: 3,
            network_buffer_size_mb: 16,
            enable_network_compression: true,
        }
    }
}

/// 内存配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// 工作内存大小 (MB)
    pub work_memory_mb: usize,
    
    /// 共享内存大小 (MB)
    pub shared_memory_mb: usize,
    
    /// 最大查询内存 (MB)
    pub max_query_memory_mb: usize,
    
    /// 内存分配策略
    pub allocation_strategy: MemoryAllocationStrategy,
    
    /// 是否启用内存监控
    pub enable_memory_monitoring: bool,
    
    /// 内存使用阈值 (0.0-1.0)
    pub memory_usage_threshold: f64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            work_memory_mb: 64,
            shared_memory_mb: 256,
            max_query_memory_mb: 1024,
            allocation_strategy: MemoryAllocationStrategy::Dynamic,
            enable_memory_monitoring: true,
            memory_usage_threshold: 0.8,
        }
    }
}

/// 内存分配策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryAllocationStrategy {
    Static,
    Dynamic,
    Pooled,
}

/// 并行配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelConfig {
    /// 是否启用并行执行
    pub enable_parallel_execution: bool,
    
    /// 最大并行度
    pub max_parallelism: usize,
    
    /// 并行度策略
    pub parallelism_strategy: ParallelismStrategy,
    
    /// 是否启用自适应并行度
    pub enable_adaptive_parallelism: bool,
    
    /// 并行度调整间隔（秒）
    pub parallelism_adjustment_interval: u64,
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            enable_parallel_execution: true,
            max_parallelism: 8,
            parallelism_strategy: ParallelismStrategy::CPU,
            enable_adaptive_parallelism: true,
            parallelism_adjustment_interval: 60,
        }
    }
}

/// 并行度策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParallelismStrategy {
    CPU,
    IO,
    Hybrid,
    Fixed,
}

/// 执行器配置管理器
#[derive(Debug, Clone)]
pub struct ExecutorConfigManager {
    config: ExecutorConfig,
}

impl ExecutorConfigManager {
    pub fn new() -> Self {
        Self {
            config: ExecutorConfig::default(),
        }
    }
    
    pub fn with_config(config: ExecutorConfig) -> Self {
        Self { config }
    }
    
    /// 从文件加载配置
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: ExecutorConfig = serde_yaml::from_str(&content)?;
        Ok(Self { config })
    }
    
    /// 保存配置到文件
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = serde_yaml::to_string(&self.config)?;
        std::fs::write(path, content)?;
        Ok(())
    }
    
    /// 获取配置
    pub fn get_config(&self) -> &ExecutorConfig {
        &self.config
    }
    
    /// 更新配置
    pub fn update_config(&mut self, config: ExecutorConfig) {
        self.config = config;
    }
    
    /// 验证配置
    pub fn validate(&self) -> Result<(), String> {
        if self.config.volcano_config.max_iterator_depth == 0 {
            return Err("max_iterator_depth must be greater than 0".to_string());
        }
        
        if self.config.pipeline_config.pipeline_stages == 0 {
            return Err("pipeline_stages must be greater than 0".to_string());
        }
        
        if self.config.vectorized_config.vector_size == 0 {
            return Err("vector_size must be greater than 0".to_string());
        }
        
        if self.config.mpp_config.node_count == 0 {
            return Err("node_count must be greater than 0".to_string());
        }
        
        if self.config.memory_config.work_memory_mb == 0 {
            return Err("work_memory_mb must be greater than 0".to_string());
        }
        
        if self.config.parallel_config.max_parallelism == 0 {
            return Err("max_parallelism must be greater than 0".to_string());
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_executor_config_default() {
        let config = ExecutorConfig::default();
        assert!(config.enable_volcano_executor);
        assert!(config.enable_pipeline_executor);
        assert!(config.enable_vectorized_executor);
        assert!(config.enable_mpp_executor);
    }
    
    #[test]
    fn test_config_manager() {
        let manager = ExecutorConfigManager::new();
        let config = manager.get_config();
        assert!(config.enable_volcano_executor);
        assert!(config.enable_pipeline_executor);
    }
    
    #[test]
    fn test_config_validation() {
        let manager = ExecutorConfigManager::new();
        assert!(manager.validate().is_ok());
    }
} 
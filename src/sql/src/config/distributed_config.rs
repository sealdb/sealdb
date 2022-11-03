//! SealDB 分布式配置
//!
//! 管理分布式执行相关的配置参数

use serde::{Deserialize, Serialize};

/// 分布式配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedConfig {
    /// 是否启用分布式执行
    pub enable_distributed_execution: bool,

    /// 节点管理配置
    pub node_config: NodeConfig,

    /// 分片配置
    pub sharding_config: ShardingConfig,

    /// 事务配置
    pub transaction_config: TransactionConfig,

    /// 网络配置
    pub network_config: DistributedNetworkConfig,

    /// 一致性配置
    pub consistency_config: ConsistencyConfig,
}

impl Default for DistributedConfig {
    fn default() -> Self {
        Self {
            enable_distributed_execution: true,
            node_config: NodeConfig::default(),
            sharding_config: ShardingConfig::default(),
            transaction_config: TransactionConfig::default(),
            network_config: DistributedNetworkConfig::default(),
            consistency_config: ConsistencyConfig::default(),
        }
    }
}

/// 节点配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// 节点 ID
    pub node_id: String,

    /// 节点类型
    pub node_type: NodeType,

    /// 节点地址
    pub node_address: String,

    /// 节点端口
    pub node_port: u16,

    /// 节点权重
    pub node_weight: f64,

    /// 是否启用节点监控
    pub enable_node_monitoring: bool,

    /// 节点心跳间隔（秒）
    pub heartbeat_interval_seconds: u64,

    /// 节点超时时间（秒）
    pub node_timeout_seconds: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: "node-1".to_string(),
            node_type: NodeType::Data,
            node_address: "localhost".to_string(),
            node_port: 5432,
            node_weight: 1.0,
            enable_node_monitoring: true,
            heartbeat_interval_seconds: 30,
            node_timeout_seconds: 120,
        }
    }
}

/// 节点类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeType {
    Coordinator,
    Data,
    Compute,
    Storage,
}

/// 分片配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardingConfig {
    /// 是否启用数据分片
    pub enable_data_sharding: bool,

    /// 分片策略
    pub sharding_strategy: DistributedShardingStrategy,

    /// 分片数量
    pub shard_count: usize,

    /// 分片键
    pub shard_key: String,

    /// 是否启用分片再平衡
    pub enable_shard_rebalancing: bool,

    /// 再平衡阈值
    pub rebalancing_threshold: f64,

    /// 是否启用分片监控
    pub enable_shard_monitoring: bool,
}

impl Default for ShardingConfig {
    fn default() -> Self {
        Self {
            enable_data_sharding: true,
            sharding_strategy: DistributedShardingStrategy::Hash,
            shard_count: 4,
            shard_key: "id".to_string(),
            enable_shard_rebalancing: true,
            rebalancing_threshold: 0.2,
            enable_shard_monitoring: true,
        }
    }
}

/// 分布式分片策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DistributedShardingStrategy {
    Hash,
    Range,
    RoundRobin,
    ConsistentHash,
    Directory,
}

/// 事务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionConfig {
    /// 是否启用分布式事务
    pub enable_distributed_transactions: bool,

    /// 事务隔离级别
    pub isolation_level: IsolationLevel,

    /// 事务超时时间（秒）
    pub transaction_timeout_seconds: u64,

    /// 是否启用两阶段提交
    pub enable_two_phase_commit: bool,

    /// 是否启用事务监控
    pub enable_transaction_monitoring: bool,

    /// 最大事务重试次数
    pub max_transaction_retries: usize,

    /// 死锁检测间隔（秒）
    pub deadlock_detection_interval: u64,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            enable_distributed_transactions: true,
            isolation_level: IsolationLevel::ReadCommitted,
            transaction_timeout_seconds: 30,
            enable_two_phase_commit: true,
            enable_transaction_monitoring: true,
            max_transaction_retries: 3,
            deadlock_detection_interval: 10,
        }
    }
}

/// 事务隔离级别
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// 分布式网络配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedNetworkConfig {
    /// 网络超时时间（秒）
    pub network_timeout_seconds: u64,

    /// 最大网络重试次数
    pub max_network_retries: usize,

    /// 网络缓冲区大小 (MB)
    pub network_buffer_size_mb: usize,

    /// 是否启用网络压缩
    pub enable_network_compression: bool,

    /// 是否启用网络加密
    pub enable_network_encryption: bool,

    /// 网络连接池大小
    pub connection_pool_size: usize,

    /// 连接空闲超时时间（秒）
    pub connection_idle_timeout_seconds: u64,
}

impl Default for DistributedNetworkConfig {
    fn default() -> Self {
        Self {
            network_timeout_seconds: 30,
            max_network_retries: 3,
            network_buffer_size_mb: 16,
            enable_network_compression: true,
            enable_network_encryption: false,
            connection_pool_size: 100,
            connection_idle_timeout_seconds: 300,
        }
    }
}

/// 一致性配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyConfig {
    /// 一致性级别
    pub consistency_level: ConsistencyLevel,

    /// 是否启用强一致性
    pub enable_strong_consistency: bool,

    /// 是否启用最终一致性
    pub enable_eventual_consistency: bool,

    /// 复制因子
    pub replication_factor: usize,

    /// 是否启用故障检测
    pub enable_failure_detection: bool,

    /// 故障检测间隔（秒）
    pub failure_detection_interval: u64,

    /// 是否启用自动故障恢复
    pub enable_auto_failure_recovery: bool,
}

impl Default for ConsistencyConfig {
    fn default() -> Self {
        Self {
            consistency_level: ConsistencyLevel::Strong,
            enable_strong_consistency: true,
            enable_eventual_consistency: false,
            replication_factor: 3,
            enable_failure_detection: true,
            failure_detection_interval: 10,
            enable_auto_failure_recovery: true,
        }
    }
}

/// 一致性级别
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConsistencyLevel {
    Strong,
    Eventual,
    BoundedStaleness,
    Session,
}

/// 分布式配置管理器
#[derive(Debug, Clone)]
pub struct DistributedConfigManager {
    config: DistributedConfig,
}

impl DistributedConfigManager {
    pub fn new() -> Self {
        Self {
            config: DistributedConfig::default(),
        }
    }

    pub fn with_config(config: DistributedConfig) -> Self {
        Self { config }
    }

    /// 从文件加载配置
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: DistributedConfig = serde_yaml::from_str(&content)?;
        Ok(Self { config })
    }

    /// 保存配置到文件
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = serde_yaml::to_string(&self.config)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 获取配置
    pub fn get_config(&self) -> &DistributedConfig {
        &self.config
    }

    /// 更新配置
    pub fn update_config(&mut self, config: DistributedConfig) {
        self.config = config;
    }

    /// 验证配置
    pub fn validate(&self) -> Result<(), String> {
        if self.config.sharding_config.shard_count == 0 {
            return Err("shard_count must be greater than 0".to_string());
        }

        if self.config.transaction_config.transaction_timeout_seconds == 0 {
            return Err("transaction_timeout_seconds must be greater than 0".to_string());
        }

        if self.config.network_config.network_timeout_seconds == 0 {
            return Err("network_timeout_seconds must be greater than 0".to_string());
        }

        if self.config.consistency_config.replication_factor == 0 {
            return Err("replication_factor must be greater than 0".to_string());
        }

        if self.config.node_config.node_port == 0 {
            return Err("node_port must be greater than 0".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distributed_config_default() {
        let config = DistributedConfig::default();
        assert!(config.enable_distributed_execution);
        assert!(config.sharding_config.enable_data_sharding);
        assert!(config.transaction_config.enable_distributed_transactions);
        assert!(config.consistency_config.enable_strong_consistency);
    }

    #[test]
    fn test_config_manager() {
        let manager = DistributedConfigManager::new();
        let config = manager.get_config();
        assert!(config.enable_distributed_execution);
        assert!(config.sharding_config.enable_data_sharding);
    }

    #[test]
    fn test_config_validation() {
        let manager = DistributedConfigManager::new();
        assert!(manager.validate().is_ok());
    }
}
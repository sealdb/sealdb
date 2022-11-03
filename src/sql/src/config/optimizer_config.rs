//! SealDB 优化器配置
//!
//! 管理基于规则优化 (RBO) 和基于成本优化 (CBO) 的配置参数

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 优化器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizerConfig {
    /// 是否启用基于规则的优化 (RBO)
    pub enable_rbo: bool,

    /// 是否启用基于成本的优化 (CBO)
    pub enable_cbo: bool,

    /// RBO 配置
    pub rbo_config: RboConfig,

    /// CBO 配置
    pub cbo_config: CboConfig,

    /// 统计信息配置
    pub statistics_config: StatisticsConfig,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            enable_rbo: true,
            enable_cbo: true,
            rbo_config: RboConfig::default(),
            cbo_config: CboConfig::default(),
            statistics_config: StatisticsConfig::default(),
        }
    }
}

/// 基于规则优化 (RBO) 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RboConfig {
    /// 是否启用常量折叠
    pub enable_constant_folding: bool,

    /// 是否启用表达式简化
    pub enable_expression_simplification: bool,

    /// 是否启用子查询扁平化
    pub enable_subquery_flattening: bool,

    /// 是否启用谓词下推
    pub enable_predicate_pushdown: bool,

    /// 是否启用列裁剪
    pub enable_column_pruning: bool,

    /// 是否启用连接重排序
    pub enable_join_reordering: bool,

    /// 是否启用索引选择
    pub enable_index_selection: bool,

    /// 是否启用排序优化
    pub enable_order_by_optimization: bool,

    /// 是否启用分组优化
    pub enable_group_by_optimization: bool,

    /// 是否启用去重优化
    pub enable_distinct_optimization: bool,

    /// 是否启用限制优化
    pub enable_limit_optimization: bool,

    /// 是否启用联合优化
    pub enable_union_optimization: bool,

    /// 优化规则的最大应用次数
    pub max_rule_applications: usize,
}

impl Default for RboConfig {
    fn default() -> Self {
        Self {
            enable_constant_folding: true,
            enable_expression_simplification: true,
            enable_subquery_flattening: true,
            enable_predicate_pushdown: true,
            enable_column_pruning: true,
            enable_join_reordering: true,
            enable_index_selection: true,
            enable_order_by_optimization: true,
            enable_group_by_optimization: true,
            enable_distinct_optimization: true,
            enable_limit_optimization: true,
            enable_union_optimization: true,
            max_rule_applications: 10,
        }
    }
}

/// 基于成本优化 (CBO) 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CboConfig {
    /// 是否启用连接重排序
    pub enable_join_reordering: bool,

    /// 是否启用索引选择
    pub enable_index_selection: bool,

    /// 是否启用聚合优化
    pub enable_aggregation_optimization: bool,

    /// 是否启用排序优化
    pub enable_sorting_optimization: bool,

    /// 是否启用并行化优化
    pub enable_parallelization_optimization: bool,

    /// 每个组保留的最大计划数
    pub max_plans_per_group: usize,

    /// 最大搜索深度
    pub max_search_depth: usize,

    /// 成本模型配置
    pub cost_model: CostModelConfig,
}

impl Default for CboConfig {
    fn default() -> Self {
        Self {
            enable_join_reordering: true,
            enable_index_selection: true,
            enable_aggregation_optimization: true,
            enable_sorting_optimization: true,
            enable_parallelization_optimization: true,
            max_plans_per_group: 100,
            max_search_depth: 10,
            cost_model: CostModelConfig::default(),
        }
    }
}

/// 成本模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostModelConfig {
    /// CPU 成本参数
    pub cpu_tuple_cost: f64,
    pub cpu_index_tuple_cost: f64,
    pub cpu_operator_cost: f64,

    /// I/O 成本参数
    pub seq_page_cost: f64,
    pub random_page_cost: f64,
    pub cpu_page_cost: f64,

    /// 网络成本参数（分布式环境）
    pub network_cost_per_byte: f64,
    pub network_latency: f64,

    /// 内存成本参数
    pub memory_cost_per_mb: f64,

    /// 并行度参数
    pub parallel_worker_cost: f64,
    pub parallel_setup_cost: f64,
}

impl Default for CostModelConfig {
    fn default() -> Self {
        Self {
            // PostgreSQL 默认值
            cpu_tuple_cost: 0.01,
            cpu_index_tuple_cost: 0.005,
            cpu_operator_cost: 0.0025,
            seq_page_cost: 1.0,
            random_page_cost: 4.0,
            cpu_page_cost: 0.1,

            // 分布式环境参数
            network_cost_per_byte: 0.000001,
            network_latency: 0.1,
            memory_cost_per_mb: 0.01,
            parallel_worker_cost: 0.1,
            parallel_setup_cost: 1000.0,
        }
    }
}

/// 统计信息配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatisticsConfig {
    /// 是否启用统计信息收集
    pub enable_statistics_collection: bool,

    /// 统计信息自动更新间隔（秒）
    pub auto_update_interval: u64,

    /// 统计信息采样比例 (0.0-1.0)
    pub sample_ratio: f64,

    /// 直方图桶数
    pub histogram_buckets: usize,

    /// 最大统计信息大小 (MB)
    pub max_statistics_size_mb: usize,

    /// 统计信息过期时间（秒）
    pub statistics_ttl_seconds: u64,
}

impl Default for StatisticsConfig {
    fn default() -> Self {
        Self {
            enable_statistics_collection: true,
            auto_update_interval: 3600, // 1小时
            sample_ratio: 0.1, // 10% 采样
            histogram_buckets: 100,
            max_statistics_size_mb: 100,
            statistics_ttl_seconds: 86400, // 24小时
        }
    }
}

/// 优化器配置管理器
#[derive(Debug, Clone)]
pub struct OptimizerConfigManager {
    config: OptimizerConfig,
}

impl OptimizerConfigManager {
    pub fn new() -> Self {
        Self {
            config: OptimizerConfig::default(),
        }
    }

    pub fn with_config(config: OptimizerConfig) -> Self {
        Self { config }
    }

    /// 从文件加载配置
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: OptimizerConfig = serde_yaml::from_str(&content)?;
        Ok(Self { config })
    }

    /// 保存配置到文件
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = serde_yaml::to_string(&self.config)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 获取配置
    pub fn get_config(&self) -> &OptimizerConfig {
        &self.config
    }

    /// 更新配置
    pub fn update_config(&mut self, config: OptimizerConfig) {
        self.config = config;
    }

    /// 验证配置
    pub fn validate(&self) -> Result<(), String> {
        if self.config.cbo_config.max_plans_per_group == 0 {
            return Err("max_plans_per_group must be greater than 0".to_string());
        }

        if self.config.cbo_config.max_search_depth == 0 {
            return Err("max_search_depth must be greater than 0".to_string());
        }

        if self.config.rbo_config.max_rule_applications == 0 {
            return Err("max_rule_applications must be greater than 0".to_string());
        }

        if self.config.statistics_config.sample_ratio <= 0.0 || self.config.statistics_config.sample_ratio > 1.0 {
            return Err("sample_ratio must be between 0.0 and 1.0".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimizer_config_default() {
        let config = OptimizerConfig::default();
        assert!(config.enable_rbo);
        assert!(config.enable_cbo);
        assert!(config.rbo_config.enable_constant_folding);
        assert!(config.cbo_config.enable_join_reordering);
    }

    #[test]
    fn test_config_manager() {
        let manager = OptimizerConfigManager::new();
        let config = manager.get_config();
        assert!(config.enable_rbo);
        assert!(config.enable_cbo);
    }

    #[test]
    fn test_config_validation() {
        let manager = OptimizerConfigManager::new();
        assert!(manager.validate().is_ok());
    }
}
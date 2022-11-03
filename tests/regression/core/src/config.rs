//! 配置模块
//!
//! 负责加载和管理测试框架的配置

use std::collections::HashMap;
use std::path::Path;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn, error};

use crate::{TestConfig, DatabaseConfig, ValidationConfig, LoggingConfig, PerformanceThreshold};

/// 配置管理器
pub struct ConfigManager {
    config: TestConfig,
    config_path: String,
}

impl ConfigManager {
    /// 创建新的配置管理器
    pub fn new(config_path: &str) -> Result<Self> {
        let config = Self::load_config(config_path)?;
        Ok(Self {
            config,
            config_path: config_path.to_string(),
        })
    }

    /// 加载配置文件
    pub fn load_config(config_path: &str) -> Result<TestConfig> {
        if !Path::new(config_path).exists() {
            warn!("配置文件不存在: {}，使用默认配置", config_path);
            return Ok(TestConfig::default());
        }

        let config_content = std::fs::read_to_string(config_path)?;
        let config: TestConfig = serde_yaml::from_str(&config_content)?;

        debug!("成功加载配置文件: {}", config_path);
        Ok(config)
    }

    /// 保存配置文件
    pub fn save_config(&self, config_path: &str) -> Result<()> {
        let yaml = serde_yaml::to_string(&self.config)?;
        std::fs::write(config_path, yaml)?;

        debug!("成功保存配置文件: {}", config_path);
        Ok(())
    }

    /// 获取配置
    pub fn get_config(&self) -> &TestConfig {
        &self.config
    }

    /// 获取可变配置
    pub fn get_config_mut(&mut self) -> &mut TestConfig {
        &mut self.config
    }

    /// 验证配置
    pub fn validate_config(&self) -> Result<Vec<String>> {
        let mut errors = Vec::new();

        // 验证数据库配置
        if let Err(e) = self.validate_database_config(&self.config.database) {
            errors.push(format!("数据库配置错误: {}", e));
        }

        // 验证测试套件配置
        for (suite_name, suite) in &self.config.test_suites {
            if let Err(e) = self.validate_test_suite_config(suite_name, suite) {
                errors.push(format!("测试套件 '{}' 配置错误: {}", suite_name, e));
            }
        }

        // 验证性能阈值配置
        if let Err(e) = self.validate_performance_config(&self.config.performance_thresholds) {
            errors.push(format!("性能阈值配置错误: {}", e));
        }

        Ok(errors)
    }

    /// 验证数据库配置
    fn validate_database_config(&self, config: &DatabaseConfig) -> Result<()> {
        if config.host.is_empty() {
            return Err(anyhow::anyhow!("数据库主机不能为空"));
        }

        if config.port == 0 {
            return Err(anyhow::anyhow!("数据库端口不能为 0"));
        }

        if config.username.is_empty() {
            return Err(anyhow::anyhow!("数据库用户名不能为空"));
        }

        if config.database.is_empty() {
            return Err(anyhow::anyhow!("数据库名不能为空"));
        }

        if config.connection_timeout == 0 {
            return Err(anyhow::anyhow!("连接超时时间不能为 0"));
        }

        if config.query_timeout == 0 {
            return Err(anyhow::anyhow!("查询超时时间不能为 0"));
        }

        if config.max_connections == 0 {
            return Err(anyhow::anyhow!("最大连接数不能为 0"));
        }

        Ok(())
    }

    /// 验证测试套件配置
    fn validate_test_suite_config(&self, suite_name: &str, suite: &crate::TestSuite) -> Result<()> {
        if suite.name.is_empty() {
            return Err(anyhow::anyhow!("套件名称不能为空"));
        }

        if suite.test_cases.is_empty() {
            warn!("测试套件 '{}' 没有测试用例", suite_name);
        }

        if suite.timeout_seconds == 0 {
            return Err(anyhow::anyhow!("套件超时时间不能为 0"));
        }

        Ok(())
    }

    /// 验证性能配置
    fn validate_performance_config(&self, config: &PerformanceThreshold) -> Result<()> {
        if config.max_execution_time_ms == 0 {
            return Err(anyhow::anyhow!("最大执行时间不能为 0"));
        }

        if config.min_throughput_qps < 0.0 {
            return Err(anyhow::anyhow!("最小吞吐量不能为负数"));
        }

        if config.max_memory_usage_mb <= 0.0 {
            return Err(anyhow::anyhow!("最大内存使用必须大于 0"));
        }

        if config.max_cpu_usage_percent <= 0.0 || config.max_cpu_usage_percent > 100.0 {
            return Err(anyhow::anyhow!("最大 CPU 使用率必须在 0-100% 之间"));
        }

        Ok(())
    }

    /// 生成默认配置文件
    pub fn generate_default_config() -> String {
        let config = TestConfig::default();
        serde_yaml::to_string(&config).unwrap()
    }

    /// 合并配置
    pub fn merge_config(&mut self, other: TestConfig) {
        // 合并数据库配置
        if other.database.host != "localhost" {
            self.config.database.host = other.database.host;
        }
        if other.database.port != 4000 {
            self.config.database.port = other.database.port;
        }
        if other.database.username != "root" {
            self.config.database.username = other.database.username;
        }
        if other.database.password != "" {
            self.config.database.password = other.database.password;
        }
        if other.database.database != "test" {
            self.config.database.database = other.database.database;
        }

        // 合并测试套件
        for (suite_name, suite) in other.test_suites {
            self.config.test_suites.insert(suite_name, suite);
        }

        // 合并性能阈值
        if other.performance_thresholds.max_execution_time_ms != 1000 {
            self.config.performance_thresholds.max_execution_time_ms = other.performance_thresholds.max_execution_time_ms;
        }
        if other.performance_thresholds.min_throughput_qps != 1000.0 {
            self.config.performance_thresholds.min_throughput_qps = other.performance_thresholds.min_throughput_qps;
        }
        if other.performance_thresholds.max_memory_usage_mb != 512.0 {
            self.config.performance_thresholds.max_memory_usage_mb = other.performance_thresholds.max_memory_usage_mb;
        }
        if other.performance_thresholds.max_cpu_usage_percent != 80.0 {
            self.config.performance_thresholds.max_cpu_usage_percent = other.performance_thresholds.max_cpu_usage_percent;
        }
    }

    /// 获取启用的测试套件
    pub fn get_enabled_suites(&self) -> Vec<&str> {
        self.config.test_suites
            .iter()
            .filter(|(_, suite)| suite.enabled)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// 获取测试套件统计信息
    pub fn get_suite_statistics(&self) -> SuiteStatistics {
        let total_suites = self.config.test_suites.len();
        let enabled_suites = self.config.test_suites.values().filter(|s| s.enabled).count();
        let total_test_cases: usize = self.config.test_suites.values()
            .map(|suite| suite.test_cases.len())
            .sum();
        let enabled_test_cases: usize = self.config.test_suites.values()
            .flat_map(|suite| &suite.test_cases)
            .filter(|test_case| test_case.enabled)
            .count();

        SuiteStatistics {
            total_suites,
            enabled_suites,
            total_test_cases,
            enabled_test_cases,
        }
    }
}

/// 测试套件统计信息
#[derive(Debug)]
pub struct SuiteStatistics {
    pub total_suites: usize,
    pub enabled_suites: usize,
    pub total_test_cases: usize,
    pub enabled_test_cases: usize,
}

/// 配置验证器
pub struct ConfigValidator;

impl ConfigValidator {
    /// 验证配置文件格式
    pub fn validate_yaml_format(content: &str) -> Result<()> {
        let _config: TestConfig = serde_yaml::from_str(content)?;
        Ok(())
    }

    /// 检查必需的配置项
    pub fn check_required_fields(config: &TestConfig) -> Result<Vec<String>> {
        let mut missing_fields = Vec::new();

        if config.database.host.is_empty() {
            missing_fields.push("database.host".to_string());
        }

        if config.database.username.is_empty() {
            missing_fields.push("database.username".to_string());
        }

        if config.database.database.is_empty() {
            missing_fields.push("database.database".to_string());
        }

        Ok(missing_fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_config() {
        let config = TestConfig::default();
        assert_eq!(config.database.host, "localhost");
        assert_eq!(config.database.port, 4000);
        assert_eq!(config.database.username, "root");
    }

    #[test]
    fn test_validate_database_config() {
        let manager = ConfigManager::new("nonexistent.yaml").unwrap();

        let valid_config = DatabaseConfig {
            host: "localhost".to_string(),
            port: 4000,
            username: "root".to_string(),
            password: "".to_string(),
            database: "test".to_string(),
            connection_timeout: 30,
            query_timeout: 60,
            max_connections: 10,
        };

        let result = manager.validate_database_config(&valid_config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_generate_default_config() {
        let yaml = ConfigManager::generate_default_config();
        assert!(yaml.contains("database:"));
        assert!(yaml.contains("test_suites:"));
        assert!(yaml.contains("performance_thresholds:"));
    }

    #[test]
    fn test_get_enabled_suites() {
        let manager = ConfigManager::new("nonexistent.yaml").unwrap();
        let enabled_suites = manager.get_enabled_suites();
        assert_eq!(enabled_suites.len(), 0); // 默认配置没有测试套件
    }
}
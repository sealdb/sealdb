//! SealDB 测试框架核心库
//!
//! 提供测试执行、结果验证、性能监控等核心功能

pub mod test_runner;
pub mod result_checker;
pub mod performance_monitor;
pub mod report_generator;
pub mod database_connection;
pub mod test_case;
pub mod config;
pub mod test_framework;

use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use anyhow::Result;

/// 测试结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// 测试名称
    pub test_name: String,
    /// 测试套件名称
    pub suite_name: String,
    /// 是否通过
    pub passed: bool,
    /// 执行时间
    pub execution_time: Duration,
    /// 实际结果
    pub actual_result: Option<QueryResult>,
    /// 期望结果
    pub expected_result: Option<ExpectedResult>,
    /// 错误信息
    pub error_message: Option<String>,
    /// 性能指标
    pub performance_metrics: Option<PerformanceMetrics>,
}

/// 查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// 查询语句
    pub sql: String,
    /// 结果数据
    pub data: Vec<Vec<String>>,
    /// 列名
    pub columns: Vec<String>,
    /// 行数
    pub row_count: usize,
    /// 执行时间 (毫秒)
    pub execution_time_ms: u64,
    /// 错误信息
    pub error: Option<String>,
}

/// 期望结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedResult {
    /// 验证类型
    pub validation_type: ValidationType,
    /// 期望数据
    pub data: Option<Vec<Vec<String>>>,
    /// 期望列名
    pub columns: Option<Vec<String>>,
    /// 期望行数
    pub row_count: Option<usize>,
    /// 性能阈值
    pub performance_threshold: Option<PerformanceThreshold>,
    /// 模式匹配 (正则表达式)
    pub pattern: Option<String>,
}

/// 验证类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidationType {
    /// 精确匹配
    ExactMatch,
    /// 模糊匹配 (允许误差)
    FuzzyMatch,
    /// 模式匹配 (正则表达式)
    PatternMatch,
    /// 性能阈值检查
    PerformanceThreshold,
    /// 错误检查
    ErrorCheck,
}

/// 性能指标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    /// 执行时间 (毫秒)
    pub execution_time_ms: u64,
    /// 内存使用 (MB)
    pub memory_usage_mb: f64,
    /// CPU 使用率 (%)
    pub cpu_usage_percent: f64,
    /// 吞吐量 (QPS)
    pub throughput_qps: f64,
    /// 网络 I/O (KB)
    pub network_io_kb: f64,
    /// 磁盘 I/O (KB)
    pub disk_io_kb: f64,
}

/// 性能阈值
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceThreshold {
    /// 最大执行时间 (毫秒)
    pub max_execution_time_ms: u64,
    /// 最小吞吐量 (QPS)
    pub min_throughput_qps: f64,
    /// 最大内存使用 (MB)
    pub max_memory_usage_mb: f64,
    /// 最大 CPU 使用率 (%)
    pub max_cpu_usage_percent: f64,
}

/// 测试用例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    /// 测试名称
    pub name: String,
    /// 测试描述
    pub description: String,
    /// SQL 语句
    pub sql: String,
    /// 期望结果
    pub expected_result: ExpectedResult,
    /// 标签
    pub tags: Vec<String>,
    /// 超时时间 (秒)
    pub timeout_seconds: u64,
    /// 是否启用
    pub enabled: bool,
}

/// 测试套件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSuite {
    /// 套件名称
    pub name: String,
    /// 套件描述
    pub description: String,
    /// 测试用例列表
    pub test_cases: Vec<TestCase>,
    /// 是否启用
    pub enabled: bool,
    /// 并行执行
    pub parallel: bool,
    /// 重试次数
    pub retry_count: u32,
    /// 超时时间 (秒)
    pub timeout_seconds: u64,
}

/// 测试配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfig {
    /// 数据库连接配置
    pub database: DatabaseConfig,
    /// 测试套件配置
    pub test_suites: std::collections::HashMap<String, TestSuite>,
    /// 结果验证配置
    pub result_validation: ValidationConfig,
    /// 性能阈值配置
    pub performance_thresholds: PerformanceThreshold,
    /// 日志配置
    pub logging: LoggingConfig,
}

/// 数据库配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// 主机地址
    pub host: String,
    /// 端口
    pub port: u16,
    /// 用户名
    pub username: String,
    /// 密码
    pub password: String,
    /// 数据库名
    pub database: String,
    /// 连接超时 (秒)
    pub connection_timeout: u64,
    /// 查询超时 (秒)
    pub query_timeout: u64,
    /// 最大连接数
    pub max_connections: u32,
}

/// 验证配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// 精确匹配
    pub exact_match: bool,
    /// 大小写敏感
    pub case_sensitive: bool,
    /// 忽略空白字符
    pub ignore_whitespace: bool,
    /// 容差
    pub tolerance: f64,
    /// 最大差异行数
    pub max_diff_rows: usize,
}

/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// 日志级别
    pub level: String,
    /// 日志文件路径
    pub file_path: Option<String>,
    /// 是否输出到控制台
    pub console: bool,
    /// 是否包含性能指标
    pub include_performance: bool,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            database: DatabaseConfig {
                host: "localhost".to_string(),
                port: 4000,
                username: "root".to_string(),
                password: "".to_string(),
                database: "test".to_string(),
                connection_timeout: 30,
                query_timeout: 60,
                max_connections: 10,
            },
            test_suites: std::collections::HashMap::new(),
            result_validation: ValidationConfig {
                exact_match: true,
                case_sensitive: false,
                ignore_whitespace: true,
                tolerance: 0.01,
                max_diff_rows: 10,
            },
            performance_thresholds: PerformanceThreshold {
                max_execution_time_ms: 1000,
                min_throughput_qps: 1000.0,
                max_memory_usage_mb: 512.0,
                max_cpu_usage_percent: 80.0,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                file_path: Some("test.log".to_string()),
                console: true,
                include_performance: true,
            },
        }
    }
}

/// 测试统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStatistics {
    /// 总测试数
    pub total_tests: u32,
    /// 通过测试数
    pub passed_tests: u32,
    /// 失败测试数
    pub failed_tests: u32,
    /// 跳过测试数
    pub skipped_tests: u32,
    /// 总执行时间
    pub total_execution_time: Duration,
    /// 平均执行时间
    pub average_execution_time: Duration,
    /// 通过率
    pub pass_rate: f64,
    /// 性能统计
    pub performance_stats: Option<PerformanceStatistics>,
}

/// 性能统计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceStatistics {
    /// 平均执行时间 (毫秒)
    pub avg_execution_time_ms: f64,
    /// 最大执行时间 (毫秒)
    pub max_execution_time_ms: u64,
    /// 最小执行时间 (毫秒)
    pub min_execution_time_ms: u64,
    /// 平均内存使用 (MB)
    pub avg_memory_usage_mb: f64,
    /// 平均 CPU 使用率 (%)
    pub avg_cpu_usage_percent: f64,
    /// 平均吞吐量 (QPS)
    pub avg_throughput_qps: f64,
}

/// 计算测试统计信息
pub fn calculate_statistics(results: &[TestResult]) -> TestStatistics {
    let total_tests = results.len() as u32;
    let passed_tests = results.iter().filter(|r| r.passed).count() as u32;
    let failed_tests = results.iter().filter(|r| !r.passed).count() as u32;
    let skipped_tests = 0u32; // 暂时不支持跳过

    let total_execution_time = results.iter()
        .map(|r| r.execution_time)
        .fold(Duration::ZERO, |acc, x| acc + x);

    let average_execution_time = if total_tests > 0 {
        Duration::from_millis(total_execution_time.as_millis() as u64 / total_tests as u64)
    } else {
        Duration::ZERO
    };

    let pass_rate = if total_tests > 0 {
        passed_tests as f64 / total_tests as f64 * 100.0
    } else {
        0.0
    };

    // 计算性能统计
    let performance_stats = if results.iter().any(|r| r.performance_metrics.is_some()) {
        let perf_metrics: Vec<&PerformanceMetrics> = results.iter()
            .filter_map(|r| r.performance_metrics.as_ref())
            .collect();

        if !perf_metrics.is_empty() {
            let avg_execution_time_ms = perf_metrics.iter()
                .map(|m| m.execution_time_ms as f64)
                .sum::<f64>() / perf_metrics.len() as f64;

            let max_execution_time_ms = perf_metrics.iter()
                .map(|m| m.execution_time_ms)
                .max()
                .unwrap_or(0);

            let min_execution_time_ms = perf_metrics.iter()
                .map(|m| m.execution_time_ms)
                .min()
                .unwrap_or(0);

            let avg_memory_usage_mb = perf_metrics.iter()
                .map(|m| m.memory_usage_mb)
                .sum::<f64>() / perf_metrics.len() as f64;

            let avg_cpu_usage_percent = perf_metrics.iter()
                .map(|m| m.cpu_usage_percent)
                .sum::<f64>() / perf_metrics.len() as f64;

            let avg_throughput_qps = perf_metrics.iter()
                .map(|m| m.throughput_qps)
                .sum::<f64>() / perf_metrics.len() as f64;

            Some(PerformanceStatistics {
                avg_execution_time_ms,
                max_execution_time_ms,
                min_execution_time_ms,
                avg_memory_usage_mb,
                avg_cpu_usage_percent,
                avg_throughput_qps,
            })
        } else {
            None
        }
    } else {
        None
    };

    TestStatistics {
        total_tests,
        passed_tests,
        failed_tests,
        skipped_tests,
        total_execution_time,
        average_execution_time,
        pass_rate,
        performance_stats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_statistics() {
        let results = vec![
            TestResult {
                test_name: "test1".to_string(),
                suite_name: "basic".to_string(),
                passed: true,
                execution_time: Duration::from_millis(100),
                actual_result: None,
                expected_result: None,
                error_message: None,
                performance_metrics: Some(PerformanceMetrics {
                    execution_time_ms: 100,
                    memory_usage_mb: 50.0,
                    cpu_usage_percent: 25.0,
                    throughput_qps: 1000.0,
                    network_io_kb: 10.0,
                    disk_io_kb: 5.0,
                }),
            },
            TestResult {
                test_name: "test2".to_string(),
                suite_name: "basic".to_string(),
                passed: false,
                execution_time: Duration::from_millis(200),
                actual_result: None,
                expected_result: None,
                error_message: Some("测试失败".to_string()),
                performance_metrics: None,
            },
        ];

        let stats = calculate_statistics(&results);

        assert_eq!(stats.total_tests, 2);
        assert_eq!(stats.passed_tests, 1);
        assert_eq!(stats.failed_tests, 1);
        assert_eq!(stats.pass_rate, 50.0);
        assert!(stats.performance_stats.is_some());
    }
}
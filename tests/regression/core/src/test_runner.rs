//! 测试运行器
//!
//! 负责执行测试用例、管理测试生命周期

use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::Semaphore;
use anyhow::{Result, anyhow};
use tracing::{info, warn, error, debug};

use crate::{
    TestConfig, TestSuite, TestCase, TestResult, QueryResult, ExpectedResult,
    PerformanceMetrics, ValidationType, PerformanceThreshold,
};
use super::database_connection::DatabaseConnection;
use super::result_checker::ResultChecker;
use super::performance_monitor::PerformanceMonitor;

/// 测试运行器
pub struct TestRunner {
    /// 测试配置
    config: TestConfig,
    /// 数据库连接
    database: DatabaseConnection,
    /// 结果检查器
    result_checker: ResultChecker,
    /// 性能监控器
    performance_monitor: PerformanceMonitor,
    /// 测试结果
    results: Vec<TestResult>,
}

impl TestRunner {
    /// 创建新的测试运行器
    pub async fn new(config: TestConfig) -> Result<Self> {
        let database = DatabaseConnection::new(&config.database).await?;
        let result_checker = ResultChecker::new(&config.result_validation);
        let performance_monitor = PerformanceMonitor::new();

        Ok(Self {
            config,
            database,
            result_checker,
            performance_monitor,
            results: Vec::new(),
        })
    }

    /// 运行所有测试套件
    pub async fn run_all_suites(&mut self) -> Result<Vec<TestResult>> {
        info!("开始运行所有测试套件");

        let mut all_results = Vec::new();

        for (suite_name, suite) in &self.config.test_suites {
            if suite.enabled {
                info!("运行测试套件: {}", suite_name);
                let suite_results = self.run_test_suite(suite_name, suite).await?;
                all_results.extend(suite_results);
            }
        }

        self.results = all_results.clone();
        info!("所有测试套件运行完成，共 {} 个测试", all_results.len());

        Ok(all_results)
    }

    /// 运行指定测试套件
    pub async fn run_test_suite(&mut self, suite_name: &str, suite: &TestSuite) -> Result<Vec<TestResult>> {
        info!("运行测试套件: {} ({} 个测试用例)", suite_name, suite.test_cases.len());

        let mut suite_results = Vec::new();

        if suite.parallel {
            // 并行执行测试用例
            suite_results = self.run_test_suite_parallel(suite_name, suite).await?;
        } else {
            // 串行执行测试用例
            suite_results = self.run_test_suite_sequential(suite_name, suite).await?;
        }

        info!("测试套件 {} 完成，通过: {}/{}",
              suite_name,
              suite_results.iter().filter(|r| r.passed).count(),
              suite_results.len());

        Ok(suite_results)
    }

    /// 串行执行测试套件
    async fn run_test_suite_sequential(&mut self, suite_name: &str, suite: &TestSuite) -> Result<Vec<TestResult>> {
        let mut results = Vec::new();

        for test_case in &suite.test_cases {
            if test_case.enabled {
                let result = self.execute_test_case(suite_name, test_case).await?;
                results.push(result);
            }
        }

        Ok(results)
    }

    /// 并行执行测试套件
    async fn run_test_suite_parallel(&mut self, suite_name: &str, suite: &TestSuite) -> Result<Vec<TestResult>> {
        let max_concurrent = std::cmp::min(suite.test_cases.len(), 10); // 最多10个并发
        let semaphore = Semaphore::new(max_concurrent);
        let mut handles = Vec::new();

        for test_case in &suite.test_cases {
            if test_case.enabled {
                let semaphore = semaphore.clone();
                let suite_name = suite_name.to_string();
                let test_case = test_case.clone();
                let database = self.database.clone();
                let result_checker = self.result_checker.clone();
                let performance_monitor = self.performance_monitor.clone();

                let handle = tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();
                    Self::execute_test_case_async(
                        &suite_name,
                        &test_case,
                        database,
                        result_checker,
                        performance_monitor,
                    ).await
                });

                handles.push(handle);
            }
        }

        // 等待所有测试完成
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(e)) => {
                    error!("测试执行失败: {}", e);
                    // 创建一个失败的测试结果
                    results.push(TestResult {
                        test_name: "unknown".to_string(),
                        suite_name: suite_name.to_string(),
                        passed: false,
                        execution_time: Duration::ZERO,
                        actual_result: None,
                        expected_result: None,
                        error_message: Some(e.to_string()),
                        performance_metrics: None,
                    });
                }
                Err(e) => {
                    error!("任务执行失败: {}", e);
                }
            }
        }

        Ok(results)
    }

    /// 执行单个测试用例
    async fn execute_test_case(&mut self, suite_name: &str, test_case: &TestCase) -> Result<TestResult> {
        debug!("执行测试用例: {}", test_case.name);

        let start_time = Instant::now();

        // 设置测试环境
        self.setup_test_environment(test_case).await?;

        // 执行 SQL 查询
        let query_result = match self.execute_sql(&test_case.sql).await {
            Ok(result) => Some(result),
            Err(e) => {
                warn!("SQL 执行失败: {}", e);
                None
            }
        };

        let execution_time = start_time.elapsed();

        // 收集性能指标
        let performance_metrics = if self.config.logging.include_performance {
            Some(self.performance_monitor.collect_metrics().await)
        } else {
            None
        };

        // 验证结果
        let validation_result = if let Some(ref actual_result) = query_result {
            self.result_checker.validate_result(actual_result, &test_case.expected_result).await
        } else {
            Err(anyhow!("SQL 执行失败"))
        };

        let passed = validation_result.is_ok();
        let error_message = validation_result.err().map(|e| e.to_string());

        let result = TestResult {
            test_name: test_case.name.clone(),
            suite_name: suite_name.to_string(),
            passed,
            execution_time,
            actual_result: query_result,
            expected_result: Some(test_case.expected_result.clone()),
            error_message,
            performance_metrics,
        };

        // 清理测试环境
        self.cleanup_test_environment(test_case).await?;

        debug!("测试用例 {} 完成，结果: {}", test_case.name, if passed { "通过" } else { "失败" });

        Ok(result)
    }

    /// 异步执行测试用例 (用于并行执行)
    async fn execute_test_case_async(
        suite_name: &str,
        test_case: &TestCase,
        database: DatabaseConnection,
        result_checker: ResultChecker,
        performance_monitor: PerformanceMonitor,
    ) -> Result<TestResult> {
        let start_time = Instant::now();

        // 执行 SQL 查询
        let query_result = match database.execute_sql(&test_case.sql).await {
            Ok(result) => Some(result),
            Err(e) => {
                warn!("SQL 执行失败: {}", e);
                None
            }
        };

        let execution_time = start_time.elapsed();

        // 收集性能指标
        let performance_metrics = Some(performance_monitor.collect_metrics().await);

        // 验证结果
        let validation_result = if let Some(ref actual_result) = query_result {
            result_checker.validate_result(actual_result, &test_case.expected_result).await
        } else {
            Err(anyhow!("SQL 执行失败"))
        };

        let passed = validation_result.is_ok();
        let error_message = validation_result.err().map(|e| e.to_string());

        Ok(TestResult {
            test_name: test_case.name.clone(),
            suite_name: suite_name.to_string(),
            passed,
            execution_time,
            actual_result: query_result,
            expected_result: Some(test_case.expected_result.clone()),
            error_message,
            performance_metrics,
        })
    }

    /// 设置测试环境
    async fn setup_test_environment(&mut self, test_case: &TestCase) -> Result<()> {
        // 这里可以添加测试环境设置逻辑
        // 比如创建测试数据库、插入测试数据等
        debug!("设置测试环境: {}", test_case.name);
        Ok(())
    }

    /// 清理测试环境
    async fn cleanup_test_environment(&mut self, test_case: &TestCase) -> Result<()> {
        // 这里可以添加测试环境清理逻辑
        // 比如删除测试数据、重置数据库状态等
        debug!("清理测试环境: {}", test_case.name);
        Ok(())
    }

    /// 执行 SQL 查询
    async fn execute_sql(&mut self, sql: &str) -> Result<QueryResult> {
        let start_time = Instant::now();

        // 执行 SQL 查询
        let result = self.database.execute_sql(sql).await?;

        let execution_time = start_time.elapsed();

        Ok(QueryResult {
            sql: sql.to_string(),
            data: result.data,
            columns: result.columns,
            row_count: result.row_count,
            execution_time_ms: execution_time.as_millis() as u64,
            error: result.error,
        })
    }

    /// 运行性能测试
    pub async fn run_performance_test(&mut self, test_case: &TestCase, iterations: u32) -> Result<Vec<TestResult>> {
        info!("运行性能测试: {} ({} 次迭代)", test_case.name, iterations);

        let mut results = Vec::new();

        for i in 0..iterations {
            debug!("性能测试迭代 {}/{}", i + 1, iterations);

            let result = self.execute_test_case("performance", test_case).await?;
            results.push(result);

            // 在迭代之间添加短暂延迟
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 计算性能统计
        let performance_stats = self.calculate_performance_statistics(&results);

        info!("性能测试完成，平均执行时间: {:.2}ms",
              performance_stats.avg_execution_time_ms);

        Ok(results)
    }

    /// 计算性能统计
    fn calculate_performance_statistics(&self, results: &[TestResult]) -> crate::PerformanceStatistics {
        let perf_metrics: Vec<&PerformanceMetrics> = results.iter()
            .filter_map(|r| r.performance_metrics.as_ref())
            .collect();

        if perf_metrics.is_empty() {
            return crate::PerformanceStatistics {
                avg_execution_time_ms: 0.0,
                max_execution_time_ms: 0,
                min_execution_time_ms: 0,
                avg_memory_usage_mb: 0.0,
                avg_cpu_usage_percent: 0.0,
                avg_throughput_qps: 0.0,
            };
        }

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

        crate::PerformanceStatistics {
            avg_execution_time_ms,
            max_execution_time_ms,
            min_execution_time_ms,
            avg_memory_usage_mb,
            avg_cpu_usage_percent,
            avg_throughput_qps,
        }
    }

    /// 获取测试结果
    pub fn get_results(&self) -> &[TestResult] {
        &self.results
    }

    /// 获取测试统计信息
    pub fn get_statistics(&self) -> crate::TestStatistics {
        crate::calculate_statistics(&self.results)
    }

    /// 检查性能阈值
    pub fn check_performance_thresholds(&self, results: &[TestResult]) -> Vec<String> {
        let mut violations = Vec::new();

        for result in results {
            if let Some(ref metrics) = result.performance_metrics {
                let thresholds = &self.config.performance_thresholds;

                if metrics.execution_time_ms > thresholds.max_execution_time_ms {
                    violations.push(format!(
                        "测试 {} 执行时间 {}ms 超过阈值 {}ms",
                        result.test_name, metrics.execution_time_ms, thresholds.max_execution_time_ms
                    ));
                }

                if metrics.memory_usage_mb > thresholds.max_memory_usage_mb {
                    violations.push(format!(
                        "测试 {} 内存使用 {:.2}MB 超过阈值 {:.2}MB",
                        result.test_name, metrics.memory_usage_mb, thresholds.max_memory_usage_mb
                    ));
                }

                if metrics.cpu_usage_percent > thresholds.max_cpu_usage_percent {
                    violations.push(format!(
                        "测试 {} CPU 使用率 {:.2}% 超过阈值 {:.2}%",
                        result.test_name, metrics.cpu_usage_percent, thresholds.max_cpu_usage_percent
                    ));
                }

                if metrics.throughput_qps < thresholds.min_throughput_qps {
                    violations.push(format!(
                        "测试 {} 吞吐量 {:.2} QPS 低于阈值 {:.2} QPS",
                        result.test_name, metrics.throughput_qps, thresholds.min_throughput_qps
                    ));
                }
            }
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TestConfig, TestSuite, TestCase, ExpectedResult, ValidationType};

    #[tokio::test]
    async fn test_test_runner_creation() {
        let config = TestConfig::default();
        let runner = TestRunner::new(config).await;
        assert!(runner.is_ok());
    }

    #[tokio::test]
    async fn test_performance_statistics() {
        let config = TestConfig::default();
        let mut runner = TestRunner::new(config).await.unwrap();

        let test_case = TestCase {
            name: "test".to_string(),
            description: "test".to_string(),
            sql: "SELECT 1".to_string(),
            expected_result: ExpectedResult {
                validation_type: ValidationType::ExactMatch,
                data: Some(vec![vec!["1".to_string()]]),
                columns: Some(vec!["1".to_string()]),
                row_count: Some(1),
                performance_threshold: None,
                pattern: None,
            },
            tags: vec![],
            timeout_seconds: 30,
            enabled: true,
        };

        let results = runner.run_performance_test(&test_case, 3).await;
        assert!(results.is_ok());
    }
}
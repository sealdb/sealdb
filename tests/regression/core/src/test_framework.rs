//! SealDB 测试框架主程序
//!
//! 替代 Python 脚本的 Rust 实现

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use clap::{App, Arg};
use serde::{Deserialize, Serialize};
use tokio;
use tracing::{info, warn, error, debug};

use crate::{
    TestConfig, TestResult, TestRunner, TestStatistics,
    calculate_statistics, TestSuite, TestCase, ExpectedResult, ValidationType,
};

/// 测试框架主程序
pub struct TestFramework {
    config: TestConfig,
    runner: TestRunner,
    results: Vec<TestResult>,
}

impl TestFramework {
    /// 创建新的测试框架实例
    pub async fn new(config_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Self::load_config(config_path)?;
        let runner = TestRunner::new(config.clone()).await?;

        Ok(Self {
            config,
            runner,
            results: Vec::new(),
        })
    }

    /// 加载配置文件
    fn load_config(config_path: &str) -> Result<TestConfig, Box<dyn std::error::Error>> {
        let config_content = std::fs::read_to_string(config_path)?;
        let config: TestConfig = serde_yaml::from_str(&config_content)?;
        Ok(config)
    }

    /// 运行所有测试
    pub async fn run_all_tests(&mut self) -> Result<HashMap<String, Vec<TestResult>>, Box<dyn std::error::Error>> {
        info!("开始运行测试框架");

        let mut all_results = HashMap::new();

        for (suite_name, suite) in &self.config.test_suites {
            if suite.enabled {
                info!("运行测试套件: {}", suite_name);
                let results = self.run_test_suite(suite_name, suite).await?;
                all_results.insert(suite_name.clone(), results.clone());
                self.results.extend(results);
            } else {
                info!("跳过禁用的测试套件: {}", suite_name);
            }
        }

        info!("所有测试套件运行完成，共 {} 个测试", self.results.len());
        Ok(all_results)
    }

    /// 运行指定测试套件
    pub async fn run_test_suite(&mut self, suite_name: &str, suite: &TestSuite) -> Result<Vec<TestResult>, Box<dyn std::error::Error>> {
        info!("运行测试套件: {} ({} 个测试用例)", suite_name, suite.test_cases.len());

        let mut results = Vec::new();

        for test_case in &suite.test_cases {
            if test_case.enabled {
                debug!("执行测试用例: {}", test_case.name);

                let start_time = Instant::now();
                let result = self.runner.execute_test_case(suite_name, test_case).await?;
                let execution_time = start_time.elapsed();

                info!("测试 {} 完成，结果: {} ({:?})",
                      test_case.name,
                      if result.passed { "通过" } else { "失败" },
                      execution_time);

                results.push(result);
            }
        }

        let passed_count = results.iter().filter(|r| r.passed).count();
        info!("测试套件 {} 完成，通过: {}/{}", suite_name, passed_count, results.len());

        Ok(results)
    }

    /// 运行特定测试套件
    pub async fn run_specific_suites(&mut self, suite_names: &[String]) -> Result<HashMap<String, Vec<TestResult>>, Box<dyn std::error::Error>> {
        info!("运行指定测试套件: {:?}", suite_names);

        let mut all_results = HashMap::new();

        for suite_name in suite_names {
            if let Some(suite) = self.config.test_suites.get(suite_name) {
                if suite.enabled {
                    let results = self.run_test_suite(suite_name, suite).await?;
                    all_results.insert(suite_name.clone(), results.clone());
                    self.results.extend(results);
                } else {
                    warn!("测试套件 {} 已禁用", suite_name);
                }
            } else {
                warn!("未找到测试套件: {}", suite_name);
            }
        }

        Ok(all_results)
    }

    /// 生成测试报告
    pub fn generate_report(&self, results: &HashMap<String, Vec<TestResult>>) -> String {
        let stats = self.get_statistics();

        let mut report = String::new();
        report.push_str("# SealDB 测试报告\n\n");
        report.push_str(&format!("## 测试摘要\n"));
        report.push_str(&format!("- 总测试数: {}\n", stats.total_tests));
        report.push_str(&format!("- 通过测试: {}\n", stats.passed_tests));
        report.push_str(&format!("- 失败测试: {}\n", stats.failed_tests));
        report.push_str(&format!("- 通过率: {:.1}%\n\n", stats.pass_rate));

        report.push_str("## 详细结果\n\n");

        for (suite_name, suite_results) in results {
            let suite_passed = suite_results.iter().filter(|r| r.passed).count();
            let suite_total = suite_results.len();
            let suite_pass_rate = if suite_total > 0 {
                (suite_passed as f64 / suite_total as f64) * 100.0
            } else {
                0.0
            };

            report.push_str(&format!("### {}\n", suite_name));
            report.push_str(&format!("- 总测试数: {}\n", suite_total));
            report.push_str(&format!("- 通过测试: {}\n", suite_passed));
            report.push_str(&format!("- 失败测试: {}\n", suite_total - suite_passed));
            report.push_str(&format!("- 通过率: {:.1}%\n\n", suite_pass_rate));

            for result in suite_results {
                let status = if result.passed { "✅ 通过" } else { "❌ 失败" };
                report.push_str(&format!("- {}: {} ({:?})\n",
                                       result.test_name, status, result.execution_time));

                if let Some(ref error) = result.error_message {
                    report.push_str(&format!("  - 错误: {}\n", error));
                }
            }
            report.push_str("\n");
        }

        report
    }

    /// 保存测试结果到 JSON 文件
    pub fn save_results(&self, results: &HashMap<String, Vec<TestResult>>, output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let results_data: HashMap<String, Vec<serde_json::Value>> = results
            .iter()
            .map(|(suite_name, suite_results)| {
                let json_results: Vec<serde_json::Value> = suite_results
                    .iter()
                    .map(|result| serde_json::to_value(result).unwrap())
                    .collect();
                (suite_name.clone(), json_results)
            })
            .collect();

        let json_content = serde_json::to_string_pretty(&results_data)?;
        std::fs::write(output_path, json_content)?;

        info!("测试结果已保存: {}", output_path);
        Ok(())
    }

    /// 打印测试摘要
    pub fn print_summary(&self, results: &HashMap<String, Vec<TestResult>>) {
        let stats = self.get_statistics();

        println!("\n{}", "=".repeat(60));
        println!("测试摘要");
        println!("{}", "=".repeat(60));
        println!("总测试数: {}", stats.total_tests);
        println!("通过测试: {}", stats.passed_tests);
        println!("失败测试: {}", stats.failed_tests);
        println!("通过率: {:.1}%", stats.pass_rate);
        println!("{}", "=".repeat(60));

        // 按套件显示结果
        for (suite_name, suite_results) in results {
            let suite_passed = suite_results.iter().filter(|r| r.passed).count();
            let suite_total = suite_results.len();
            let suite_pass_rate = if suite_total > 0 {
                (suite_passed as f64 / suite_total as f64) * 100.0
            } else {
                0.0
            };

            let status = if suite_passed == suite_total { "✅" } else { "❌" };
            println!("{} {}: {}/{} ({:.1}%)",
                    status, suite_name, suite_passed, suite_total, suite_pass_rate);
        }

        // 显示失败的测试
        let failed_tests: Vec<String> = self.results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| format!("{}.{}", r.suite_name, r.test_name))
            .collect();

        if !failed_tests.is_empty() {
            println!("\n失败的测试:");
            for failed_test in failed_tests {
                println!("  - {}", failed_test);
            }
        }

        println!("{}", "=".repeat(60));
    }

    /// 获取测试统计信息
    pub fn get_statistics(&self) -> TestStatistics {
        calculate_statistics(&self.results)
    }

    /// 检查性能阈值
    pub fn check_performance_thresholds(&self) -> Vec<String> {
        self.runner.check_performance_thresholds(&self.results)
    }
}

/// 命令行参数结构
#[derive(Debug)]
pub struct CliArgs {
    pub config_path: String,
    pub suites: Option<Vec<String>>,
    pub output_path: String,
    pub report_path: String,
    pub parallel: bool,
    pub verbose: bool,
}

impl CliArgs {
    /// 解析命令行参数
    pub fn parse() -> Self {
        let matches = App::new("SealDB Test Framework")
            .version("1.0")
            .author("SealDB Team")
            .about("SealDB 测试框架 - Rust 实现")
            .arg(
                Arg::with_name("config")
                    .short("c")
                    .long("config")
                    .value_name("FILE")
                    .help("配置文件路径")
                    .default_value("config/test_config.yaml")
            )
            .arg(
                Arg::with_name("suite")
                    .short("s")
                    .long("suite")
                    .value_name("SUITE")
                    .help("指定测试套件")
                    .multiple(true)
            )
            .arg(
                Arg::with_name("output")
                    .short("o")
                    .long("output")
                    .value_name("FILE")
                    .help("结果输出文件")
                    .default_value("test_results.json")
            )
            .arg(
                Arg::with_name("report")
                    .short("r")
                    .long("report")
                    .value_name("FILE")
                    .help("报告输出文件")
                    .default_value("test_report.md")
            )
            .arg(
                Arg::with_name("parallel")
                    .short("p")
                    .long("parallel")
                    .help("并行执行测试")
            )
            .arg(
                Arg::with_name("verbose")
                    .short("v")
                    .long("verbose")
                    .help("详细输出")
            )
            .get_matches();

        let suites = matches.values_of("suite")
            .map(|values| values.map(|s| s.to_string()).collect());

        Self {
            config_path: matches.value_of("config").unwrap().to_string(),
            suites,
            output_path: matches.value_of("output").unwrap().to_string(),
            report_path: matches.value_of("report").unwrap().to_string(),
            parallel: matches.is_present("parallel"),
            verbose: matches.is_present("verbose"),
        }
    }
}

/// 主函数
pub async fn run_test_framework() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();

    // 检查配置文件是否存在
    if !Path::new(&args.config_path).exists() {
        error!("配置文件不存在: {}", args.config_path);
        std::process::exit(1);
    }

    // 初始化测试框架
    let mut framework = TestFramework::new(&args.config_path).await?;

    let start_time = Instant::now();

    // 运行测试
    let results = if let Some(suites) = args.suites {
        framework.run_specific_suites(&suites).await?
    } else {
        framework.run_all_tests().await?
    };

    let end_time = start_time.elapsed();

    // 保存结果
    framework.save_results(&results, &args.output_path)?;

    // 生成报告
    let report = framework.generate_report(&results);
    std::fs::write(&args.report_path, report)?;

    // 打印摘要
    framework.print_summary(&results);

    // 输出执行时间
    println!("\n总执行时间: {:.2?}", end_time);

    // 检查是否有失败的测试
    let stats = framework.get_statistics();
    if stats.passed_tests < stats.total_tests {
        println!("\n❌ 有 {} 个测试失败", stats.failed_tests);
        std::process::exit(1);
    } else {
        println!("\n🎉 所有测试通过!");
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cli_args_parse() {
        let args = CliArgs {
            config_path: "test_config.yaml".to_string(),
            suites: Some(vec!["basic".to_string()]),
            output_path: "test_results.json".to_string(),
            report_path: "test_report.md".to_string(),
            parallel: false,
            verbose: true,
        };

        assert_eq!(args.config_path, "test_config.yaml");
        assert_eq!(args.suites.unwrap(), vec!["basic"]);
    }
}
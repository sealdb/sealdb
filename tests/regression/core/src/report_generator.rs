//! 报告生成器模块
//!
//! 负责生成各种格式的测试报告

use std::collections::HashMap;
use std::time::Duration;
use anyhow::Result;
use tracing::{debug, info, warn};

use crate::{TestResult, TestStatistics, PerformanceStatistics};

/// 报告生成器
pub struct ReportGenerator {
    output_format: ReportFormat,
    include_performance: bool,
    include_details: bool,
}

/// 报告格式
#[derive(Debug, Clone)]
pub enum ReportFormat {
    Markdown,
    Json,
    Html,
    Text,
}

impl ReportGenerator {
    /// 创建新的报告生成器
    pub fn new(format: ReportFormat, include_performance: bool, include_details: bool) -> Self {
        Self {
            output_format: format,
            include_performance,
            include_details,
        }
    }

    /// 生成测试报告
    pub fn generate_report(&self, results: &HashMap<String, Vec<TestResult>>, stats: &TestStatistics) -> Result<String> {
        match self.output_format {
            ReportFormat::Markdown => self.generate_markdown_report(results, stats),
            ReportFormat::Json => self.generate_json_report(results, stats),
            ReportFormat::Html => self.generate_html_report(results, stats),
            ReportFormat::Text => self.generate_text_report(results, stats),
        }
    }

    /// 生成 Markdown 报告
    fn generate_markdown_report(&self, results: &HashMap<String, Vec<TestResult>>, stats: &TestStatistics) -> Result<String> {
        let mut report = String::new();

        // 标题
        report.push_str("# SealDB 测试报告\n\n");

        // 摘要
        report.push_str("## 测试摘要\n\n");
        report.push_str(&format!("- **总测试数**: {}\n", stats.total_tests));
        report.push_str(&format!("- **通过测试**: {}\n", stats.passed_tests));
        report.push_str(&format!("- **失败测试**: {}\n", stats.failed_tests));
        report.push_str(&format!("- **跳过测试**: {}\n", stats.skipped_tests));
        report.push_str(&format!("- **通过率**: {:.1}%\n", stats.pass_rate));
        report.push_str(&format!("- **总执行时间**: {:.2?}\n", stats.total_execution_time));
        report.push_str(&format!("- **平均执行时间**: {:.2?}\n\n", stats.average_execution_time));

        // 性能统计
        if self.include_performance {
            if let Some(ref perf_stats) = stats.performance_stats {
                report.push_str("## 性能统计\n\n");
                report.push_str(&format!("- **平均执行时间**: {:.2}ms\n", perf_stats.avg_execution_time_ms));
                report.push_str(&format!("- **最大执行时间**: {}ms\n", perf_stats.max_execution_time_ms));
                report.push_str(&format!("- **最小执行时间**: {}ms\n", perf_stats.min_execution_time_ms));
                report.push_str(&format!("- **平均内存使用**: {:.2}MB\n", perf_stats.avg_memory_usage_mb));
                report.push_str(&format!("- **平均 CPU 使用率**: {:.2}%\n", perf_stats.avg_cpu_usage_percent));
                report.push_str(&format!("- **平均吞吐量**: {:.2} QPS\n\n", perf_stats.avg_throughput_qps));
            }
        }

        // 详细结果
        if self.include_details {
            report.push_str("## 详细结果\n\n");

            for (suite_name, suite_results) in results {
                let suite_passed = suite_results.iter().filter(|r| r.passed).count();
                let suite_total = suite_results.len();
                let suite_pass_rate = if suite_total > 0 {
                    (suite_passed as f64 / suite_total as f64) * 100.0
                } else {
                    0.0
                };

                report.push_str(&format!("### {}\n\n", suite_name));
                report.push_str(&format!("- **总测试数**: {}\n", suite_total));
                report.push_str(&format!("- **通过测试**: {}\n", suite_passed));
                report.push_str(&format!("- **失败测试**: {}\n", suite_total - suite_passed));
                report.push_str(&format!("- **通过率**: {:.1}%\n\n", suite_pass_rate));

                for result in suite_results {
                    let status = if result.passed { "✅ 通过" } else { "❌ 失败" };
                    report.push_str(&format!("- **{}**: {} ({:?})\n",
                                           result.test_name, status, result.execution_time));

                    if let Some(ref error) = result.error_message {
                        report.push_str(&format!("  - 错误: {}\n", error));
                    }
                }
                report.push_str("\n");
            }
        }

        // 失败测试列表
        let failed_tests: Vec<&TestResult> = results.values()
            .flatten()
            .filter(|r| !r.passed)
            .collect();

        if !failed_tests.is_empty() {
            report.push_str("## 失败的测试\n\n");
            for result in failed_tests {
                report.push_str(&format!("- **{}.{}**: {}\n",
                                       result.suite_name, result.test_name,
                                       result.error_message.as_ref().unwrap_or(&"未知错误".to_string())));
            }
            report.push_str("\n");
        }

        Ok(report)
    }

    /// 生成 JSON 报告
    fn generate_json_report(&self, results: &HashMap<String, Vec<TestResult>>, stats: &TestStatistics) -> Result<String> {
        #[derive(serde::Serialize)]
        struct JsonReport {
            summary: TestStatistics,
            results: HashMap<String, Vec<TestResult>>,
            timestamp: chrono::DateTime<chrono::Utc>,
        }

        let report = JsonReport {
            summary: stats.clone(),
            results: results.clone(),
            timestamp: chrono::Utc::now(),
        };

        let json = serde_json::to_string_pretty(&report)?;
        Ok(json)
    }

    /// 生成 HTML 报告
    fn generate_html_report(&self, results: &HashMap<String, Vec<TestResult>>, stats: &TestStatistics) -> Result<String> {
        let mut html = String::new();

        html.push_str("<!DOCTYPE html>\n<html>\n<head>\n");
        html.push_str("<title>SealDB 测试报告</title>\n");
        html.push_str("<style>\n");
        html.push_str("body { font-family: Arial, sans-serif; margin: 20px; }\n");
        html.push_str(".summary { background: #f5f5f5; padding: 15px; border-radius: 5px; }\n");
        html.push_str(".passed { color: green; }\n");
        html.push_str(".failed { color: red; }\n");
        html.push_str(".suite { margin: 20px 0; padding: 10px; border: 1px solid #ddd; }\n");
        html.push_str("</style>\n</head>\n<body>\n");

        html.push_str("<h1>SealDB 测试报告</h1>\n");

        // 摘要
        html.push_str("<div class='summary'>\n");
        html.push_str("<h2>测试摘要</h2>\n");
        html.push_str(&format!("<p><strong>总测试数</strong>: {}</p>\n", stats.total_tests));
        html.push_str(&format!("<p><strong>通过测试</strong>: <span class='passed'>{}</span></p>\n", stats.passed_tests));
        html.push_str(&format!("<p><strong>失败测试</strong>: <span class='failed'>{}</span></p>\n", stats.failed_tests));
        html.push_str(&format!("<p><strong>通过率</strong>: {:.1}%</p>\n", stats.pass_rate));
        html.push_str("</div>\n");

        // 详细结果
        if self.include_details {
            for (suite_name, suite_results) in results {
                let suite_passed = suite_results.iter().filter(|r| r.passed).count();
                let suite_total = suite_results.len();
                let suite_pass_rate = if suite_total > 0 {
                    (suite_passed as f64 / suite_total as f64) * 100.0
                } else {
                    0.0
                };

                html.push_str(&format!("<div class='suite'>\n"));
                html.push_str(&format!("<h3>{}</h3>\n", suite_name));
                html.push_str(&format!("<p>通过率: {:.1}% ({}/{})</p>\n", suite_pass_rate, suite_passed, suite_total));

                for result in suite_results {
                    let status_class = if result.passed { "passed" } else { "failed" };
                    let status_text = if result.passed { "✅ 通过" } else { "❌ 失败" };
                    html.push_str(&format!("<p class='{}'>{}: {}</p>\n",
                                          status_class, result.test_name, status_text));
                }
                html.push_str("</div>\n");
            }
        }

        html.push_str("</body>\n</html>");

        Ok(html)
    }

    /// 生成文本报告
    fn generate_text_report(&self, results: &HashMap<String, Vec<TestResult>>, stats: &TestStatistics) -> Result<String> {
        let mut report = String::new();

        report.push_str("SealDB 测试报告\n");
        report.push_str(&"=".repeat(50));
        report.push_str("\n\n");

        // 摘要
        report.push_str("测试摘要:\n");
        report.push_str(&format!("  总测试数: {}\n", stats.total_tests));
        report.push_str(&format!("  通过测试: {}\n", stats.passed_tests));
        report.push_str(&format!("  失败测试: {}\n", stats.failed_tests));
        report.push_str(&format!("  通过率: {:.1}%\n", stats.pass_rate));
        report.push_str(&format!("  总执行时间: {:.2?}\n\n", stats.total_execution_time));

        // 详细结果
        if self.include_details {
            for (suite_name, suite_results) in results {
                let suite_passed = suite_results.iter().filter(|r| r.passed).count();
                let suite_total = suite_results.len();
                let suite_pass_rate = if suite_total > 0 {
                    (suite_passed as f64 / suite_total as f64) * 100.0
                } else {
                    0.0
                };

                report.push_str(&format!("{}:\n", suite_name));
                report.push_str(&format!("  通过率: {:.1}% ({}/{})\n", suite_pass_rate, suite_passed, suite_total));

                for result in suite_results {
                    let status = if result.passed { "PASS" } else { "FAIL" };
                    report.push_str(&format!("    {}: {} ({:?})\n",
                                           result.test_name, status, result.execution_time));
                }
                report.push_str("\n");
            }
        }

        Ok(report)
    }
}

impl Default for ReportGenerator {
    fn default() -> Self {
        Self::new(ReportFormat::Markdown, true, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_markdown_report_generation() {
        let generator = ReportGenerator::new(ReportFormat::Markdown, true, true);

        let mut results = HashMap::new();
        results.insert("basic".to_string(), vec![
            TestResult {
                test_name: "test1".to_string(),
                suite_name: "basic".to_string(),
                passed: true,
                execution_time: Duration::from_millis(100),
                actual_result: None,
                expected_result: None,
                error_message: None,
                performance_metrics: None,
            }
        ]);

        let stats = TestStatistics {
            total_tests: 1,
            passed_tests: 1,
            failed_tests: 0,
            skipped_tests: 0,
            total_execution_time: Duration::from_millis(100),
            average_execution_time: Duration::from_millis(100),
            pass_rate: 100.0,
            performance_stats: None,
        };

        let report = generator.generate_report(&results, &stats).unwrap();
        assert!(report.contains("SealDB 测试报告"));
        assert!(report.contains("100.0%"));
    }
}
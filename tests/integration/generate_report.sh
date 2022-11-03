#!/bin/bash

# SealDB 测试报告生成脚本

set -e

echo "=== 生成 SealDB 测试报告 ==="

# 创建报告目录
mkdir -p reports

# 运行测试并生成报告
echo "运行测试并收集结果..."

# 单元测试
echo "## 单元测试结果" > reports/test_report.md
cargo test --lib -- --nocapture 2>&1 | tee reports/unit_tests.log

# 集成测试
echo "## 集成测试结果" >> reports/test_report.md
cargo test -p sealdb-integration-tests -- --nocapture 2>&1 | tee reports/integration_tests.log

# 回归测试
echo "## 回归测试结果" >> reports/test_report.md
cd tests/regression/test_framework
cargo test -- --nocapture 2>&1 | tee ../../../reports/regression_tests.log
cd ../../..

# 生成测试统计
echo "## 测试统计" >> reports/test_report.md
echo "- 单元测试: $(grep -c 'test.*ok' reports/unit_tests.log || echo '0') 通过" >> reports/test_report.md
echo "- 集成测试: $(grep -c 'test.*ok' reports/integration_tests.log || echo '0') 通过" >> reports/test_report.md
echo "- 回归测试: $(grep -c 'test.*ok' reports/regression_tests.log || echo '0') 通过" >> reports/test_report.md

echo "测试报告已生成到 reports/test_report.md" 
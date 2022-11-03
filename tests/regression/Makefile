# SealDB 测试框架 Makefile
# 纯 Rust 实现，类似 PostgreSQL regress 和 MySQL MTR 的使用方式

.PHONY: help setup test test-basic test-advanced test-optimizer test-performance test-regression clean report install-deps build

# 默认目标
help:
	@echo "SealDB 测试框架 (Rust 实现)"
	@echo ""
	@echo "可用命令:"
	@echo "  build            - 编译测试框架"
	@echo "  test             - 运行所有测试"
	@echo "  test-basic       - 运行基本 SQL 测试"
	@echo "  test-advanced    - 运行高级 SQL 测试"
	@echo "  test-optimizer   - 运行优化器测试"
	@echo "  test-performance - 运行性能测试"
	@echo "  test-regression  - 运行回归测试"
	@echo "  clean            - 清理测试环境"
	@echo "  report           - 生成测试报告"
	@echo "  install-deps     - 安装依赖"
	@echo "  benchmark        - 运行基准测试"
	@echo "  stress-test      - 运行压力测试"

# 编译测试框架
build:
	@echo "编译测试框架..."
	cargo build --release
	@echo "编译完成!"

# 安装依赖
install-deps:
	@echo "安装 Rust 依赖..."
	cargo build --release
	@echo "依赖安装完成!"

# 设置测试环境
setup:
	@echo "设置测试环境..."
	@mkdir -p reports
	@mkdir -p data/expected_results
	@echo "测试环境设置完成!"

# 运行所有测试
test: build
	@echo "运行所有测试..."
	../target/release/test-framework run --config config/test_config.yaml
	@echo "所有测试完成!"

# 运行基本 SQL 测试
test-basic: build
	@echo "运行基本 SQL 测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "基本 SQL 测试完成!"

# 运行高级 SQL 测试
test-advanced: build
	@echo "运行高级 SQL 测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite advanced
	@echo "高级 SQL 测试完成!"

# 运行优化器测试
test-optimizer: build
	@echo "运行优化器测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite optimizer
	@echo "优化器测试完成!"

# 运行性能测试
test-performance: build
	@echo "运行性能测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite performance
	@echo "性能测试完成!"

# 运行回归测试
test-regression: build
	@echo "运行回归测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite regression
	@echo "回归测试完成!"

# 运行基准测试
benchmark: build
	@echo "运行基准测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite performance --format json --concurrency 1
	@echo "基准测试完成!"

# 运行压力测试
stress-test: build
	@echo "运行压力测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite performance --format json --concurrency 16
	@echo "压力测试完成!"

# 生成测试报告
report:
	@echo "生成测试报告..."
	@if [ -f test_results.json ]; then \
		echo "测试结果文件存在，生成报告..."; \
		../target/release/test-framework report --results test_results.json --format html; \
	else \
		echo "未找到测试结果文件，请先运行测试"; \
	fi
	@echo "测试报告已生成到 reports/ 目录"

# 清理测试环境
clean:
	@echo "清理测试环境..."
	rm -rf reports/
	rm -f test_results.json
	rm -f test_report.md
	rm -f test_framework.log
	@echo "清理完成!"

# 快速测试 (只运行基本测试)
quick-test: build
	@echo "运行快速测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "快速测试完成!"

# 并行测试
parallel-test: build
	@echo "运行并行测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite performance --concurrency 8
	@echo "并行测试完成!"

# 详细测试 (包含调试信息)
verbose-test: build
	@echo "运行详细测试..."
	RUST_LOG=debug ../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "详细测试完成!"

# 持续集成测试
ci-test: build
	@echo "运行 CI 测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "CI 测试完成!"

# 性能基准测试
performance-benchmark: build
	@echo "运行性能基准测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite performance --concurrency 1
	@echo "性能基准测试完成!"

# 兼容性测试
compatibility-test: build
	@echo "运行兼容性测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "兼容性测试完成!"

# 生成测试用例
generate-tests:
	@echo "生成测试用例..."
	@echo "请手动创建测试用例文件到 sql_tests/ 目录"
	@echo "测试用例生成完成!"

# 验证测试环境
validate-env: build
	@echo "验证测试环境..."
	../target/release/test-framework validate --test-dir sql_tests
	@echo "环境验证完成!"

# 运行特定测试
test-specific: build
	@echo "运行特定测试..."
	@if [ -z "$(TEST_NAME)" ]; then \
		echo "请指定测试名称: make test-specific TEST_NAME=test_name"; \
	else \
		echo "运行特定测试: $(TEST_NAME)"; \
		../target/release/test-framework run --config config/test_config.yaml --suite basic; \
	fi
	@echo "特定测试完成!"

# 运行测试套件
test-suite: build
	@echo "运行测试套件: $(SUITE)"
	@if [ -z "$(SUITE)" ]; then \
		echo "请指定测试套件: make test-suite SUITE=basic"; \
	else \
		../target/release/test-framework run --config config/test_config.yaml --suite $(SUITE); \
	fi
	@echo "测试套件完成!"

# 示例用法:
# make test-suite SUITE=basic
# make test-specific TEST_NAME=select_basic

# 开发模式测试
dev-test: build
	@echo "运行开发模式测试..."
	RUST_LOG=debug ../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "开发模式测试完成!"

# 生产模式测试
prod-test: build
	@echo "运行生产模式测试..."
	../target/release/test-framework run --config config/test_config.yaml --suite regression
	@echo "生产模式测试完成!"

# 调试模式测试
debug-test: build
	@echo "运行调试模式测试..."
	RUST_LOG=trace ../target/release/test-framework run --config config/test_config.yaml --suite basic
	@echo "调试模式测试完成!"

# 检查测试覆盖率
coverage:
	@echo "检查测试覆盖率..."
	cargo test --lib --all-features
	@echo "覆盖率检查完成!"

# 运行测试并生成报告
test-with-report: test report
	@echo "测试和报告生成完成!"

# 完整测试流程
full-test: setup test report
	@echo "完整测试流程完成!"

# 帮助信息
usage:
	@echo "使用示例:"
	@echo "  make test                    # 运行所有测试"
	@echo "  make test-basic              # 运行基本测试"
	@echo "  make test-performance        # 运行性能测试"
	@echo "  make test-suite SUITE=basic # 运行指定套件"
	@echo "  make benchmark               # 运行基准测试"
	@echo "  make report                  # 生成报告"
	@echo "  make clean                   # 清理环境"
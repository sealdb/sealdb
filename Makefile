.PHONY: help build test clean docker-build docker-run docker-stop lint format coverage docs

# 默认目标
help:
	@echo "SealDB 开发工具"
	@echo ""
	@echo "可用命令:"
	@echo "  build        - 构建项目"
	@echo "  test         - 运行测试"
	@echo "  test-integration - 运行集成测试"
	@echo "  test-unit    - 运行单元测试"
	@echo "  test-performance - 运行性能测试"
	@echo "  test-concurrent - 运行并发测试"
	@echo "  test-framework - 构建测试框架"
	@echo "  test-framework-run - 运行测试框架"
	@echo "  clean        - 清理构建文件"
	@echo "  lint         - 运行代码检查"
	@echo "  format       - 格式化代码"
	@echo "  coverage     - 生成代码覆盖率报告"
	@echo "  docs         - 生成文档"
	@echo "  docker-build - 构建 Docker 镜像"
	@echo "  docker-run   - 运行 Docker 容器"
	@echo "  docker-stop  - 停止 Docker 容器"
	@echo "  dev          - 开发模式运行"

# 构建项目
build:
	@echo "构建 SealDB..."
	cargo build --release
	@echo "构建完成!"

# 运行测试
test:
	@echo "运行测试..."
	cargo test --all-features
	@echo "测试完成!"

# 运行集成测试
test-integration:
	@echo "运行集成测试..."
	cargo run --bin integration_test
	@echo "集成测试完成!"

# 运行单元测试
test-unit:
	@echo "运行单元测试..."
	cargo test --lib --all-features
	@echo "单元测试完成!"

# 运行性能测试
test-performance:
	@echo "运行性能测试..."
	cargo run --bin integration_test -- --performance
	@echo "性能测试完成!"

# 运行并发测试
test-concurrent:
	@echo "运行并发测试..."
	cargo run --bin integration_test -- --concurrent
	@echo "并发测试完成!"

# 构建测试框架
test-framework:
	@echo "构建测试框架..."
	cargo build --release -p sealdb-test-framework
	@echo "测试框架构建完成!"

# 运行测试框架
test-framework-run: test-framework
	@echo "运行测试框架..."
	./target/release/test-framework run
	@echo "测试框架运行完成!"

# 清理构建文件
clean:
	@echo "清理构建文件..."
	cargo clean
	@echo "清理完成!"

# 代码检查
lint:
	@echo "运行代码检查..."
	cargo clippy --all-targets --all-features -- -D warnings
	@echo "代码检查完成!"

# 格式化代码
format:
	@echo "格式化代码..."
	cargo fmt --all
	@echo "格式化完成!"

# 代码覆盖率
coverage:
	@echo "生成代码覆盖率报告..."
	cargo install cargo-llvm-cov
	cargo llvm-cov --all-features --workspace --html --output-dir coverage
	@echo "覆盖率报告已生成到 coverage/ 目录"

# 生成文档
docs:
	@echo "生成文档..."
	cargo doc --no-deps --open
	@echo "文档生成完成!"

# 构建 Docker 镜像
docker-build:
	@echo "构建 Docker 镜像..."
	docker build -t sealdb:latest .
	@echo "Docker 镜像构建完成!"

# 运行 Docker 容器
docker-run:
	@echo "启动 SealDB 容器..."
	docker-compose up -d
	@echo "容器启动完成!"

# 停止 Docker 容器
docker-stop:
	@echo "停止 SealDB 容器..."
	docker-compose down
	@echo "容器已停止!"

# 开发模式
dev:
	@echo "开发模式运行..."
	cargo run --bin sealdb

# 安装开发依赖
install-deps:
	@echo "安装开发依赖..."
	cargo install cargo-llvm-cov
	cargo install cargo-audit
	cargo install cargo-udeps
	cargo install cargo-outdated
	@echo "依赖安装完成!"

# 安全检查
security-check:
	@echo "运行安全检查..."
	cargo audit
	@echo "安全检查完成!"

# 依赖更新检查
deps-check:
	@echo "检查过时的依赖..."
	cargo outdated
	@echo "依赖检查完成!"

# 完整检查
check-all: lint test security-check
	@echo "所有检查完成!"

# 发布准备
release-prep: clean build test lint security-check
	@echo "发布准备完成!"
# SealDB 测试框架 - 使用指南

## 🚀 快速开始

### 1. 构建测试框架

```bash
# 从项目根目录构建
cargo build --release -p sealdb-test-framework

# 或者使用 Makefile
make test-framework
```

### 2. 运行测试

```bash
# 运行所有测试
./target/release/test-framework run

# 运行基本测试
./target/release/test-framework run --suite basic

# 运行性能测试
./target/release/test-framework run --suite performance

# 运行优化器测试
./target/release/test-framework run --suite optimizer
```

### 3. 生成报告

```bash
# 生成 HTML 报告
./target/release/test-framework report --results test_results.json --format html

# 生成 JSON 报告
./target/release/test-framework report --results test_results.json --format json

# 生成 Markdown 报告
./target/release/test-framework report --results test_results.json --format md
```

### 4. 验证测试用例

```bash
# 验证测试用例文件
./target/release/test-framework validate --test-dir suites
```

## 📋 命令行选项

### run 命令

```bash
./target/release/test-framework run [OPTIONS]

选项:
  -c, --config <CONFIG>            测试配置文件路径 [默认: config/test_config.yaml]
  -f, --format <FORMAT>            输出格式 [默认: json]
  -c, --concurrency <CONCURRENCY>  并发数 [默认: 4]
  -s, --suite <SUITE>              测试套件类型 [默认: all]
  -h, --help                       显示帮助信息
```

### report 命令

```bash
./target/release/test-framework report [OPTIONS]

选项:
  -r, --results <RESULTS>          结果文件路径
  -f, --format <FORMAT>            输出格式 [默认: html]
  -h, --help                       显示帮助信息
```

### validate 命令

```bash
./target/release/test-framework validate [OPTIONS]

选项:
  -t, --test-dir <TEST_DIR>        测试用例目录
  -h, --help                       显示帮助信息
```

## 🔧 Makefile 命令

### 基本测试命令

```bash
# 构建测试框架
make test-framework

# 运行所有测试
make test

# 运行基本测试
make test-basic

# 运行高级测试
make test-advanced

# 运行优化器测试
make test-optimizer

# 运行性能测试
make test-performance

# 运行回归测试
make test-regression
```

### 特殊测试命令

```bash
# 快速测试
make quick-test

# 并行测试
make parallel-test

# 详细测试
make verbose-test

# 基准测试
make benchmark

# 压力测试
make stress-test

# 持续集成测试
make ci-test

# 兼容性测试
make compatibility-test
```

### 报告和验证

```bash
# 生成测试报告
make report

# 验证测试环境
make validate-env

# 清理测试环境
make clean
```

### 开发工具

```bash
# 开发模式测试
make dev-test

# 生产模式测试
make prod-test

# 调试模式测试
make debug-test
```

## 📊 测试套件类型

### 1. basic - 基本 SQL 测试
- **用途**: 验证基本 SQL 功能
- **测试用例**: 5 个
- **执行时间**: ~1.2 秒
- **特点**: 快速验证核心功能

```bash
./target/release/test-framework run --suite basic
```

### 2. advanced - 高级 SQL 测试
- **用途**: 验证复杂 SQL 功能
- **测试用例**: 8 个
- **执行时间**: ~2.1 秒
- **特点**: 测试高级查询功能

```bash
./target/release/test-framework run --suite advanced
```

### 3. optimizer - 优化器测试
- **用途**: 验证查询优化器功能
- **测试用例**: 12 个
- **执行时间**: ~3.5 秒
- **特点**: 测试查询优化能力

```bash
./target/release/test-framework run --suite optimizer
```

### 4. performance - 性能测试
- **用途**: 性能基准和压力测试
- **测试用例**: 15 个
- **执行时间**: ~8.2 秒
- **特点**: 测试系统性能

```bash
./target/release/test-framework run --suite performance
```

### 5. regression - 回归测试
- **用途**: 功能回归验证
- **测试用例**: 20 个
- **执行时间**: ~5.8 秒
- **特点**: 确保功能稳定性

```bash
./target/release/test-framework run --suite regression
```

## 🔍 调试和日志

### 启用调试日志

```bash
# 设置日志级别
export RUST_LOG=debug

# 运行测试
./target/release/test-framework run --suite basic
```

### 详细输出模式

```bash
# 使用详细模式
RUST_LOG=debug ./target/release/test-framework run --suite basic
```

### 性能分析

```bash
# 使用 cargo-flamegraph 进行性能分析
cargo install flamegraph
cargo flamegraph --bin test-framework run --suite performance
```

## 📈 性能测试

### 基准测试

```bash
# 单线程基准测试
./target/release/test-framework run --suite performance --concurrency 1
```

### 压力测试

```bash
# 高并发压力测试
./target/release/test-framework run --suite performance --concurrency 16
```

### 性能监控

```bash
# 运行性能测试并监控资源
./target/release/test-framework run --suite performance --concurrency 8
```

## 📄 报告格式

### HTML 报告

```bash
# 生成 HTML 报告
./target/release/test-framework report --results test_results.json --format html

# 查看报告
open reports/test_report.html
```

### JSON 报告

```bash
# 生成 JSON 报告
./target/release/test-framework report --results test_results.json --format json

# 查看报告
cat reports/test_report.json
```

### Markdown 报告

```bash
# 生成 Markdown 报告
./target/release/test-framework report --results test_results.json --format md

# 查看报告
cat reports/test_report.md
```

## 🔧 配置示例

### 基本配置

```yaml
# config/test_config.yaml
database:
  host: localhost
  port: 4000
  username: root
  password: ""
  database: test
  connection_timeout: 30
  query_timeout: 60
  max_connections: 10

test_suites:
  basic:
    enabled: true
    description: "基本 SQL 功能测试"
    parallel: false
    retry_count: 3
    timeout_seconds: 30
    test_cases_dir: "suites/basic"

performance_thresholds:
  max_execution_time_ms: 1000
  min_throughput_qps: 1000.0
  max_memory_usage_mb: 512.0
  max_cpu_usage_percent: 80.0
```

### 高级配置

```yaml
# config/test_config.yaml
logging:
  level: info
  format: json
  output: file
  file: test_framework.log

monitoring:
  enabled: true
  metrics:
    - cpu_usage
    - memory_usage
    - disk_io
    - network_io

reporting:
  formats:
    - html
    - json
    - md
  output_dir: reports/
  retention_days: 30
```

## 🚨 故障排除

### 常见问题

1. **构建失败**
   ```bash
   # 检查 Rust 版本
   rustc --version

   # 清理并重新构建
   cargo clean
   cargo build --release -p sealdb-test-framework
   ```

2. **测试执行失败**
   ```bash
   # 检查配置文件
   cat config/test_config.yaml

   # 验证测试环境
   make validate-env
   ```

3. **报告生成失败**
   ```bash
   # 检查测试结果文件
   ls -la test_results.json

   # 重新运行测试
   make test-basic
   make report
   ```

### 调试技巧

1. **启用详细日志**
   ```bash
   RUST_LOG=debug ./target/release/test-framework run --suite basic
   ```

2. **检查测试结果**
   ```bash
   cat test_results.json
   ```

3. **验证测试用例**
   ```bash
   ./target/release/test-framework validate --test-dir suites
   ```

## 📚 更多资源

- [README.md](README.md) - 详细文档
- [QUICK_START.md](QUICK_START.md) - 快速开始指南
- [config/test_config.yaml](config/test_config.yaml) - 配置文件示例
- [suites/](suites/) - 测试用例示例
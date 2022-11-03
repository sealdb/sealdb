# SealDB 测试框架 - 快速开始指南

## 🚀 5分钟快速上手

### 1. 安装依赖

```bash
# 进入测试框架目录
cd test_framework

# 构建 Rust 测试框架
cargo build --release -p sealdb-test-framework

# 设置环境
make setup
```

### 2. 运行第一个测试

```bash
# 运行基本 SQL 测试
make test-basic

# 或者使用 Rust 测试框架
./target/release/test-framework run --suite basic
```

### 3. 查看结果

```bash
# 生成 HTML 报告
make report

# 查看测试摘要
cat test_results.json
```

## 📋 常用命令

| 命令 | 说明 | 示例 |
|------|------|------|
| `make test` | 运行所有测试 | `make test` |
| `make test-basic` | 运行基本测试 | `make test-basic` |
| `make test-performance` | 运行性能测试 | `make test-performance` |
| `make report` | 生成报告 | `make report` |
| `make clean` | 清理环境 | `make clean` |

## 🔧 基本配置

### 1. 数据库连接配置

编辑 `config/test_config.yaml`:

```yaml
database:
  host: localhost
  port: 4000
  username: root
  password: ""
  database: test
```

### 2. 运行特定测试套件

```bash
# 运行基本测试
make test-suite SUITE=basic

# 运行性能测试
make test-suite SUITE=performance

# 运行所有启用的测试套件
./target/release/test-framework run
```

## 📝 创建测试用例

### 1. 创建 SQL 测试文件

```sql
-- suites/basic/my_test.sql
-- 测试名称: 我的第一个测试
-- 描述: 验证基本功能
-- 标签: basic, first-test

-- 准备数据
CREATE TABLE test_users (id INT, name VARCHAR(50));
INSERT INTO test_users VALUES (1, 'Alice');

-- 测试查询
SELECT * FROM test_users WHERE id = 1;

-- 期望结果
-- id | name
-- 1  | Alice

-- 清理
DROP TABLE test_users;
```

### 2. 注册测试套件

在 `config/test_config.yaml` 中添加:

```yaml
test_suites:
  my_suite:
    enabled: true
    description: "我的测试套件"
    parallel: false
    timeout_seconds: 30
    test_cases_dir: "suites/my_suite"
```

### 3. 运行新测试

```bash
make test-suite SUITE=my_suite
```

## 📊 查看测试结果

### 1. 控制台输出

```bash
./target/release/test-framework run --suite basic
```

输出示例:
```
============================================================
测试摘要
============================================================
总测试数: 50
通过测试: 48
失败测试: 2
通过率: 96.0%
============================================================
✅ basic: 48/50 (96.0%)
❌ performance: 10/12 (83.3%)
============================================================
```

### 2. HTML 报告

```bash
make report
# 打开 reports/test_report.html
```

### 3. JSON 结果

```bash
cat test_results.json
```

## 🔍 调试测试

### 1. 启用详细日志

```bash
RUST_LOG=debug ./target/release/test-framework run
```

### 2. 调试配置

```yaml
# config/test_config.yaml
debug:
  enabled: true
  verbose_sql: true
  show_query_plans: true
```

### 3. 单步调试

```bash
# 运行单个测试套件
./target/release/test-framework run --suite basic

# 查看测试详情
RUST_LOG=debug ./target/release/test-framework run --suite basic
```

## 🚨 常见问题

### 1. 数据库连接失败

```bash
# 检查数据库服务
systemctl status sealdb

# 检查端口
netstat -tlnp | grep 4000

# 检查配置文件
cat config/test_config.yaml
```

### 2. 测试超时

```yaml
# 增加超时时间
test_suites:
  basic:
    timeout_seconds: 60  # 从 30 增加到 60
```

### 3. 内存不足

```yaml
# 调整性能阈值
performance_thresholds:
  max_memory_usage_mb: 1024.0  # 增加内存限制
```

## 📈 性能测试

### 1. 运行性能测试

```bash
make test-performance
```

### 2. 基准测试

```bash
make benchmark
```

### 3. 压力测试

```bash
make stress-test
```

## 🔄 CI/CD 集成

### GitHub Actions

```yaml
# .github/workflows/test.yml
name: Test Framework

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - name: Run Tests
        run: |
          cd test_framework
          make ci-test
      - name: Generate Report
        run: |
          cd test_framework
          make report
```

## 📚 下一步

1. **阅读完整文档**: 查看 `README.md` 了解详细信息
2. **探索测试类型**: 尝试不同的测试套件
3. **自定义配置**: 根据需求调整配置
4. **添加测试用例**: 为你的功能创建测试
5. **集成 CI/CD**: 自动化测试流程

## 🆘 获取帮助

- **文档**: 查看 `README.md` 获取详细信息
- **示例**: 参考 `suites/` 目录中的示例
- **配置**: 查看 `config/test_config.yaml` 了解所有配置选项
- **问题**: 提交 Issue 或查看故障排除部分

---

*这个快速开始指南帮助你快速上手 SealDB 测试框架。更多详细信息请参考完整文档。*
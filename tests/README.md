# SealDB 测试框架

本目录包含 SealDB 项目的完整测试框架，包括单元测试、集成测试和回归测试。

## 目录结构

```
tests/
├── integration/           # 集成测试
│   ├── src/             # 测试源代码
│   ├── Cargo.toml       # 测试依赖配置
│   └── README.md        # 集成测试说明
└── regression/           # 回归测试框架
    ├── src/              # 框架源代码
    ├── config/           # 配置文件
    ├── core/             # 核心功能
    ├── reports/          # 测试报告
    ├── suites/           # 测试套件
    ├── Cargo.toml        # 框架配置
    ├── Makefile          # 构建脚本
    ├── README.md         # 框架说明
    ├── QUICK_START.md    # 快速开始
    └── USAGE.md          # 使用指南
```

## 测试类型

### 单元测试
- 位置：各个模块的 `src/` 目录中
- 运行：`cargo test`
- 覆盖：函数级测试，验证单个功能

### 集成测试
- 位置：`tests/integration/`
- 运行：`cargo test -p sealdb-integration-tests`
- 覆盖：模块间协作，端到端功能验证

### 回归测试
- 位置：`tests/regression/`
- 运行：`cargo test -p sealdb-regression`
- 覆盖：SQL 功能测试，性能基准，兼容性验证

## 快速开始

### 运行所有测试
```bash
# 单元测试
cargo test

# 集成测试
cargo test -p sealdb-integration-tests

# 回归测试
cd tests/regression
cargo test
```

### 运行特定测试
```bash
# 运行特定模块的单元测试
cargo test -p sql

# 运行特定集成测试
cargo test -p sealdb-integration-tests test_basic_connection

# 运行特定回归测试套件
cd tests/regression
cargo run -- run --suite basic
```

## 测试配置

### 环境变量
- `RUST_LOG` - 日志级别 (debug, info, warn, error)
- `SEALDB_TEST_DATABASE_URL` - 测试数据库连接
- `SEALDB_TEST_TIMEOUT` - 测试超时时间

### 配置文件
- 集成测试：`tests/integration/src/lib.rs`
- 回归测试：`tests/regression/config/test_config.yaml`

## 测试报告

### 生成报告
```bash
# 集成测试报告
cd tests/integration
./generate_report.sh

# 回归测试报告
cd tests/regression
make report
```

### 报告位置
- 集成测试：`tests/integration/reports/`
- 回归测试：`tests/regression/reports/`

## 开发指南

### 添加单元测试
1. 在对应模块的 `src/` 目录中添加测试
2. 使用 `#[cfg(test)]` 标记测试模块
3. 运行 `cargo test` 验证

### 添加集成测试
1. 在 `tests/integration/src/` 中添加测试文件
2. 更新 `tests/integration/src/lib.rs`
3. 运行 `cargo test -p sealdb-integration-tests`

### 添加回归测试
1. 在 `tests/regression/suites/` 中添加 SQL 测试用例
2. 更新 `tests/regression/config/test_config.yaml`
3. 运行 `cargo test -p sealdb-regression`

## 持续集成

测试已集成到 CI/CD 流程中：
- **单元测试** - 每次提交时运行
- **集成测试** - 每次合并请求时运行
- **回归测试** - 每日定时运行
- **性能测试** - 每周运行一次

## 故障排除

### 常见问题
1. **测试超时** - 增加超时时间或检查网络连接
2. **内存不足** - 减少并发测试数量
3. **依赖冲突** - 清理并重新构建

### 调试技巧
```bash
# 启用详细日志
RUST_LOG=debug cargo test

# 运行单个测试
cargo test test_name -- --nocapture

# 查看测试输出
cargo test -- --nocapture
``` 
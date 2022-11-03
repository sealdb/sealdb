# SealDB 集成测试

本目录包含 SealDB 的集成测试，用于验证各个模块之间的协作和整体功能。

## 目录结构

```
tests/integration/
├── src/
│   ├── lib.rs                    # 集成测试库入口
│   ├── basic_integration_test.rs # 基本集成测试
│   └── advanced_integration_test.rs # 高级集成测试
├── Cargo.toml                    # 测试依赖配置
├── run_tests.sh                  # 测试运行脚本
├── generate_report.sh            # 报告生成脚本
└── README.md                     # 本文件
```

## 测试类型

### 基本集成测试 (basic_integration_test.rs)

包含以下测试场景：

1. **基本连接测试** - 验证数据库连接功能
2. **查询执行测试** - 验证 SQL 查询执行
3. **事务处理测试** - 验证事务的 ACID 特性
4. **并发访问测试** - 验证多线程并发访问
5. **性能基准测试** - 验证基本性能指标
6. **错误处理测试** - 验证错误场景处理
7. **数据一致性测试** - 验证数据完整性
8. **网络连接测试** - 验证网络连接功能
9. **内存管理测试** - 验证内存使用和释放
10. **配置验证测试** - 验证配置参数
11. **日志记录测试** - 验证日志功能

### 高级集成测试 (advanced_integration_test.rs)

包含以下测试场景：

1. **分布式一致性测试** - 验证分布式环境下的数据一致性
2. **故障恢复测试** - 验证节点故障时的恢复能力
3. **负载均衡测试** - 验证负载分布和均衡
4. **性能基准测试** - 验证详细的性能指标
5. **压力测试** - 验证高负载下的系统稳定性
6. **并发测试** - 验证大规模并发访问
7. **内存泄漏测试** - 验证内存使用和泄漏检测
8. **网络延迟测试** - 验证不同网络环境下的性能
9. **数据持久化测试** - 验证数据持久化功能
10. **安全测试** - 验证安全功能和防护
11. **监控和告警测试** - 验证监控系统功能
12. **配置管理测试** - 验证配置管理功能

## 运行测试

### 运行所有测试

```bash
# 在项目根目录运行
./tests/integration/run_tests.sh
```

### 运行特定测试

```bash
# 运行基本集成测试
cargo test -p sealdb-integration-tests --lib basic_integration_test

# 运行高级集成测试
cargo test -p sealdb-integration-tests --lib advanced_integration_test

# 运行特定测试函数
cargo test -p sealdb-integration-tests test_basic_connection
```

### 生成测试报告

```bash
# 生成详细的测试报告
./tests/integration/generate_report.sh
```

## 测试配置

### 环境变量

- `SEALDB_TEST_DATABASE_URL` - 测试数据库连接字符串
- `SEALDB_TEST_TIMEOUT` - 测试超时时间（秒）
- `SEALDB_TEST_LOG_LEVEL` - 测试日志级别
- `SEALDB_TEST_MAX_CONNECTIONS` - 最大连接数

### 测试环境设置

```rust
use sealdb_integration_tests::IntegrationTestConfig;

let config = IntegrationTestConfig::default();
// 自定义配置
let custom_config = IntegrationTestConfig {
    database_url: "localhost:4000".to_string(),
    test_timeout: std::time::Duration::from_secs(600),
    max_connections: 200,
    log_level: "debug".to_string(),
};
```

## 测试依赖

### 核心依赖

- `common` - 公共模块
- `sql` - SQL 引擎模块
- `kv` - 键值存储模块
- `core` - 核心模块
- `server` - 服务器模块

### 测试框架依赖

- `tokio` - 异步运行时
- `tokio-test` - 异步测试工具
- `tempfile` - 临时文件处理
- `serde` - 序列化支持
- `tracing` - 日志记录
- `criterion` - 性能基准测试

## 测试最佳实践

### 1. 测试隔离

每个测试应该独立运行，不依赖其他测试的状态：

```rust
#[test]
fn test_independent_functionality() {
    // 设置测试环境
    let config = TestConfig::default();
    
    // 执行测试逻辑
    let result = perform_test_operation(&config);
    
    // 验证结果
    assert!(result.is_ok());
    
    // 清理测试环境
    cleanup_test_environment();
}
```

### 2. 异步测试

对于异步操作，使用 `#[tokio::test]`：

```rust
#[tokio::test]
async fn test_async_operation() {
    let result = async_operation().await;
    assert!(result.is_ok());
}
```

### 3. 错误处理测试

测试各种错误场景：

```rust
#[test]
fn test_error_scenarios() {
    let error_cases = vec![
        "invalid_sql",
        "connection_timeout",
        "permission_denied",
    ];
    
    for case in error_cases {
        let result = test_error_case(case);
        assert!(result.is_err());
    }
}
```

### 4. 性能测试

使用基准测试验证性能：

```rust
#[test]
fn test_performance_benchmark() {
    let start = std::time::Instant::now();
    
    // 执行性能测试
    for _ in 0..1000 {
        perform_operation();
    }
    
    let duration = start.elapsed();
    assert!(duration.as_millis() < 1000); // 应该在1秒内完成
}
```

## 故障排除

### 常见问题

1. **测试超时**
   - 增加测试超时时间
   - 检查网络连接
   - 验证数据库服务状态

2. **内存不足**
   - 减少并发测试数量
   - 增加系统内存
   - 优化测试代码

3. **网络连接问题**
   - 检查防火墙设置
   - 验证端口配置
   - 确认网络可达性

### 调试技巧

1. **启用详细日志**
   ```bash
   RUST_LOG=debug cargo test
   ```

2. **运行单个测试**
   ```bash
   cargo test test_name -- --nocapture
   ```

3. **查看测试输出**
   ```bash
   cargo test -- --nocapture
   ```

## 贡献指南

### 添加新测试

1. 在相应的测试文件中添加测试函数
2. 遵循测试命名规范：`test_<功能名称>`
3. 添加适当的文档注释
4. 确保测试独立且可重复

### 测试代码规范

1. **命名规范**
   - 测试函数：`test_<功能名称>`
   - 测试配置：`TestConfig`
   - 测试环境：`TestEnvironment`

2. **文档规范**
   - 每个测试函数都有文档注释
   - 说明测试的目的和预期结果
   - 包含使用示例

3. **错误处理**
   - 测试应该处理所有可能的错误情况
   - 验证错误消息和错误类型
   - 确保错误不会导致测试崩溃

## 持续集成

集成测试已集成到 CI/CD 流程中：

- **单元测试** - 每次提交时运行
- **集成测试** - 每次合并请求时运行
- **回归测试** - 每日定时运行
- **性能测试** - 每周运行一次

测试结果会自动生成报告并发送到相关团队。 
# SealDB 快速开始指南

## 系统要求

### 基础环境
- **操作系统**: Linux (推荐 Ubuntu 20.04+)
- **内存**: 最少 4GB，推荐 8GB+
- **磁盘**: 最少 10GB 可用空间
- **网络**: 稳定的网络连接

### 依赖软件
- **Rust**: 1.70+ (通过 rustup 安装)
- **Cargo**: Rust 包管理器
- **pkg-config**: 系统配置工具
- **OpenSSL**: 加密库
- **TiKV**: 分布式存储引擎 (可选，用于生产环境)

## 安装步骤

### 1. 安装 Rust

```bash
# 安装 rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 重新加载环境变量
source ~/.cargo/env

# 验证安装
rustc --version
cargo --version
```

### 2. 安装系统依赖

```bash
# Ubuntu/Debian
sudo apt update
sudo apt install -y pkg-config libssl-dev

# CentOS/RHEL
sudo yum install -y pkgconfig openssl-devel
```

### 3. 克隆项目

```bash
git clone https://github.com/sealdb/sealdb.git
cd sealdb
```

### 4. 编译项目

```bash
# 检查依赖
cargo check

# 编译项目
cargo build

# 运行测试
cargo test
```

### 5. 运行 SealDB

```bash
# 开发模式运行
cargo run --bin sealdb

# 发布模式运行
cargo run --release --bin sealdb
```

## 配置说明

### 1. 基础配置

创建配置文件 `config/sealdb.toml`:

```toml
[server]
host = "127.0.0.1"
port = 3306
max_connections = 1000

[storage]
engine = "tikv"
endpoints = ["127.0.0.1:2379"]

[sql]
max_query_time = 300
max_result_size = 104857600  # 100MB

[logging]
level = "info"
file = "logs/sealdb.log"
```

### 2. 连接配置

```toml
[connection]
max_connections = 1000
max_idle_connections = 100
connection_timeout = 30
idle_timeout = 300
```

### 3. 线程池配置

```toml
[thread_pool]
min_workers = 4
max_workers = 32
queue_size = 10000
keep_alive_time = 60
```

## 使用示例

### 1. 连接数据库

```bash
# 使用 MySQL 客户端连接
mysql -h 127.0.0.1 -P 3306 -u root -p
```

### 2. 创建数据库和表

```sql
-- 创建数据库
CREATE DATABASE testdb;
USE testdb;

-- 创建表
CREATE TABLE users (
    id INT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    email VARCHAR(255) UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### 3. 插入数据

```sql
INSERT INTO users (id, name, email) VALUES
(1, 'Alice', 'alice@example.com'),
(2, 'Bob', 'bob@example.com'),
(3, 'Charlie', 'charlie@example.com');
```

### 4. 查询数据

```sql
-- 基础查询
SELECT * FROM users;

-- 条件查询
SELECT name, email FROM users WHERE id > 1;

-- 聚合查询
SELECT COUNT(*) as user_count FROM users;
```

## 监控和调试

### 1. 查看日志

```bash
# 查看实时日志
tail -f logs/sealdb.log

# 查看错误日志
grep ERROR logs/sealdb.log
```

### 2. 性能监控

```bash
# 查看系统资源使用
htop

# 查看网络连接
netstat -an | grep 3306
```

### 3. 调试模式

```bash
# 启用调试日志
RUST_LOG=debug cargo run --bin sealdb
```

## 常见问题

### 1. 编译错误

**问题**: `failed to run custom build command for openssl-sys`
```bash
# 解决方案
sudo apt install -y pkg-config libssl-dev
```

**问题**: `error[E0599]: no method named 'as_slice'`
```bash
# 解决方案: 更新依赖版本或修复代码
cargo update
```

### 2. 运行时错误

**问题**: 连接被拒绝
```bash
# 检查服务是否启动
ps aux | grep sealdb

# 检查端口是否监听
netstat -an | grep 3306
```

**问题**: 内存不足
```bash
# 调整 JVM 参数或增加系统内存
# 检查内存使用情况
free -h
```

### 3. 性能问题

**问题**: 查询速度慢
- 检查索引是否正确创建
- 优化查询语句
- 增加系统资源

**问题**: 连接数过多
- 调整连接池配置
- 检查连接泄漏
- 增加最大连接数

## 下一步

1. **阅读架构文档**: [架构设计](architecture.md)
2. **了解 SQL 引擎**: [SQL 引擎设计](sql-engine.md)
3. **学习连接管理**: [连接管理设计](connection-management.md)
4. **参与开发**: 查看 [贡献指南](../CONTRIBUTING.md)

## 获取帮助

- **GitHub Issues**: [提交问题](https://github.com/sealdb/sealdb/issues)
- **文档**: [完整文档](../docs/)
- **社区**: [讨论区](https://github.com/sealdb/sealdb/discussions)
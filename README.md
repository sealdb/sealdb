# SealDB 🦭

[![Rust](https://img.shields.io/badge/Rust-1.70+-blue.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Build Status](https://img.shields.io/badge/Build-Passing-brightgreen.svg)]()

SealDB 是一个基于 Rust 开发的高性能分布式数据库系统，采用 TiKV 作为底层存储引擎，提供完整的 SQL 查询能力和企业级特性。

## ✨ 核心特性

- **🚀 高性能**: 基于 Rust 和 Tokio 异步运行时，支持高并发处理
- **🔧 完整 SQL 支持**: 支持 SELECT、INSERT、UPDATE、DELETE、CREATE TABLE 等标准 SQL 语句
- **🧠 智能优化**: 基于规则 (RBO) 和成本 (CBO) 的查询优化器
- **🔗 连接管理**: 多级优先级队列和智能连接池管理
- **📊 实时监控**: CPU、内存、网络等系统资源实时监控
- **🔄 分布式存储**: 基于 TiKV 的分布式存储引擎
- **⚡ 异步架构**: 全异步设计，支持高并发和低延迟

## 🏗️ 系统架构

```mermaid
graph TB
    Client[客户端] --> Server[服务层]
    Server --> SQL[SQL 引擎]
    SQL --> Optimizer[查询优化器]
    Optimizer --> Executor[执行器]
    Executor --> KV[KV 存储层]
    KV --> TiKV[TiKV 集群]
    
    subgraph "连接管理"
        CM[连接管理器]
        TP[线程池管理器]
        PQ[优先级队列]
    end
    
    Server --> CM
    CM --> TP
    TP --> PQ
```

## 🚀 快速开始

### 系统要求

- **操作系统**: Linux (推荐 Ubuntu 20.04+)
- **内存**: 最少 4GB，推荐 8GB+
- **Rust**: 1.70+ (通过 rustup 安装)

### 安装和运行

```bash
# 1. 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# 2. 安装系统依赖
sudo apt update && sudo apt install -y pkg-config libssl-dev

# 3. 克隆项目
git clone https://github.com/your-username/sealdb.git
cd sealdb

# 4. 编译和运行
cargo build
cargo run --bin sealdb
```

### 使用示例

```sql
-- 创建数据库和表
CREATE DATABASE testdb;
USE testdb;

CREATE TABLE users (
    id INT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    email VARCHAR(255) UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- 插入数据
INSERT INTO users (id, name, email) VALUES 
(1, 'Alice', 'alice@example.com'),
(2, 'Bob', 'bob@example.com');

-- 查询数据
SELECT * FROM users WHERE id > 1;
```

## 📚 文档

- **[快速开始指南](docs/quickstart.md)** - 安装、配置和使用教程
- **[架构设计](docs/architecture.md)** - 系统架构和模块设计
- **[SQL 引擎设计](docs/sql-engine.md)** - SQL 解析、优化和执行
- **[连接管理设计](docs/connection-management.md)** - 连接池和线程池管理

## 🛠️ 技术栈

| 组件 | 技术 | 说明 |
|------|------|------|
| **语言** | Rust | 高性能系统编程语言 |
| **异步运行时** | Tokio | 高性能异步 I/O |
| **存储引擎** | TiKV | 分布式 KV 存储 |
| **SQL 解析** | sqlparser-rs | SQL 语法解析 |
| **序列化** | Serde | 数据序列化 |
| **错误处理** | thiserror + anyhow | 统一错误处理 |
| **日志** | tracing | 分布式追踪 |
| **监控** | sysinfo | 系统资源监控 |

## 🔧 项目结构

```
sealdb/
├── bin/                    # 可执行文件
├── common/                 # 公共模块 (配置、错误、类型、连接管理)
├── core/                   # 核心计算逻辑
├── kv/                     # KV 存储层 (TiKV 客户端)
├── sql/                    # SQL 引擎 (解析器、优化器、执行器)
├── planner/                # 查询计划器
├── server/                 # 服务层
├── docs/                   # 文档
└── README.md              # 项目说明
```

## 🎯 核心功能

### SQL 引擎
- **解析器**: 支持标准 SQL 语法，生成抽象语法树 (AST)
- **优化器**: 基于规则和成本的查询优化
- **执行器**: 高效执行优化后的查询计划

### 连接管理
- **连接池**: 智能连接复用和管理
- **优先级队列**: 多级请求优先级调度
- **资源监控**: 实时 CPU、内存使用监控

### 存储引擎
- **TiKV 集成**: 分布式 KV 存储支持
- **事务支持**: ACID 事务特性
- **高可用**: 分布式架构保证高可用性

## 📊 性能特性

- **高并发**: 支持数千并发连接
- **低延迟**: 毫秒级查询响应
- **高吞吐**: 每秒数万次查询处理
- **智能调度**: 基于优先级的请求调度
- **资源优化**: 动态资源分配和监控

## 🤝 贡献指南

我们欢迎所有形式的贡献！

1. **Fork** 本仓库
2. 创建特性分支 (`git checkout -b feature/amazing-feature`)
3. 提交更改 (`git commit -m 'Add amazing feature'`)
4. 推送到分支 (`git push origin feature/amazing-feature`)
5. 创建 **Pull Request**

### 开发环境设置

```bash
# 克隆项目
git clone https://github.com/your-username/sealdb.git
cd sealdb

# 安装依赖
cargo build

# 运行测试
cargo test

# 代码检查
cargo clippy
```

## 📄 许可证

本项目采用 [Apache 2.0 许可证](LICENSE)。

## 🙏 致谢

- [TiKV](https://github.com/tikv/tikv) - 分布式 KV 存储引擎
- [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs) - SQL 解析库
- [Tokio](https://github.com/tokio-rs/tokio) - 异步运行时
- [Rust](https://www.rust-lang.org/) - 系统编程语言

## 📞 联系我们

- **GitHub Issues**: [提交问题](https://github.com/your-username/sealdb/issues)
- **GitHub Discussions**: [社区讨论](https://github.com/your-username/sealdb/discussions)
- **邮箱**: your-email@example.com

---

⭐ 如果这个项目对你有帮助，请给我们一个 Star！
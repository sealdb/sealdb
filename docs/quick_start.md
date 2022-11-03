# SealDB 快速开始指南

## 概述

本指南将帮助您快速搭建 SealDB 开发环境，并运行第一个示例程序。

## 环境要求

### 最低要求
- **操作系统**: Linux (Ubuntu 18.04+)
- **内存**: 4GB RAM
- **磁盘**: 10GB 可用空间
- **网络**: 稳定的网络连接

### 推荐配置
- **操作系统**: Ubuntu 20.04 LTS
- **内存**: 8GB+ RAM
- **磁盘**: 50GB+ 可用空间
- **CPU**: 4 核心以上

## 安装步骤

### 1. 安装系统依赖

#### Ubuntu/Debian

```bash
# 更新包管理器
sudo apt update

# 安装基础开发工具
sudo apt install -y build-essential cmake pkg-config git

# 安装可选依赖
sudo apt install -y libssl-dev zlib1g-dev

# 安装 Protobuf 和 gRPC (可选，用于 TiKV 集成)
sudo apt install -y libprotobuf-dev protobuf-compiler
sudo apt install -y libgrpc++-dev libgrpc-dev
```

#### CentOS/RHEL

```bash
# 安装开发工具组
sudo yum groupinstall -y "Development Tools"

# 安装 CMake
sudo yum install -y cmake3 pkgconfig

# 安装可选依赖
sudo yum install -y openssl-devel zlib-devel

# 安装 Protobuf 和 gRPC (可选)
sudo yum install -y protobuf-devel grpc-devel
```

### 2. 克隆项目

```bash
# 克隆 SealDB 仓库
git clone https://github.com/sealdb/seal.git
cd seal

# 查看项目结构
ls -la
```

### 3. 编译项目

```bash
# 创建构建目录
mkdir build && cd build

# 配置项目
cmake ..

# 编译项目 (使用多核加速)
make -j$(nproc)

# 检查编译结果
ls -la bin/
```

### 4. 运行测试

```bash
# 运行所有测试
make test

# 运行线程池测试示例
./examples/thread_pool_test
```

## 第一个示例

### 1. 创建配置文件

创建配置文件 `config/sealdb.conf`：

```ini
# 服务器配置
server.host=0.0.0.0
server.port=3306
server.max_connections=1000

# 多协议配置
protocols.mysql.enabled=true
protocols.mysql.port=3306
protocols.mysql.max_connections=1000
protocols.mysql.timeout_ms=30000

protocols.postgresql.enabled=true
protocols.postgresql.port=5432
protocols.postgresql.max_connections=500
protocols.postgresql.timeout_ms=30000

protocols.grpc.enabled=true
protocols.grpc.port=9090
protocols.grpc.max_connections=200
protocols.grpc.timeout_ms=30000

protocols.http.enabled=true
protocols.http.port=8080
protocols.http.max_connections=1000
protocols.http.timeout_ms=30000

# TiKV 配置 (可选)
tikv.pd_endpoints=127.0.0.1:2379
tikv.connection_pool_size=10
tikv.request_timeout=3000

# 线程池配置
thread_pool.min_threads=4
thread_pool.max_threads=32
thread_pool.enable_adaptive_scheduling=true
thread_pool.enable_resource_limits=true
thread_pool.max_cpu_percent=80
thread_pool.max_memory_mb=1024

# 日志配置
log.level=INFO
log.file=logs/sealdb.log
```

### 2. 创建日志目录

```bash
# 创建日志目录
mkdir -p logs
```

### 3. 运行 SealDB

```bash
# 使用默认配置运行
./bin/sealdb

# 或使用自定义配置文件
./bin/sealdb ../config/sealdb.conf
```

### 4. 验证运行状态

```bash
# 检查进程
ps aux | grep sealdb

# 检查各协议端口
netstat -tlnp | grep sealdb
# 应该看到以下端口：
# 3306 - MySQL 协议
# 5432 - PostgreSQL 协议
# 9090 - gRPC 协议
# 8080 - HTTP 协议

# 查看日志
tail -f logs/sealdb.log
```

### 5. 测试多协议连接

#### MySQL 客户端测试
```bash
# 使用 MySQL 客户端连接
mysql -h 127.0.0.1 -P 3306 -u root -p
```

#### PostgreSQL 客户端测试
```bash
# 使用 psql 客户端连接
psql -h 127.0.0.1 -p 5432 -U postgres -d testdb
```

#### gRPC 客户端测试
```bash
# 使用 grpcurl 测试 gRPC 接口
grpcurl -plaintext 127.0.0.1:9090 list
```

#### HTTP API 测试
```bash
# 使用 curl 测试 HTTP API
curl -X GET http://127.0.0.1:8080/api/v1/status
```

## 开发示例

### 1. 线程池使用示例

创建文件 `examples/thread_pool_example.cpp`：

```cpp
#include "sealdb/thread_pool.h"
#include "sealdb/logger.h"
#include <iostream>
#include <chrono>
#include <thread>

int main() {
    // 设置日志
    Logger::set_level(Logger::Level::INFO);

    // 创建线程池配置
    ThreadPoolConfig config;
    config.min_threads = 2;
    config.max_threads = 8;
    config.enable_adaptive_scheduling = true;
    config.enable_resource_limits = true;

    // 创建线程池
    ThreadPool thread_pool(config);

    std::cout << "线程池创建成功" << std::endl;

    // 提交一些测试任务
    for (int i = 0; i < 10; ++i) {
        thread_pool.submit_task(Task{
            [i]() {
                std::this_thread::sleep_for(std::chrono::milliseconds(100));
                Logger::info("任务 " + std::to_string(i) + " 执行完成");
            },
            TaskPriority::NORMAL,
            TaskType::QUERY,
            "测试任务 " + std::to_string(i)
        });
    }

    // 等待任务完成
    std::this_thread::sleep_for(std::chrono::seconds(2));

    // 获取统计信息
    auto stats = thread_pool.get_stats();
    std::cout << "统计信息:" << std::endl;
    std::cout << "  总线程数: " << stats.total_threads << std::endl;
    std::cout << "  活跃线程数: " << stats.active_threads << std::endl;
    std::cout << "  完成任务数: " << stats.total_completed_tasks << std::endl;
    std::cout << "  失败任务数: " << stats.total_failed_tasks << std::endl;

    return 0;
}
```

编译和运行：

```bash
# 编译示例
cd build
cmake ..
make thread_pool_example

# 运行示例
./examples/thread_pool_example
```

### 2. 配置管理示例

创建文件 `examples/config_example.cpp`：

```cpp
#include "sealdb/config.h"
#include "sealdb/logger.h"
#include <iostream>

int main() {
    // 创建配置对象
    Config config;

    // 设置服务器配置
    config.set_server_host("0.0.0.0");
    config.set_server_port(3306);
    config.set_max_connections(1000);

    // 设置 TiKV 配置
    config.set_tikv_pd_endpoints("127.0.0.1:2379");
    config.set_tikv_connection_pool_size(10);
    config.set_tikv_request_timeout(3000);

    // 设置线程池配置
    config.set_thread_pool_min_threads(4);
    config.set_thread_pool_max_threads(32);
    config.set_thread_pool_enable_adaptive_scheduling(true);
    config.set_thread_pool_enable_resource_limits(true);
    config.set_thread_pool_max_cpu_percent(80);
    config.set_thread_pool_max_memory_mb(1024);

    // 设置日志配置
    config.set_log_level(LogLevel::INFO);
    config.set_log_file("logs/sealdb.log");

    // 保存配置到文件
    // config.save_to_file("config/sealdb.conf");

    // 显示配置信息
    std::cout << "服务器配置:" << std::endl;
    std::cout << "  主机: " << config.get_server_host() << std::endl;
    std::cout << "  端口: " << config.get_server_port() << std::endl;
    std::cout << "  最大连接数: " << config.get_max_connections() << std::endl;

    std::cout << "\nTiKV 配置:" << std::endl;
    std::cout << "  PD 端点: " << config.get_tikv_pd_endpoints() << std::endl;
    std::cout << "  连接池大小: " << config.get_tikv_connection_pool_size() << std::endl;
    std::cout << "  请求超时: " << config.get_tikv_request_timeout() << "ms" << std::endl;

    std::cout << "\n线程池配置:" << std::endl;
    std::cout << "  最小线程数: " << config.get_thread_pool_min_threads() << std::endl;
    std::cout << "  最大线程数: " << config.get_thread_pool_max_threads() << std::endl;
    std::cout << "  自适应调度: " << (config.get_thread_pool_enable_adaptive_scheduling() ? "启用" : "禁用") << std::endl;
    std::cout << "  资源限制: " << (config.get_thread_pool_enable_resource_limits() ? "启用" : "禁用") << std::endl;

    return 0;
}
```

### 3. 多协议管理示例

创建文件 `examples/protocol_manager_example.cpp`：

```cpp
#include "sealdb/protocol_manager.h"
#include "sealdb/logger.h"
#include <iostream>
#include <vector>

int main() {
    // 设置日志
    Logger::set_level(Logger::Level::INFO);

    // 创建协议管理器
    ProtocolManager manager;

    // 配置支持的协议
    std::vector<ProtocolConfig> configs;

    // MySQL 协议配置
    ProtocolConfig mysql_config(ProtocolType::MYSQL, 3306);
    mysql_config.enabled = true;
    mysql_config.max_connections = 1000;
    mysql_config.timeout_ms = 30000;
    configs.push_back(mysql_config);

    // PostgreSQL 协议配置
    ProtocolConfig pg_config(ProtocolType::POSTGRESQL, 5432);
    pg_config.enabled = true;
    pg_config.max_connections = 500;
    pg_config.timeout_ms = 30000;
    configs.push_back(pg_config);

    // gRPC 协议配置
    ProtocolConfig grpc_config(ProtocolType::GRPC, 9090);
    grpc_config.enabled = true;
    grpc_config.max_connections = 200;
    grpc_config.timeout_ms = 30000;
    configs.push_back(grpc_config);

    // HTTP 协议配置
    ProtocolConfig http_config(ProtocolType::HTTP, 8080);
    http_config.enabled = true;
    http_config.max_connections = 1000;
    http_config.timeout_ms = 30000;
    configs.push_back(http_config);

    // 初始化协议管理器
    auto result = manager.initialize(configs);
    if (result != ErrorCode::SUCCESS) {
        Logger::error("协议管理器初始化失败");
        return 1;
    }

    Logger::info("协议管理器初始化成功");

    // 启动所有协议
    result = manager.start_all_protocols();
    if (result != ErrorCode::SUCCESS) {
        Logger::error("启动协议失败");
        return 1;
    }

    Logger::info("所有协议启动成功");

    // 显示启用的协议
    auto enabled_protocols = manager.get_enabled_protocols();
    std::cout << "启用的协议:" << std::endl;
    for (auto protocol : enabled_protocols) {
        std::string name = manager.get_protocol_name(protocol);
        auto version = manager.get_protocol_version(protocol);
        std::cout << "  - " << name << " v" << version.to_string() << std::endl;
    }

    // 模拟处理查询请求
    QueryRequest request;
    request.sql = "SELECT * FROM users";
    request.timeout_ms = 5000;

    QueryResponse response;

    // 通过不同协议处理请求
    for (auto protocol : enabled_protocols) {
        result = manager.handle_query_request(protocol, request, response);
        if (result == ErrorCode::SUCCESS) {
            Logger::info("通过 " + manager.get_protocol_name(protocol) + " 协议处理请求成功");
        } else {
            Logger::warn("通过 " + manager.get_protocol_name(protocol) + " 协议处理请求失败");
        }
    }

    // 显示协议统计信息
    std::cout << "\n协议统计信息:" << std::endl;
    auto all_stats = manager.get_all_stats();
    for (const auto& [protocol, stats] : all_stats) {
        std::string name = manager.get_protocol_name(protocol);
        std::cout << "  " << name << ":" << std::endl;
        std::cout << "    总连接数: " << stats.total_connections << std::endl;
        std::cout << "    活跃连接数: " << stats.active_connections << std::endl;
        std::cout << "    总请求数: " << stats.total_requests << std::endl;
        std::cout << "    总错误数: " << stats.total_errors << std::endl;
    }

    return 0;
}
```

### 4. 日志系统示例

创建文件 `examples/logger_example.cpp`：

```cpp
#include "sealdb/logger.h"
#include <iostream>
#include <thread>
#include <chrono>

int main() {
    // 设置日志级别
    Logger::set_level(Logger::Level::DEBUG);

    // 设置日志文件
    Logger::set_file("logs/example.log");

    std::cout << "开始日志示例..." << std::endl;

    // 输出不同级别的日志
    Logger::trace("这是 TRACE 级别的日志");
    Logger::debug("这是 DEBUG 级别的日志");
    Logger::info("这是 INFO 级别的日志");
    Logger::warn("这是 WARN 级别的日志");
    Logger::error("这是 ERROR 级别的日志");

    // 模拟一些操作
    Logger::info("启动应用程序");

    for (int i = 0; i < 5; ++i) {
        Logger::debug("处理任务 " + std::to_string(i));
        std::this_thread::sleep_for(std::chrono::milliseconds(100));

        if (i == 3) {
            Logger::warn("任务 " + std::to_string(i) + " 执行时间较长");
        }
    }

    Logger::info("应用程序运行完成");

    std::cout << "日志示例完成，请查看 logs/example.log 文件" << std::endl;

    return 0;
}
```

## 故障排除

### 常见问题

#### 1. 编译错误

**问题**: `fatal error: 'sealdb/sealdb.h' file not found`

**解决方案**:
```bash
# 检查包含路径
cmake -DCMAKE_VERBOSE_MAKEFILE=ON ..
make VERBOSE=1

# 确保头文件存在
find . -name "*.h" | grep sealdb
```

#### 2. 链接错误

**问题**: `undefined reference to 'sealdb::ThreadPool::ThreadPool'`

**解决方案**:
```bash
# 检查库是否正确链接
pkg-config --libs protobuf
pkg-config --cflags protobuf

# 重新编译
make clean
make -j$(nproc)
```

#### 3. 运行时错误

**问题**: `terminate called after throwing an instance of 'std::system_error'`

**解决方案**:
```bash
# 检查系统资源
ulimit -a

# 增加文件描述符限制
ulimit -n 65536
```

#### 4. 性能问题

**问题**: 程序运行缓慢

**解决方案**:
```bash
# 检查系统资源使用
top
htop
iostat

# 调整线程池配置
# 减少线程数或启用资源限制
```

### 调试技巧

#### 1. 启用调试模式

```bash
# 编译调试版本
cmake -DCMAKE_BUILD_TYPE=Debug ..
make -j$(nproc)

# 使用 GDB 调试
gdb ./bin/sealdb
```

#### 2. 启用详细日志

```cpp
// 在代码中设置日志级别
Logger::set_level(Logger::Level::DEBUG);
```

#### 3. 使用 Valgrind 检查内存

```bash
# 检查内存泄漏
valgrind --leak-check=full ./bin/sealdb

# 检查线程错误
valgrind --tool=helgrind ./bin/sealdb
```

## 下一步

### 1. 阅读文档

- [架构设计文档](architecture.md)
- [线程池设计文档](thread_pool_design.md)
- [API 参考文档](api_reference.md)
- [开发指南](development_guide.md)

### 2. 运行测试

```bash
# 运行所有测试
make test

# 运行特定测试
ctest -R thread_pool_test
```

### 3. 贡献代码

- Fork 项目
- 创建功能分支
- 提交 Pull Request

### 4. 加入社区

- 提交 Issue
- 参与讨论
- 分享经验

## 获取帮助

如果您遇到问题，可以通过以下方式获取帮助：

1. **查看文档**: 阅读 `docs/` 目录下的详细文档
2. **搜索 Issues**: 在 GitHub 上搜索相关问题
3. **提交 Issue**: 报告 Bug 或请求新功能
4. **参与讨论**: 在 GitHub Discussions 中提问
5. **邮件列表**: 发送邮件到 sealdb-dev@googlegroups.com

## 许可证

SealDB 采用 Apache 2.0 许可证 - 查看 [LICENSE](../LICENSE) 文件了解详情。
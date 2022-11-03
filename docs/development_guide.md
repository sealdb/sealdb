# SealDB 开发指南

## 概述

本文档为 SealDB 项目的开发指南，包含开发环境搭建、代码规范、贡献流程等内容。我们欢迎所有开发者参与项目贡献。

## 开发环境

### 系统要求

- **操作系统**: Linux (推荐 Ubuntu 20.04+)
- **编译器**: GCC 7.0+ 或 Clang 6.0+
- **CMake**: 3.16+
- **内存**: 至少 4GB RAM
- **磁盘**: 至少 10GB 可用空间

### 依赖库

#### 必需依赖
- **Threads**: 线程库 (系统自带)
- **PkgConfig**: 包配置工具

#### 可选依赖
- **Protobuf**: 3.x (用于 TiKV 通信)
- **gRPC**: 1.x (用于 TiKV 通信)
- **OpenSSL**: (用于加密)
- **ZLIB**: (用于压缩)

### 环境搭建

#### Ubuntu/Debian

```bash
# 更新包管理器
sudo apt update

# 安装基础开发工具
sudo apt install -y build-essential cmake pkg-config

# 安装可选依赖
sudo apt install -y libssl-dev zlib1g-dev

# 安装 Protobuf 和 gRPC (可选)
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

#### macOS

```bash
# 安装 Homebrew (如果未安装)
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# 安装依赖
brew install cmake pkg-config

# 安装可选依赖
brew install openssl zlib protobuf grpc
```

## 项目结构

```
sealdb/
├── CMakeLists.txt          # 主构建文件
├── README.md               # 项目说明
├── LICENSE                 # 许可证文件
├── .gitignore             # Git 忽略文件
├── build.sh               # 构建脚本
├── config/                # 配置文件
│   └── sealdb.conf
├── docs/                  # 文档目录
│   ├── architecture.md    # 架构设计文档
│   ├── thread_pool_design.md  # 线程池设计文档
│   └── development_guide.md   # 开发指南
├── include/               # 公共头文件 (待创建)
│   └── sealdb/
├── src/                   # 源代码
│   ├── CMakeLists.txt     # 源码构建配置
│   ├── main.cpp           # 程序入口
│   ├── sealdb.cpp         # 主类实现
│   ├── common/            # 通用模块
│   │   ├── CMakeLists.txt
│   │   ├── error.cpp/h    # 错误处理
│   │   ├── config.cpp/h   # 配置管理
│   │   ├── logger.cpp/h   # 日志系统
│   │   ├── utils.cpp/h    # 工具函数
│   │   ├── buffer.cpp/h   # 缓冲区管理
│   │   ├── thread_pool.cpp # 高级线程池实现
│   │   └── connection.cpp/h # 连接管理实现
│   ├── sql/               # SQL 处理
│   │   ├── CMakeLists.txt
│   │   ├── parser.cpp/h   # SQL 解析器
│   │   ├── lexer.cpp/h    # 词法分析器
│   │   ├── ast.cpp/h      # 抽象语法树
│   │   ├── executor.cpp/h # SQL 执行器
│   │   └── statement.cpp/h # 语句处理
│   ├── planner/           # 查询优化
│   │   ├── CMakeLists.txt
│   │   ├── optimizer.cpp/h # 查询优化器
│   │   ├── planner.cpp/h  # 执行计划生成
│   │   ├── cost_estimator.cpp/h # 成本估算
│   │   └── index_selector.cpp/h # 索引选择
│   ├── kv/                # TiKV 客户端
│   │   ├── CMakeLists.txt
│   │   ├── tikv_client.cpp/h # TiKV 客户端
│   │   ├── tikv_connection.cpp/h # 连接管理
│   │   ├── tikv_transaction.cpp/h # 事务处理
│   │   └── tikv_storage.cpp/h # 存储接口
│   └── server/            # 服务层
│       ├── CMakeLists.txt
│       ├── mysql_server.cpp/h # MySQL 协议服务
│       ├── http_server.cpp/h # HTTP API 服务
│       ├── connection_manager.cpp/h # 连接管理
│       └── session.cpp/h # 会话管理
├── tests/                 # 测试代码
│   ├── CMakeLists.txt
│   ├── unit/             # 单元测试
│   └── integration/      # 集成测试
├── tools/                 # 工具程序
│   └── CMakeLists.txt
├── examples/             # 示例代码
│   ├── CMakeLists.txt
│   └── thread_pool_test.cpp # 线程池测试示例
├── benches/              # 性能测试
└── docker/               # Docker 配置
```

## 构建系统

### CMake 配置

项目使用 CMake 作为构建系统，主要配置文件：

- `CMakeLists.txt`: 主构建文件
- `src/CMakeLists.txt`: 源码构建配置
- `tests/CMakeLists.txt`: 测试构建配置

### 构建命令

```bash
# 创建构建目录
mkdir build && cd build

# 配置项目
cmake ..

# 编译项目
make -j$(nproc)

# 运行测试
make test

# 安装
make install
```

### 构建选项

```bash
# 调试模式
cmake -DCMAKE_BUILD_TYPE=Debug ..

# 发布模式
cmake -DCMAKE_BUILD_TYPE=Release ..

# 启用测试
cmake -DBUILD_TESTING=ON ..

# 启用示例
cmake -DBUILD_EXAMPLES=ON ..
```

## 代码规范

### C++ 标准

- 使用 C++17 标准
- 遵循 Google C++ 风格指南
- 使用 `clang-format` 格式化代码

### 命名规范

#### 文件命名
- 头文件: `snake_case.h`
- 源文件: `snake_case.cpp`
- 测试文件: `test_snake_case.cpp`

#### 标识符命名
- 类名: `PascalCase`
- 函数名: `snake_case`
- 变量名: `snake_case`
- 常量: `UPPER_SNAKE_CASE`
- 命名空间: `snake_case`

### 代码格式

#### 缩进
- 使用 4 个空格缩进
- 不使用 Tab 字符

#### 行长度
- 最大行长度: 120 字符
- 超过长度时适当换行

#### 注释
- 使用中文注释
- 类和方法必须有文档注释
- 复杂逻辑需要行内注释

### 示例代码

```cpp
/**
 * @brief 数据库连接管理器
 *
 * 负责管理数据库连接的生命周期，包括连接的创建、复用和清理。
 */
class ConnectionManager {
public:
    /**
     * @brief 构造函数
     * @param max_connections 最大连接数
     */
    explicit ConnectionManager(size_t max_connections);

    /**
     * @brief 获取连接
     * @return 数据库连接对象
     */
    std::shared_ptr<Connection> get_connection();

    /**
     * @brief 释放连接
     * @param connection 要释放的连接
     */
    void release_connection(std::shared_ptr<Connection> connection);

private:
    size_t max_connections_;                    // 最大连接数
    std::queue<std::shared_ptr<Connection>> available_connections_;  // 可用连接队列
    std::mutex mutex_;                         // 互斥锁
    std::condition_variable condition_;        // 条件变量
};
```

## 测试规范

### 测试框架

使用 Google Test 框架进行单元测试：

```cpp
#include <gtest/gtest.h>
#include "sealdb/thread_pool.h"

class ThreadPoolTest : public ::testing::Test {
protected:
    void SetUp() override {
        config_.min_threads = 2;
        config_.max_threads = 4;
        thread_pool_ = std::make_unique<ThreadPool>(config_);
    }

    void TearDown() override {
        thread_pool_.reset();
    }

    ThreadPoolConfig config_;
    std::unique_ptr<ThreadPool> thread_pool_;
};

TEST_F(ThreadPoolTest, SubmitTask) {
    std::atomic<int> counter{0};

    thread_pool_->submit_task(Task{
        [&counter]() { counter++; },
        TaskPriority::NORMAL,
        TaskType::QUERY,
        "测试任务"
    });

    // 等待任务完成
    std::this_thread::sleep_for(std::chrono::milliseconds(100));

    EXPECT_EQ(counter.load(), 1);
}
```

### 测试类型

#### 单元测试
- 测试单个函数或类的功能
- 使用 Mock 对象隔离依赖
- 覆盖正常和异常情况

#### 集成测试
- 测试模块间的交互
- 测试完整的业务流程
- 验证系统集成

#### 性能测试
- 测试系统性能指标
- 压力测试和负载测试
- 内存泄漏检测

### 测试覆盖率

目标测试覆盖率：
- 单元测试: 80%+
- 集成测试: 60%+
- 关键路径: 100%

## 调试指南

### 编译时调试

```bash
# 启用调试信息
cmake -DCMAKE_BUILD_TYPE=Debug ..

# 启用地址消毒器
cmake -DCMAKE_BUILD_TYPE=Debug -DUSE_ASAN=ON ..

# 启用线程消毒器
cmake -DCMAKE_BUILD_TYPE=Debug -DUSE_TSAN=ON ..
```

### 运行时调试

#### GDB 调试

```bash
# 启动调试
gdb ./build/bin/sealdb

# 设置断点
(gdb) break main
(gdb) break ThreadPool::submit_task

# 运行程序
(gdb) run

# 查看变量
(gdb) print config_
(gdb) print stats_
```

#### Valgrind 内存检查

```bash
# 内存泄漏检查
valgrind --leak-check=full --show-leak-kinds=all ./build/bin/sealdb

# 线程错误检查
valgrind --tool=helgrind ./build/bin/sealdb
```

### 日志调试

```cpp
// 设置日志级别
Logger::set_level(LogLevel::DEBUG);

// 输出调试信息
Logger::debug("线程池状态: " + std::to_string(stats_.active_threads) + " 活跃线程");

// 输出错误信息
Logger::error("任务执行失败: " + std::string(e.what()));
```

## 性能优化

### 编译优化

```bash
# 启用优化
cmake -DCMAKE_BUILD_TYPE=Release ..

# 启用链接时优化
cmake -DCMAKE_BUILD_TYPE=Release -DCMAKE_INTERPROCEDURAL_OPTIMIZATION=ON ..
```

### 运行时优化

#### 线程池调优

```cpp
// 根据 CPU 核心数设置线程数
ThreadPoolConfig config;
config.min_threads = std::thread::hardware_concurrency();
config.max_threads = std::thread::hardware_concurrency() * 2;

// 启用自适应调度
config.enable_adaptive_scheduling = true;
config.enable_resource_limits = true;
```

#### 内存优化

```cpp
// 使用智能指针管理内存
std::unique_ptr<ThreadPool> thread_pool;

// 避免不必要的拷贝
void process_task(Task&& task);  // 使用移动语义

// 使用内存池
class MemoryPool {
    // 内存池实现
};
```

### 性能分析

#### 使用 perf 工具

```bash
# 性能分析
perf record -g ./build/bin/sealdb
perf report

# 热点分析
perf top -p $(pgrep sealdb)
```

#### 使用 gprof 工具

```bash
# 编译时启用 gprof
cmake -DCMAKE_BUILD_TYPE=Release -DUSE_GPROF=ON ..

# 运行程序后生成分析报告
gprof ./build/bin/sealdb gmon.out > analysis.txt
```

## 贡献流程

### 1. Fork 项目

1. 访问 [SealDB GitHub 仓库](https://github.com/sealdb/seal)
2. 点击 "Fork" 按钮创建个人分支

### 2. 克隆仓库

```bash
git clone https://github.com/your-username/seal.git
cd seal
git remote add upstream https://github.com/sealdb/seal.git
```

### 3. 创建功能分支

```bash
# 更新主分支
git checkout main
git pull upstream main

# 创建功能分支
git checkout -b feature/your-feature-name
```

### 4. 开发代码

```bash
# 编写代码
# 添加测试
# 更新文档

# 提交更改
git add .
git commit -m "feat: 添加新功能描述"
```

### 5. 运行测试

```bash
# 编译项目
mkdir build && cd build
cmake ..
make -j$(nproc)

# 运行测试
make test

# 检查代码风格
clang-format -i ../src/**/*.cpp ../src/**/*.h
```

### 6. 提交 Pull Request

1. 推送到个人分支
```bash
git push origin feature/your-feature-name
```

2. 在 GitHub 上创建 Pull Request
3. 填写详细的描述和测试说明
4. 等待代码审查

### 7. 代码审查

- 确保代码符合项目规范
- 添加必要的测试用例
- 更新相关文档
- 响应审查意见

## 发布流程

### 版本号规范

使用语义化版本号 (Semantic Versioning):

- **主版本号**: 不兼容的 API 修改
- **次版本号**: 向下兼容的功能性新增
- **修订号**: 向下兼容的问题修正

### 发布步骤

1. **准备发布**
   ```bash
   # 更新版本号
   # 更新 CHANGELOG.md
   # 运行完整测试套件
   ```

2. **创建发布标签**
   ```bash
   git tag -a v1.0.0 -m "Release version 1.0.0"
   git push origin v1.0.0
   ```

3. **构建发布包**
   ```bash
   # 构建源码包
   make dist

   # 构建二进制包
   make package
   ```

4. **发布到 GitHub**
   - 创建 GitHub Release
   - 上传构建产物
   - 编写发布说明

## 常见问题

### 编译问题

#### 找不到头文件
```bash
# 检查包含路径
cmake -DCMAKE_VERBOSE_MAKEFILE=ON ..
make VERBOSE=1
```

#### 链接错误
```bash
# 检查库路径
pkg-config --libs protobuf
pkg-config --cflags protobuf
```

### 运行时问题

#### 内存泄漏
```bash
# 使用 Valgrind 检查
valgrind --leak-check=full ./build/bin/sealdb
```

#### 线程问题
```bash
# 使用 ThreadSanitizer
cmake -DCMAKE_BUILD_TYPE=Debug -DUSE_TSAN=ON ..
```

### 性能问题

#### 线程池性能
- 检查线程数配置
- 监控任务队列长度
- 分析任务执行时间

#### 内存使用
- 检查内存泄漏
- 优化数据结构
- 使用内存池

## 联系方式

- **项目主页**: https://github.com/sealdb/seal
- **问题反馈**: https://github.com/sealdb/seal/issues
- **讨论区**: https://github.com/sealdb/seal/discussions
- **邮件列表**: sealdb-dev@googlegroups.com

## 许可证

本项目采用 Apache 2.0 许可证 - 查看 [LICENSE](LICENSE) 文件了解详情。
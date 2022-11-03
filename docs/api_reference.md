# SealDB API 参考文档

## 概述

本文档详细说明了 SealDB 的公共 API 接口，包括核心类、方法和数据结构。所有 API 都在 `sealdb` 命名空间中。

## 核心类

### SealDB 主类

SealDB 的主类，负责数据库的初始化和生命周期管理。

```cpp
class SealDB {
public:
    SealDB();
    ~SealDB();

    // 初始化数据库
    ErrorCode initialize(const Config& config);

    // 启动数据库服务
    ErrorCode start();

    // 停止数据库服务
    ErrorCode stop();

    // 执行 SQL 语句
    Result<std::string> execute(const std::string& sql);

    // 获取数据库状态
    bool is_running() const;

    // 获取配置信息
    const Config& get_config() const;
};
```

#### 使用示例

```cpp
#include "sealdb/sealdb.h"

// 创建 SealDB 实例
auto sealdb = std::make_unique<SealDB>();

// 配置数据库
Config config;
config.set_server_host("0.0.0.0");
config.set_server_port(3306);
config.set_tikv_pd_endpoints("127.0.0.1:2379");

// 初始化数据库
auto result = sealdb->initialize(config);
if (result != ErrorCode::SUCCESS) {
    std::cerr << "初始化失败" << std::endl;
    return 1;
}

// 启动数据库
result = sealdb->start();
if (result != ErrorCode::SUCCESS) {
    std::cerr << "启动失败" << std::endl;
    return 1;
}

// 执行 SQL
auto sql_result = sealdb->execute("SELECT * FROM users");
if (sql_result.is_ok()) {
    std::cout << "查询结果: " << sql_result.value() << std::endl;
} else {
    std::cerr << "查询失败: " << sql_result.error().message() << std::endl;
}
```

### Config 配置类

数据库配置管理类，支持从文件、环境变量或代码中加载配置。

```cpp
class Config {
public:
    Config();

    // 从文件加载配置
    ErrorCode load_from_file(const std::string& filename);

    // 从环境变量加载配置
    ErrorCode load_from_env();

    // 服务器配置
    void set_server_host(const std::string& host);
    void set_server_port(uint16_t port);
    void set_max_connections(size_t max_connections);

    std::string get_server_host() const;
    uint16_t get_server_port() const;
    size_t get_max_connections() const;

    // TiKV 配置
    void set_tikv_pd_endpoints(const std::string& endpoints);
    void set_tikv_connection_pool_size(size_t size);
    void set_tikv_request_timeout(uint32_t timeout_ms);

    std::string get_tikv_pd_endpoints() const;
    size_t get_tikv_connection_pool_size() const;
    uint32_t get_tikv_request_timeout() const;

    // 线程池配置
    void set_thread_pool_min_threads(size_t min_threads);
    void set_thread_pool_max_threads(size_t max_threads);
    void set_thread_pool_enable_adaptive_scheduling(bool enable);
    void set_thread_pool_enable_resource_limits(bool enable);
    void set_thread_pool_max_cpu_percent(uint64_t max_cpu_percent);
    void set_thread_pool_max_memory_mb(uint64_t max_memory_mb);

    size_t get_thread_pool_min_threads() const;
    size_t get_thread_pool_max_threads() const;
    bool get_thread_pool_enable_adaptive_scheduling() const;
    bool get_thread_pool_enable_resource_limits() const;
    uint64_t get_thread_pool_max_cpu_percent() const;
    uint64_t get_thread_pool_max_memory_mb() const;

    // 日志配置
    void set_log_level(LogLevel level);
    void set_log_file(const std::string& filename);

    LogLevel get_log_level() const;
    std::string get_log_file() const;
};
```

#### 配置示例

```cpp
// 创建配置对象
Config config;

// 服务器配置
config.set_server_host("0.0.0.0");
config.set_server_port(3306);
config.set_max_connections(1000);

// TiKV 配置
config.set_tikv_pd_endpoints("127.0.0.1:2379,127.0.0.1:2380");
config.set_tikv_connection_pool_size(10);
config.set_tikv_request_timeout(3000);

// 线程池配置
config.set_thread_pool_min_threads(4);
config.set_thread_pool_max_threads(32);
config.set_thread_pool_enable_adaptive_scheduling(true);
config.set_thread_pool_enable_resource_limits(true);
config.set_thread_pool_max_cpu_percent(80);
config.set_thread_pool_max_memory_mb(1024);

// 日志配置
config.set_log_level(LogLevel::INFO);
config.set_log_file("logs/sealdb.log");

// 从文件加载配置
config.load_from_file("config/sealdb.conf");
```

## 错误处理

### ErrorCode 枚举

```cpp
enum class ErrorCode {
    SUCCESS = 0,                    // 成功
    INVALID_ARGUMENT = 1,           // 无效参数
    NOT_INITIALIZED = 2,            // 未初始化
    ALREADY_INITIALIZED = 3,        // 已经初始化
    NOT_RUNNING = 4,               // 未运行
    ALREADY_RUNNING = 5,           // 已经运行
    CONFIG_ERROR = 6,              // 配置错误
    CONNECTION_ERROR = 7,          // 连接错误
    TIMEOUT_ERROR = 8,             // 超时错误
    RESOURCE_LIMIT_EXCEEDED = 9,   // 资源限制超出
    SQL_PARSE_ERROR = 10,          // SQL 解析错误
    SQL_EXECUTION_ERROR = 11,      // SQL 执行错误
    TRANSACTION_ERROR = 12,         // 事务错误
    LOCK_ERROR = 13,               // 锁错误
    STORAGE_ERROR = 14,            // 存储错误
    INTERNAL_ERROR = 15,           // 内部错误
    UNKNOWN_ERROR = 16             // 未知错误
};
```

### Error 类

```cpp
class Error {
public:
    Error(ErrorCode code, const std::string& message = "");

    ErrorCode code() const;
    const std::string& message() const;

    bool is_ok() const;
    bool is_error() const;

    // 转换为字符串
    std::string to_string() const;
};
```

### Result 模板类

```cpp
template<typename T>
class Result {
public:
    // 成功结果构造函数
    Result(const T& value);
    Result(T&& value);

    // 错误结果构造函数
    Result(const Error& error);

    // 检查结果
    bool is_ok() const;
    bool is_error() const;

    // 获取值
    const T& value() const;
    T& value();

    // 获取错误
    const Error& error() const;

    // 安全获取值
    T value_or(const T& default_value) const;
};
```

#### 错误处理示例

```cpp
// 处理执行结果
auto result = sealdb->execute("SELECT * FROM users");
if (result.is_ok()) {
    std::cout << "查询成功: " << result.value() << std::endl;
} else {
    std::cerr << "查询失败: " << result.error().message() << std::endl;

    // 根据错误类型处理
    switch (result.error().code()) {
        case ErrorCode::SQL_PARSE_ERROR:
            std::cerr << "SQL 语法错误" << std::endl;
            break;
        case ErrorCode::CONNECTION_ERROR:
            std::cerr << "连接错误" << std::endl;
            break;
        case ErrorCode::TIMEOUT_ERROR:
            std::cerr << "查询超时" << std::endl;
            break;
        default:
            std::cerr << "未知错误" << std::endl;
            break;
    }
}
```

## 线程池 API

### ThreadPool 类

高级线程池实现，支持多级队列和自适应调度。

```cpp
class ThreadPool {
public:
    // 构造函数
    explicit ThreadPool(const ThreadPoolConfig& config);

    // 析构函数
    ~ThreadPool();

    // 提交任务
    void submit_task(Task task);

    // 获取统计信息
    ThreadPoolStats get_stats() const;

    // 获取队列大小
    size_t get_queue_size(TaskPriority priority) const;

    // 调整线程池大小
    void resize(size_t min_threads, size_t max_threads);

    // 设置资源限制
    void set_resource_limits(uint64_t max_memory_mb,
                           uint64_t max_cpu_percent,
                           uint64_t max_io_ops);

    // 停止线程池
    void stop();

    // 检查是否运行
    bool is_running() const;
};
```

### ThreadPoolConfig 类

```cpp
struct ThreadPoolConfig {
    size_t min_threads = 4;              // 最小线程数
    size_t max_threads = 32;             // 最大线程数
    bool enable_monitoring = true;        // 启用监控
    bool enable_adaptive_scheduling = true;  // 启用自适应调度
    bool enable_resource_limits = true;   // 启用资源限制

    // 资源限制
    uint64_t max_memory_mb = 1024;       // 最大内存使用 (MB)
    uint64_t max_cpu_percent = 80;       // 最大 CPU 使用率 (%)
    uint64_t max_io_ops = 1000;          // 最大 IO 操作数

    // 调度参数
    std::chrono::milliseconds monitor_interval{1000};  // 监控间隔
    std::chrono::milliseconds adjustment_interval{5000};  // 调整间隔

    // 阈值设置
    double cpu_threshold_high = 0.8;     // CPU 高阈值
    double cpu_threshold_low = 0.3;      // CPU 低阈值
    double memory_threshold_high = 0.8;  // 内存高阈值
    double memory_threshold_low = 0.3;   // 内存低阈值
};
```

### Task 结构

```cpp
struct Task {
    std::function<void()> func;           // 任务函数
    TaskPriority priority;                 // 任务优先级
    TaskType type;                        // 任务类型
    std::string description;              // 任务描述
    std::chrono::steady_clock::time_point deadline;  // 截止时间
    std::chrono::steady_clock::time_point submit_time;  // 提交时间
};
```

### 任务优先级

```cpp
enum class TaskPriority {
    CRITICAL = 0,    // 系统关键任务 (最高优先级)
    HIGH = 1,        // 用户查询任务 (高优先级)
    NORMAL = 2,      // 一般任务 (普通优先级)
    LOW = 3,         // 后台任务 (低优先级)
    BACKGROUND = 4   // 维护任务 (最低优先级)
};
```

### 任务类型

```cpp
enum class TaskType {
    QUERY,           // SQL 查询任务
    TRANSACTION,     // 事务处理任务
    MAINTENANCE,     // 维护任务
    BACKGROUND,      // 后台任务
    SYSTEM           // 系统任务
};
```

#### 线程池使用示例

```cpp
#include "sealdb/thread_pool.h"

// 创建线程池配置
ThreadPoolConfig config;
config.min_threads = 4;
config.max_threads = 16;
config.enable_adaptive_scheduling = true;
config.enable_resource_limits = true;
config.max_memory_mb = 2048;
config.max_cpu_percent = 90;

// 创建线程池
ThreadPool thread_pool(config);

// 提交高优先级任务
thread_pool.submit_task(Task{
    []() {
        // 执行 SQL 查询
        std::cout << "执行 SQL 查询" << std::endl;
    },
    TaskPriority::HIGH,
    TaskType::QUERY,
    "用户查询任务"
});

// 提交后台任务
thread_pool.submit_task(Task{
    []() {
        // 执行维护任务
        std::cout << "执行维护任务" << std::endl;
    },
    TaskPriority::BACKGROUND,
    TaskType::MAINTENANCE,
    "数据清理任务"
});

// 获取统计信息
auto stats = thread_pool.get_stats();
std::cout << "活跃线程数: " << stats.active_threads << std::endl;
std::cout << "队列任务数: " << stats.total_queued_tasks << std::endl;
std::cout << "完成任务数: " << stats.total_completed_tasks << std::endl;
```

## 日志系统

### Logger 类

```cpp
class Logger {
public:
    // 日志级别
    enum class Level {
        TRACE = 0,
        DEBUG = 1,
        INFO = 2,
        WARN = 3,
        ERROR = 4,
        FATAL = 5
    };

    // 设置日志级别
    static void set_level(Level level);

    // 设置日志文件
    static void set_file(const std::string& filename);

    // 日志输出方法
    static void trace(const std::string& message);
    static void debug(const std::string& message);
    static void info(const std::string& message);
    static void warn(const std::string& message);
    static void error(const std::string& message);
    static void fatal(const std::string& message);

    // 格式化日志
    template<typename... Args>
    static void tracef(const std::string& format, Args... args);

    template<typename... Args>
    static void debugf(const std::string& format, Args... args);

    template<typename... Args>
    static void infof(const std::string& format, Args... args);

    template<typename... Args>
    static void warnf(const std::string& format, Args... args);

    template<typename... Args>
    static void errorf(const std::string& format, Args... args);

    template<typename... Args>
    static void fatalf(const std::string& format, Args... args);
};
```

#### 日志使用示例

```cpp
#include "sealdb/logger.h"

// 设置日志级别
Logger::set_level(Logger::Level::INFO);

// 设置日志文件
Logger::set_file("logs/sealdb.log");

// 输出日志
Logger::info("SealDB 启动成功");
Logger::debug("线程池初始化完成，线程数: " + std::to_string(thread_count));
Logger::warn("内存使用率较高: " + std::to_string(memory_usage) + "%");
Logger::error("数据库连接失败: " + error_message);

// 格式化日志
Logger::infof("处理查询请求，SQL: %s", sql.c_str());
Logger::errorf("事务提交失败，错误码: %d, 消息: %s",
               error_code, error_message.c_str());
```

## 多协议支持

### ProtocolType 枚举

```cpp
enum class ProtocolType {
    MYSQL = 0,        // MySQL 协议
    POSTGRESQL = 1,   // PostgreSQL 协议
    GRPC = 2,         // gRPC 协议
    HTTP = 3          // HTTP 协议
};
```

### ProtocolVersion 结构

```cpp
struct ProtocolVersion {
    uint8_t major;    // 主版本号
    uint8_t minor;    // 次版本号
    uint8_t patch;    // 修订号

    std::string to_string() const;  // 转换为字符串
};
```

### ProtocolHandler 接口

```cpp
class ProtocolHandler {
public:
    virtual ~ProtocolHandler() = default;

    // 协议类型
    virtual ProtocolType get_protocol_type() const = 0;
    virtual ProtocolVersion get_protocol_version() const = 0;

    // 连接管理
    virtual ErrorCode handle_connection(const std::string& client_data) = 0;
    virtual ErrorCode handle_disconnection() = 0;
    virtual ConnectionState get_connection_state() const = 0;

    // 认证处理
    virtual ErrorCode handle_authentication(const AuthInfo& auth_info) = 0;
    virtual bool is_authenticated() const = 0;

    // 查询处理
    virtual ErrorCode handle_query(const QueryRequest& request, QueryResponse& response) = 0;
    virtual ErrorCode handle_prepared_statement(const QueryRequest& request, QueryResponse& response) = 0;

    // 事务处理
    virtual ErrorCode handle_begin_transaction() = 0;
    virtual ErrorCode handle_commit_transaction() = 0;
    virtual ErrorCode handle_rollback_transaction() = 0;

    // 数据序列化/反序列化
    virtual std::string serialize_response(const QueryResponse& response) = 0;
    virtual ErrorCode deserialize_request(const std::string& data, QueryRequest& request) = 0;

    // 错误处理
    virtual std::string format_error_message(ErrorCode code, const std::string& message) = 0;

    // 统计信息
    virtual uint64_t get_requests_processed() const = 0;
    virtual uint64_t get_bytes_received() const = 0;
    virtual uint64_t get_bytes_sent() const = 0;
};
```

### ProtocolManager 类

```cpp
class ProtocolManager {
public:
    ProtocolManager();
    ~ProtocolManager();

    // 初始化和配置
    ErrorCode initialize(const std::vector<ProtocolConfig>& configs);
    ErrorCode add_protocol(const ProtocolConfig& config);
    ErrorCode remove_protocol(ProtocolType type);
    ErrorCode update_protocol_config(ProtocolType type, const ProtocolConfig& config);

    // 协议处理
    ErrorCode handle_client_connection(ProtocolType type, const std::string& client_data);
    ErrorCode handle_client_disconnection(ProtocolType type);
    ErrorCode handle_query_request(ProtocolType type, const QueryRequest& request, QueryResponse& response);

    // 状态查询
    bool is_protocol_enabled(ProtocolType type) const;
    ConnectionState get_connection_state(ProtocolType type) const;
    ProtocolStats get_protocol_stats(ProtocolType type) const;
    std::vector<ProtocolType> get_enabled_protocols() const;

    // 协议工厂
    std::unique_ptr<ProtocolHandler> create_handler(ProtocolType type);
    std::string get_protocol_name(ProtocolType type) const;
    ProtocolVersion get_protocol_version(ProtocolType type) const;

    // 统计和监控
    void update_stats(ProtocolType type, const ProtocolStats& stats);
    void reset_stats(ProtocolType type);
    std::map<ProtocolType, ProtocolStats> get_all_stats() const;

    // 错误处理
    std::string format_error_message(ProtocolType type, ErrorCode code, const std::string& message);

    // 生命周期管理
    ErrorCode start_all_protocols();
    ErrorCode stop_all_protocols();
    ErrorCode start_protocol(ProtocolType type);
    ErrorCode stop_protocol(ProtocolType type);
};
```

### ProtocolConfig 结构

```cpp
struct ProtocolConfig {
    ProtocolType type;              // 协议类型
    uint16_t port;                 // 监听端口
    bool enabled;                   // 是否启用
    uint32_t max_connections;       // 最大连接数
    uint32_t timeout_ms;           // 超时时间

    ProtocolConfig(ProtocolType t = ProtocolType::MYSQL, uint16_t p = 3306);
};
```

### ProtocolStats 结构

```cpp
struct ProtocolStats {
    uint64_t total_connections;     // 总连接数
    uint64_t active_connections;    // 活跃连接数
    uint64_t total_requests;        // 总请求数
    uint64_t total_errors;          // 总错误数
    uint64_t bytes_received;        // 接收字节数
    uint64_t bytes_sent;           // 发送字节数
    std::chrono::steady_clock::time_point start_time;  // 启动时间
};
```

## 连接管理

### Connection 类

```cpp
class Connection {
public:
    Connection();
    ~Connection();

    // 连接状态
    enum class State {
        DISCONNECTED = 0,
        CONNECTING = 1,
        CONNECTED = 2,
        ERROR = 3
    };

    // 连接管理
    ErrorCode connect(const std::string& host, uint16_t port);
    ErrorCode disconnect();

    // 状态查询
    State get_state() const;
    bool is_connected() const;

    // 连接信息
    std::string get_host() const;
    uint16_t get_port() const;
    std::string get_database() const;

    // 执行查询
    Result<std::string> execute_query(const std::string& sql);
    Result<std::string> execute_update(const std::string& sql);

    // 事务管理
    ErrorCode begin_transaction();
    ErrorCode commit_transaction();
    ErrorCode rollback_transaction();

    // 统计信息
    uint64_t get_query_count() const;
    uint64_t get_error_count() const;
    std::chrono::milliseconds get_last_query_time() const;
};
```

### ConnectionManager 类

```cpp
class ConnectionManager {
public:
    explicit ConnectionManager(size_t max_connections);
    ~ConnectionManager();

    // 获取连接
    std::shared_ptr<Connection> get_connection();

    // 释放连接
    void release_connection(std::shared_ptr<Connection> connection);

    // 连接池管理
    size_t get_pool_size() const;
    size_t get_available_connections() const;
    size_t get_active_connections() const;

    // 统计信息
    uint64_t get_total_connections_created() const;
    uint64_t get_total_connections_reused() const;
    uint64_t get_total_connections_failed() const;

    // 配置
    void set_connection_timeout(std::chrono::milliseconds timeout);
    void set_idle_timeout(std::chrono::milliseconds timeout);
    void set_max_connections(size_t max_connections);
};
```

#### 连接管理示例

```cpp
#include "sealdb/connection.h"

// 创建连接管理器
ConnectionManager manager(10);

// 获取连接
auto connection = manager.get_connection();
if (connection && connection->is_connected()) {
    // 执行查询
    auto result = connection->execute_query("SELECT * FROM users");
    if (result.is_ok()) {
        std::cout << "查询结果: " << result.value() << std::endl;
    }

    // 释放连接
    manager.release_connection(connection);
}

// 查看连接池状态
std::cout << "连接池大小: " << manager.get_pool_size() << std::endl;
std::cout << "可用连接数: " << manager.get_available_connections() << std::endl;
std::cout << "活跃连接数: " << manager.get_active_connections() << std::endl;
```

## 工具类

### Buffer 类

```cpp
class Buffer {
public:
    Buffer();
    explicit Buffer(size_t initial_size);
    ~Buffer();

    // 数据操作
    void write(const void* data, size_t size);
    size_t read(void* data, size_t max_size);

    // 缓冲区管理
    void clear();
    void resize(size_t new_size);
    size_t size() const;
    size_t capacity() const;

    // 数据访问
    const uint8_t* data() const;
    uint8_t* data();

    // 位置管理
    size_t get_read_position() const;
    size_t get_write_position() const;
    void set_read_position(size_t position);
    void set_write_position(size_t position);

    // 可用空间
    size_t readable_bytes() const;
    size_t writable_bytes() const;
};
```

### Utils 类

```cpp
class Utils {
public:
    // 字符串工具
    static std::vector<std::string> split(const std::string& str, char delimiter);
    static std::string join(const std::vector<std::string>& parts, const std::string& separator);
    static std::string trim(const std::string& str);
    static std::string to_lower(const std::string& str);
    static std::string to_upper(const std::string& str);

    // 时间工具
    static std::string get_current_time_string();
    static uint64_t get_current_timestamp();
    static std::chrono::steady_clock::time_point get_current_time();

    // 系统工具
    static size_t get_cpu_count();
    static uint64_t get_memory_usage();
    static double get_cpu_usage();

    // 加密工具
    static std::string md5(const std::string& input);
    static std::string sha256(const std::string& input);

    // 网络工具
    static bool is_valid_ip(const std::string& ip);
    static bool is_valid_port(uint16_t port);
    static std::string get_local_ip();
};
```

## 常量定义

### 系统常量

```cpp
namespace sealdb {
    // 默认配置
    constexpr uint16_t DEFAULT_SERVER_PORT = 3306;
    constexpr size_t DEFAULT_MAX_CONNECTIONS = 1000;
    constexpr uint32_t DEFAULT_REQUEST_TIMEOUT = 3000;

    // 线程池默认值
    constexpr size_t DEFAULT_MIN_THREADS = 4;
    constexpr size_t DEFAULT_MAX_THREADS = 32;
    constexpr uint64_t DEFAULT_MAX_MEMORY_MB = 1024;
    constexpr uint64_t DEFAULT_MAX_CPU_PERCENT = 80;

    // 超时设置
    constexpr uint32_t DEFAULT_TASK_TIMEOUT = 30000;  // 30秒
    constexpr uint32_t DEFAULT_CONNECTION_TIMEOUT = 5000;  // 5秒
    constexpr uint32_t DEFAULT_IDLE_TIMEOUT = 300000;  // 5分钟

    // 缓冲区大小
    constexpr size_t DEFAULT_BUFFER_SIZE = 8192;
    constexpr size_t MAX_BUFFER_SIZE = 1024 * 1024;  // 1MB

    // 日志设置
    constexpr size_t MAX_LOG_FILE_SIZE = 100 * 1024 * 1024;  // 100MB
    constexpr size_t MAX_LOG_FILES = 10;
}
```

## 版本信息

### 版本常量

```cpp
namespace sealdb {
    constexpr const char* VERSION = "1.0.0";
    constexpr const char* BUILD_DATE = __DATE__;
    constexpr const char* BUILD_TIME = __TIME__;
    constexpr const char* GIT_COMMIT = "unknown";
    constexpr const char* GIT_BRANCH = "unknown";
}
```

### 版本检查

```cpp
// 获取版本信息
std::string get_version();
std::string get_build_info();
bool is_compatible_version(const std::string& version);
```

## 错误码映射

### 错误码到字符串映射

```cpp
std::string error_code_to_string(ErrorCode code);
```

### 常见错误码说明

| 错误码 | 说明 | 解决方案 |
|--------|------|----------|
| `SUCCESS` | 操作成功 | - |
| `INVALID_ARGUMENT` | 无效参数 | 检查参数类型和范围 |
| `NOT_INITIALIZED` | 未初始化 | 先调用 `initialize()` 方法 |
| `CONNECTION_ERROR` | 连接错误 | 检查网络和服务器状态 |
| `TIMEOUT_ERROR` | 超时错误 | 增加超时时间或检查网络 |
| `RESOURCE_LIMIT_EXCEEDED` | 资源限制超出 | 调整资源限制或释放资源 |
| `SQL_PARSE_ERROR` | SQL 解析错误 | 检查 SQL 语法 |
| `SQL_EXECUTION_ERROR` | SQL 执行错误 | 检查数据库状态和权限 |

## 最佳实践

### 1. 错误处理

```cpp
// 使用 Result 类型处理返回值
auto result = sealdb->execute(sql);
if (result.is_error()) {
    Logger::error("执行失败: " + result.error().message());
    return;
}

// 使用 try-catch 处理异常
try {
    thread_pool.submit_task(task);
} catch (const std::exception& e) {
    Logger::error("提交任务失败: " + std::string(e.what()));
}
```

### 2. 资源管理

```cpp
// 使用 RAII 管理资源
{
    auto connection = manager.get_connection();
    if (connection) {
        // 使用连接
        auto result = connection->execute_query(sql);
        // 自动释放连接
    }
}
```

### 3. 性能优化

```cpp
// 合理设置线程池参数
ThreadPoolConfig config;
config.min_threads = std::thread::hardware_concurrency();
config.max_threads = std::thread::hardware_concurrency() * 2;
config.enable_adaptive_scheduling = true;

// 使用连接池
ConnectionManager manager(10);
manager.set_connection_timeout(std::chrono::milliseconds(5000));
```

### 4. 日志记录

```cpp
// 设置合适的日志级别
Logger::set_level(Logger::Level::INFO);

// 记录关键操作
Logger::info("数据库启动成功");
Logger::debug("线程池状态: " + std::to_string(stats.active_threads) + " 活跃线程");
Logger::error("连接失败: " + error_message);
```

## 兼容性说明

### C++ 标准

- 最低要求: C++17
- 推荐使用: C++20

### 编译器支持

- GCC: 7.0+
- Clang: 6.0+
- MSVC: 2019+

### 平台支持

- Linux: Ubuntu 18.04+, CentOS 7+
- macOS: 10.14+
- Windows: Windows 10+ (实验性支持)

### 依赖库版本

- CMake: 3.16+
- Protobuf: 3.x (可选)
- gRPC: 1.x (可选)
- OpenSSL: 1.1+ (可选)
- ZLIB: 1.2+ (可选)
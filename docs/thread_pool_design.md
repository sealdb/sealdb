# SealDB 高级线程池设计文档

## 概述

SealDB 的高级线程池是系统的核心组件之一，参考了 PolarDB 和 TiDB 的设计理念，实现了多级队列、自适应调度、资源限制等先进特性。该线程池为数据库系统提供了高效的任务调度和资源管理能力。

## 设计目标

### 1. 高性能
- 减少锁竞争
- 提高任务处理效率
- 优化资源利用率

### 2. 高可用
- 防止系统过载
- 支持优雅关闭
- 故障自动恢复

### 3. 可扩展
- 动态调整线程数
- 支持多种任务类型
- 模块化设计

### 4. 可监控
- 实时性能统计
- 资源使用监控
- 详细的运行日志

## 架构设计

### 多级队列架构

```
┌─────────────────────────────────────────────────────────────┐
│                    线程池管理器                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │   监控      │  │   自适应     │  │   资源      │        │
│  │   线程      │  │   调度器     │  │   管理器    │        │
│  └─────────────┘  └─────────────┘  └─────────────┘        │
└─────────────────────────────────────────────────────────────┘
                                │
┌─────────────────────────────────────────────────────────────┐
│                    多级任务队列                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │  CRITICAL   │  │    HIGH     │  │   NORMAL    │        │
│  │   Queue     │  │   Queue     │  │   Queue     │        │
│  │ (系统关键)  │  │ (用户查询)  │  │  (一般任务) │        │
│  └─────────────┘  └─────────────┘  └─────────────┘        │
│  ┌─────────────┐  ┌─────────────┐                        │
│  │     LOW     │  │ BACKGROUND  │                        │
│  │   Queue     │  │   Queue     │                        │
│  │ (后台任务)  │  │ (维护任务)  │                        │
│  └─────────────┘  └─────────────┘                        │
└─────────────────────────────────────────────────────────────┘
                                │
┌─────────────────────────────────────────────────────────────┐
│                    工作线程池                                │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │   Worker    │  │   Worker    │  │   Worker    │        │
│  │  Thread 1   │  │  Thread 2   │  │  Thread N   │        │
│  └─────────────┘  └─────────────┘  └─────────────┘        │
└─────────────────────────────────────────────────────────────┘
```

### 任务优先级定义

```cpp
enum class TaskPriority {
    CRITICAL = 0,    // 系统关键任务 (最高优先级)
    HIGH = 1,        // 用户查询任务 (高优先级)
    NORMAL = 2,      // 一般任务 (普通优先级)
    LOW = 3,         // 后台任务 (低优先级)
    BACKGROUND = 4   // 维护任务 (最低优先级)
};
```

### 任务类型定义

```cpp
enum class TaskType {
    QUERY,           // SQL 查询任务
    TRANSACTION,     // 事务处理任务
    MAINTENANCE,     // 维护任务
    BACKGROUND,      // 后台任务
    SYSTEM           // 系统任务
};
```

## 核心组件

### 1. 任务结构

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

### 2. 线程池配置

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

### 3. 统计信息

```cpp
struct ThreadPoolStats {
    std::chrono::steady_clock::time_point start_time;
    std::chrono::steady_clock::time_point last_adjustment;

    std::atomic<size_t> total_threads{0};
    std::atomic<size_t> active_threads{0};
    std::atomic<size_t> total_queued_tasks{0};
    std::atomic<size_t> total_completed_tasks{0};
    std::atomic<size_t> total_failed_tasks{0};
    std::atomic<size_t> total_timeout_tasks{0};

    std::map<TaskPriority, QueueStats> queue_stats;

    // 资源使用统计
    std::atomic<uint64_t> current_cpu_usage{0};
    std::atomic<uint64_t> current_memory_usage{0};
    std::atomic<uint64_t> current_io_ops{0};
};
```

## 实现细节

### 1. 任务提交流程

```cpp
void ThreadPool::submit_task(Task task) {
    std::lock_guard<std::mutex> lock(stats_mutex_);

    // 设置任务时间戳
    task.submit_time = std::chrono::steady_clock::now();
    task.deadline = task.submit_time + std::chrono::milliseconds(30000);  // 30秒超时

    // 根据优先级提交到相应队列
    switch (task.priority) {
        case TaskPriority::CRITICAL:
            {
                std::lock_guard<std::mutex> queue_lock(critical_queue_mutex_);
                critical_queue_.push(std::move(task));
                stats_.queue_stats[TaskPriority::CRITICAL].queued_tasks++;
            }
            break;
        case TaskPriority::HIGH:
            {
                std::lock_guard<std::mutex> queue_lock(high_queue_mutex_);
                high_queue_.push(std::move(task));
                stats_.queue_stats[TaskPriority::HIGH].queued_tasks++;
            }
            break;
        // ... 其他优先级处理
    }

    stats_.total_queued_tasks++;
    condition_.notify_one();
}
```

### 2. 任务调度算法

```cpp
Task ThreadPool::get_next_task() {
    std::unique_lock<std::mutex> lock(stats_mutex_);

    // 按优先级顺序检查队列
    auto check_queue = [this](std::priority_queue<Task>& queue,
                              std::mutex& mutex,
                              TaskPriority priority) -> Task {
        std::lock_guard<std::mutex> queue_lock(mutex);
        if (!queue.empty()) {
            Task task = std::move(const_cast<Task&>(queue.top()));
            queue.pop();
            stats_.total_queued_tasks--;
            stats_.queue_stats[priority].queued_tasks--;
            return task;
        }
        return Task{};
    };

    // 等待任务
    condition_.wait(lock, [this] {
        return !critical_queue_.empty() || !high_queue_.empty() ||
               !normal_queue_.empty() || !low_queue_.empty() ||
               !background_queue_.empty() || !running_.load();
    });

    // 按优先级获取任务
    Task task = check_queue(critical_queue_, critical_queue_mutex_, TaskPriority::CRITICAL);
    if (task.func) return task;

    task = check_queue(high_queue_, high_queue_mutex_, TaskPriority::HIGH);
    if (task.func) return task;

    task = check_queue(normal_queue_, normal_queue_mutex_, TaskPriority::NORMAL);
    if (task.func) return task;

    task = check_queue(low_queue_, low_queue_mutex_, TaskPriority::LOW);
    if (task.func) return task;

    return check_queue(background_queue_, background_queue_mutex_, TaskPriority::BACKGROUND);
}
```

### 3. 自适应调度算法

```cpp
void ThreadPool::adjust_thread_count() {
    std::lock_guard<std::mutex> lock(stats_mutex_);

    size_t current_threads = workers_.size();
    size_t active_threads = stats_.active_threads.load();
    size_t total_queued = stats_.total_queued_tasks.load();

    // 获取系统资源使用情况
    double cpu_usage = static_cast<double>(current_cpu_usage_.load()) / 100.0;
    double memory_usage = static_cast<double>(current_memory_usage_.load()) /
                          (config_.max_memory_mb * 1024.0);

    // 计算目标线程数
    size_t target_threads = current_threads;

    // 高负载情况：增加线程
    if (total_queued > 0 &&
        (cpu_usage < config_.cpu_threshold_high ||
         memory_usage < config_.memory_threshold_high)) {
        target_threads = std::min(config_.max_threads, current_threads + 2);
    }

    // 低负载情况：减少线程
    if (total_queued == 0 && active_threads < current_threads * 0.3 &&
        cpu_usage < config_.cpu_threshold_low &&
        memory_usage < config_.memory_threshold_low) {
        target_threads = std::max(config_.min_threads, current_threads - 1);
    }

    // 调整线程数量
    if (target_threads > current_threads) {
        // 增加线程
        for (size_t i = current_threads; i < target_threads; ++i) {
            workers_.emplace_back(&ThreadPool::worker_thread, this);
            stats_.total_threads++;
        }
        Logger::info("Added " + std::to_string(target_threads - current_threads) + " threads");
    }

    target_thread_count_.store(target_threads);
    stats_.last_adjustment = std::chrono::steady_clock::now();
}
```

### 4. 资源限制检查

```cpp
bool ThreadPool::check_resource_limits() {
    if (!config_.enable_resource_limits) {
        return true;
    }

    // 检查 CPU 使用率
    double cpu_usage = static_cast<double>(current_cpu_usage_.load()) / 100.0;
    if (cpu_usage > config_.max_cpu_percent / 100.0) {
        return false;
    }

    // 检查内存使用量
    double memory_usage = static_cast<double>(current_memory_usage_.load()) /
                          (config_.max_memory_mb * 1024.0);
    if (memory_usage > 1.0) {
        return false;
    }

    // 检查 IO 操作频率
    if (current_io_ops_.load() > config_.max_io_ops) {
        return false;
    }

    return true;
}
```

### 5. 工作线程实现

```cpp
void ThreadPool::worker_thread() {
    while (running_.load()) {
        try {
            Task task = get_next_task();

            if (task.func) {
                stats_.active_threads++;

                auto start_time = std::chrono::steady_clock::now();

                try {
                    // 检查任务是否超时
                    if (std::chrono::steady_clock::now() > task.deadline) {
                        stats_.total_timeout_tasks++;
                        Logger::warn("Task timeout: " + task.description);
                        continue;
                    }

                    // 检查资源限制
                    if (!check_resource_limits()) {
                        Logger::warn("Resource limit exceeded, skipping task: " + task.description);
                        stats_.total_failed_tasks++;
                        continue;
                    }

                    // 执行任务
                    task.func();
                    stats_.total_completed_tasks++;

                    // 更新资源使用情况
                    auto duration = std::chrono::steady_clock::now() - start_time;
                    ResourceUsage usage;
                    usage.cpu_time_ms = std::chrono::duration_cast<std::chrono::milliseconds>(duration).count();
                    update_resource_usage(usage);

                    // 记录长时间运行的任务
                    if (duration > std::chrono::milliseconds(1000)) {
                        Logger::warn("Task took too long: " + task.description +
                                   " (" + std::to_string(usage.cpu_time_ms) + "ms)");
                    }
                } catch (const std::exception& e) {
                    stats_.total_failed_tasks++;
                    Logger::error("Task failed: " + task.description + " - " + e.what());
                }

                stats_.active_threads--;
            }
        } catch (const std::exception& e) {
            Logger::error("Worker thread error: " + std::string(e.what()));
        }
    }
}
```

## 性能优化

### 1. 锁优化
- 使用细粒度锁减少竞争
- 优先队列减少锁持有时间
- 条件变量避免忙等待

### 2. 内存优化
- 任务对象移动语义
- 避免不必要的拷贝
- 内存池管理

### 3. 调度优化
- 优先级调度减少延迟
- 自适应调度提高吞吐量
- 资源限制防止过载

## 监控和统计

### 1. 性能指标
- 线程池大小
- 活跃线程数
- 队列长度
- 任务完成率
- 任务失败率
- 平均执行时间

### 2. 资源监控
- CPU 使用率
- 内存使用量
- IO 操作频率
- 网络连接数

### 3. 日志记录
- 任务提交日志
- 任务执行日志
- 错误和异常日志
- 性能统计日志

## 使用示例

### 1. 基本使用

```cpp
#include "sealdb/thread_pool.h"

// 创建线程池配置
ThreadPoolConfig config;
config.min_threads = 4;
config.max_threads = 16;
config.enable_adaptive_scheduling = true;
config.enable_resource_limits = true;

// 创建线程池
ThreadPool thread_pool(config);

// 提交任务
thread_pool.submit_task(Task{
    []() { /* 任务逻辑 */ },
    TaskPriority::HIGH,
    TaskType::QUERY,
    "SQL查询任务"
});

// 获取统计信息
auto stats = thread_pool.get_stats();
std::cout << "活跃线程数: " << stats.active_threads << std::endl;
std::cout << "队列任务数: " << stats.total_queued_tasks << std::endl;
```

### 2. 高级配置

```cpp
// 配置资源限制
config.max_memory_mb = 2048;
config.max_cpu_percent = 90;
config.max_io_ops = 2000;

// 配置调度参数
config.cpu_threshold_high = 0.85;
config.cpu_threshold_low = 0.25;
config.memory_threshold_high = 0.85;
config.memory_threshold_low = 0.25;

// 配置监控间隔
config.monitor_interval = std::chrono::milliseconds(500);
config.adjustment_interval = std::chrono::milliseconds(3000);
```

## 故障处理

### 1. 异常处理
- 任务执行异常捕获
- 线程异常恢复
- 资源泄漏防护

### 2. 超时处理
- 任务执行超时
- 资源获取超时
- 线程启动超时

### 3. 资源保护
- 内存使用限制
- CPU 使用限制
- IO 操作限制

## 最佳实践

### 1. 配置建议
- 根据 CPU 核心数设置线程数
- 根据内存大小设置内存限制
- 根据业务特点调整优先级

### 2. 使用建议
- 合理设置任务优先级
- 避免长时间运行的任务
- 定期监控性能指标

### 3. 调试建议
- 启用详细日志
- 监控资源使用
- 分析性能瓶颈

## 未来改进

### 1. 功能增强
- 支持任务依赖关系
- 支持任务取消机制
- 支持任务优先级动态调整

### 2. 性能优化
- 无锁队列实现
- 工作窃取算法
- NUMA 感知调度

### 3. 监控增强
- 实时性能监控
- 可视化监控界面
- 告警机制
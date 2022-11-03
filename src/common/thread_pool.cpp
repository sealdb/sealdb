#include "sealdb/thread_pool.h"
#include "sealdb/logger.h"
#include <algorithm>
#include <chrono>
#include <iostream>
#include <system_error>
#include <sys/resource.h>
#include <sys/sysinfo.h>

namespace sealdb {

ThreadPool::ThreadPool(const ThreadPoolConfig& config)
    : config_(config),
      running_(true),
      stop_requested_(false),
      target_thread_count_(config.min_threads) {

    stats_.start_time = std::chrono::steady_clock::now();
    stats_.last_adjustment = stats_.start_time;

    // 初始化队列统计 - 使用emplace避免复制
    stats_.queue_stats.emplace(TaskPriority::CRITICAL, QueueStats{});
    stats_.queue_stats.emplace(TaskPriority::HIGH, QueueStats{});
    stats_.queue_stats.emplace(TaskPriority::NORMAL, QueueStats{});
    stats_.queue_stats.emplace(TaskPriority::LOW, QueueStats{});
    stats_.queue_stats.emplace(TaskPriority::BACKGROUND, QueueStats{});

    // 创建初始工作线程
    for (size_t i = 0; i < config_.min_threads; ++i) {
        workers_.emplace_back(&ThreadPool::worker_thread, this);
        stats_.total_threads++;
    }

    // 启动监控线程
    if (config_.enable_monitoring) {
        monitor_ = std::thread(&ThreadPool::monitor_thread, this);
    }

    // 启动自适应调度线程
    if (config_.enable_adaptive_scheduling) {
        adaptive_scheduler_ = std::thread(&ThreadPool::adaptive_scheduler_thread, this);
    }

    Logger::info("Advanced ThreadPool initialized with " + std::to_string(config_.min_threads) + " threads");
}

ThreadPool::~ThreadPool() {
    stop();
}

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

void ThreadPool::monitor_thread() {
    while (running_.load()) {
        try {
            std::this_thread::sleep_for(config_.monitor_interval);

            // 清理超时任务
            cleanup_timeout_tasks();

            // 输出统计信息
            Logger::debug("ThreadPool stats - Active: " + std::to_string(get_active_threads()) +
                         ", Queued: " + std::to_string(get_queued_tasks()) +
                         ", Completed: " + std::to_string(get_completed_tasks()) +
                         ", Failed: " + std::to_string(get_failed_tasks()));
        } catch (const std::exception& e) {
            Logger::error("Monitor thread error: " + std::string(e.what()));
        }
    }
}

void ThreadPool::adaptive_scheduler_thread() {
    while (running_.load()) {
        try {
            std::this_thread::sleep_for(config_.adjustment_interval);
            adjust_thread_count();
        } catch (const std::exception& e) {
            Logger::error("Adaptive scheduler error: " + std::string(e.what()));
        }
    }
}

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
        (cpu_usage < config_.cpu_threshold_high || memory_usage < config_.memory_threshold_high)) {
        target_threads = std::min(config_.max_threads, current_threads + 2);
    }

    // 低负载情况：减少线程
    if (total_queued == 0 && active_threads < current_threads * 0.3 &&
        cpu_usage < config_.cpu_threshold_low && memory_usage < config_.memory_threshold_low) {
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
    } else if (target_threads < current_threads) {
        // 减少线程 - 通过停止请求来实现
        Logger::info("Thread count adjustment: " + std::to_string(current_threads) + " -> " + std::to_string(target_threads));
    }

    target_thread_count_.store(target_threads);
    stats_.last_adjustment = std::chrono::steady_clock::now();
}

Task ThreadPool::get_next_task() {
    std::unique_lock<std::mutex> lock(stats_mutex_);

    // 按优先级顺序检查队列
    auto check_queue = [this](std::priority_queue<Task>& queue, std::mutex& mutex, TaskPriority priority) -> Task {
        std::lock_guard<std::mutex> queue_lock(mutex);
        if (!queue.empty()) {
            Task task = std::move(const_cast<Task&>(queue.top()));
            queue.pop();
            stats_.total_queued_tasks--;
            stats_.queue_stats[priority].queued_tasks--;
            return task;
        }
        return Task([](){}, TaskPriority::BACKGROUND, TaskType::BACKGROUND);
    };

    // 等待任务
    condition_.wait(lock, [this] {
        return !critical_queue_.empty() || !high_queue_.empty() ||
               !normal_queue_.empty() || !low_queue_.empty() ||
               !background_queue_.empty() || !running_.load();
    });

    if (!running_.load()) {
        return Task([](){}, TaskPriority::BACKGROUND, TaskType::BACKGROUND);
    }

    // 按优先级获取任务
    if (!critical_queue_.empty()) {
        return check_queue(critical_queue_, critical_queue_mutex_, TaskPriority::CRITICAL);
    }
    if (!high_queue_.empty()) {
        return check_queue(high_queue_, high_queue_mutex_, TaskPriority::HIGH);
    }
    if (!normal_queue_.empty()) {
        return check_queue(normal_queue_, normal_queue_mutex_, TaskPriority::NORMAL);
    }
    if (!low_queue_.empty()) {
        return check_queue(low_queue_, low_queue_mutex_, TaskPriority::LOW);
    }
    if (!background_queue_.empty()) {
        return check_queue(background_queue_, background_queue_mutex_, TaskPriority::BACKGROUND);
    }

    return Task([](){}, TaskPriority::BACKGROUND, TaskType::BACKGROUND);
}

void ThreadPool::submit_task(Task task) {
    std::lock_guard<std::mutex> lock(stats_mutex_);

    // 检查队列大小限制
    size_t queue_size = get_queue_size(task.priority);
    if (queue_size >= config_.queue_size) {
        throw std::runtime_error("Thread pool queue is full");
    }

    // 根据优先级放入对应队列
    switch (task.priority) {
        case TaskPriority::CRITICAL: {
            std::lock_guard<std::mutex> queue_lock(critical_queue_mutex_);
            if (critical_queue_.size() < config_.critical_queue_size) {
                critical_queue_.push(std::move(task));
                stats_.total_queued_tasks++;
                stats_.queue_stats[TaskPriority::CRITICAL].queued_tasks++;
            } else {
                throw std::runtime_error("Critical queue is full");
            }
            break;
        }
        case TaskPriority::HIGH: {
            std::lock_guard<std::mutex> queue_lock(high_queue_mutex_);
            if (high_queue_.size() < config_.high_queue_size) {
                high_queue_.push(std::move(task));
                stats_.total_queued_tasks++;
                stats_.queue_stats[TaskPriority::HIGH].queued_tasks++;
            } else {
                throw std::runtime_error("High priority queue is full");
            }
            break;
        }
        case TaskPriority::NORMAL: {
            std::lock_guard<std::mutex> queue_lock(normal_queue_mutex_);
            if (normal_queue_.size() < config_.normal_queue_size) {
                normal_queue_.push(std::move(task));
                stats_.total_queued_tasks++;
                stats_.queue_stats[TaskPriority::NORMAL].queued_tasks++;
            } else {
                throw std::runtime_error("Normal queue is full");
            }
            break;
        }
        case TaskPriority::LOW: {
            std::lock_guard<std::mutex> queue_lock(low_queue_mutex_);
            if (low_queue_.size() < config_.low_queue_size) {
                low_queue_.push(std::move(task));
                stats_.total_queued_tasks++;
                stats_.queue_stats[TaskPriority::LOW].queued_tasks++;
            } else {
                throw std::runtime_error("Low priority queue is full");
            }
            break;
        }
        case TaskPriority::BACKGROUND: {
            std::lock_guard<std::mutex> queue_lock(background_queue_mutex_);
            if (background_queue_.size() < config_.background_queue_size) {
                background_queue_.push(std::move(task));
                stats_.total_queued_tasks++;
                stats_.queue_stats[TaskPriority::BACKGROUND].queued_tasks++;
            } else {
                throw std::runtime_error("Background queue is full");
            }
            break;
        }
    }

    condition_.notify_one();
}

bool ThreadPool::check_resource_limits() {
    if (!config_.enable_resource_limits) {
        return true;
    }

    uint64_t memory_usage = current_memory_usage_.load();
    uint64_t cpu_usage = current_cpu_usage_.load();
    uint64_t io_ops = current_io_operations_.load();

    if (memory_usage > config_.max_memory_mb * 1024) {
        Logger::warn("Memory limit exceeded: " + std::to_string(memory_usage / 1024) + "MB");
        return false;
    }

    if (cpu_usage > config_.max_cpu_percent) {
        Logger::warn("CPU limit exceeded: " + std::to_string(cpu_usage) + "%");
        return false;
    }

    if (io_ops > config_.max_io_operations) {
        Logger::warn("IO limit exceeded: " + std::to_string(io_ops) + " operations");
        return false;
    }

    return true;
}

void ThreadPool::update_resource_usage(const ResourceUsage& usage) {
    if (config_.enable_resource_limits) {
        current_cpu_usage_.fetch_add(usage.cpu_time_ms);
        current_memory_usage_.fetch_add(usage.memory_usage_kb);
        current_io_operations_.fetch_add(usage.io_operations);

        // 更新统计信息
        stats_.resource_usage.cpu_time_ms.fetch_add(usage.cpu_time_ms);
        stats_.resource_usage.memory_usage_kb.fetch_add(usage.memory_usage_kb);
        stats_.resource_usage.io_operations.fetch_add(usage.io_operations);
        stats_.resource_usage.network_bytes.fetch_add(usage.network_bytes);
    }
}

size_t ThreadPool::get_queue_size(TaskPriority priority) const {
    switch (priority) {
        case TaskPriority::CRITICAL: return critical_queue_.size();
        case TaskPriority::HIGH: return high_queue_.size();
        case TaskPriority::NORMAL: return normal_queue_.size();
        case TaskPriority::LOW: return low_queue_.size();
        case TaskPriority::BACKGROUND: return background_queue_.size();
        default: return 0;
    }
}

void ThreadPool::cleanup_timeout_tasks() {
    auto now = std::chrono::steady_clock::now();

    // 清理各队列中的超时任务
    auto cleanup_queue = [&now](std::priority_queue<Task>& queue, std::mutex& mutex, TaskPriority /*priority*/) {
        std::lock_guard<std::mutex> lock(mutex);
        std::vector<Task> valid_tasks;

        while (!queue.empty()) {
            Task task = std::move(const_cast<Task&>(queue.top()));
            queue.pop();

            if (now <= task.deadline) {
                valid_tasks.push_back(std::move(task));
            }
        }

        // 重新放入有效任务
        for (auto& task : valid_tasks) {
            queue.push(std::move(task));
        }
    };

    cleanup_queue(critical_queue_, critical_queue_mutex_, TaskPriority::CRITICAL);
    cleanup_queue(high_queue_, high_queue_mutex_, TaskPriority::HIGH);
    cleanup_queue(normal_queue_, normal_queue_mutex_, TaskPriority::NORMAL);
    cleanup_queue(low_queue_, low_queue_mutex_, TaskPriority::LOW);
    cleanup_queue(background_queue_, background_queue_mutex_, TaskPriority::BACKGROUND);
}

void ThreadPool::wait_all() {
    while (true) {
        {
            std::lock_guard<std::mutex> lock(stats_mutex_);
            if (stats_.total_queued_tasks.load() == 0 && stats_.active_threads.load() == 0) {
                break;
            }
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
}

void ThreadPool::stop() {
    if (!running_.load()) {
        return;
    }

    Logger::info("Stopping Advanced ThreadPool...");

    running_.store(false);
    stop_requested_.store(true);
    condition_.notify_all();

    // 等待所有工作线程结束
    for (auto& worker : workers_) {
        if (worker.joinable()) {
            worker.join();
        }
    }

    // 等待监控线程结束
    if (monitor_.joinable()) {
        monitor_.join();
    }

    // 等待自适应调度线程结束
    if (adaptive_scheduler_.joinable()) {
        adaptive_scheduler_.join();
    }

    Logger::info("Advanced ThreadPool stopped");
}

const ThreadPoolStats* ThreadPool::get_stats() const {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    return &stats_;
}

void ThreadPool::resize(size_t min_threads, size_t max_threads) {
    std::lock_guard<std::mutex> lock(stats_mutex_);

    config_.min_threads = min_threads;
    config_.max_threads = max_threads;

    Logger::info("ThreadPool resized to min: " + std::to_string(min_threads) +
                 ", max: " + std::to_string(max_threads));
}

void ThreadPool::set_resource_limits(uint64_t max_memory_mb, uint64_t max_cpu_percent, uint64_t max_io_ops) {
    config_.max_memory_mb = max_memory_mb;
    config_.max_cpu_percent = max_cpu_percent;
    config_.max_io_operations = max_io_ops;

    Logger::info("Resource limits set - Memory: " + std::to_string(max_memory_mb) + "MB, " +
                 "CPU: " + std::to_string(max_cpu_percent) + "%, " +
                 "IO: " + std::to_string(max_io_ops) + " ops");
}

ResourceUsage ThreadPool::get_resource_usage() const {
    // 创建非原子副本返回
    ResourceUsage usage;
    usage.cpu_time_ms = stats_.resource_usage.cpu_time_ms.load();
    usage.memory_usage_kb = stats_.resource_usage.memory_usage_kb.load();
    usage.io_operations = stats_.resource_usage.io_operations.load();
    usage.network_bytes = stats_.resource_usage.network_bytes.load();
    return usage;
}

} // namespace sealdb
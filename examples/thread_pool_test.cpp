#include "sealdb/thread_pool.h"
#include "sealdb/connection.h"
#include "sealdb/logger.h"
#include <iostream>
#include <chrono>
#include <thread>
#include <random>

using namespace sealdb;

void test_advanced_thread_pool() {
    std::cout << "=== Testing Advanced ThreadPool ===" << std::endl;

    // 创建高级线程池配置
    ThreadPoolConfig config;
    config.min_threads = 4;
    config.max_threads = 16;
    config.queue_size = 1000;

    // 多级队列配置
    config.critical_queue_size = 50;
    config.high_queue_size = 100;
    config.normal_queue_size = 200;
    config.low_queue_size = 100;
    config.background_queue_size = 50;

    // 自适应调度配置
    config.enable_adaptive_scheduling = true;
    config.adjustment_interval = std::chrono::milliseconds(3000);
    config.cpu_threshold_high = 0.7;
    config.cpu_threshold_low = 0.3;
    config.memory_threshold_high = 0.8;
    config.memory_threshold_low = 0.4;

    // 资源限制配置
    config.enable_resource_limits = true;
    config.max_memory_mb = 512;
    config.max_cpu_percent = 70;
    config.max_io_operations = 5000;

    // 监控配置
    config.enable_monitoring = true;
    config.monitor_interval = std::chrono::milliseconds(2000);

    // 任务超时配置
    config.default_task_timeout = std::chrono::milliseconds(10000);
    config.critical_task_timeout = std::chrono::milliseconds(2000);
    config.background_task_timeout = std::chrono::milliseconds(60000);

    // 创建线程池
    ThreadPool pool(config);

    // 设置资源限制
    pool.set_resource_limits(512, 70, 5000);

    std::cout << "ThreadPool created with advanced features" << std::endl;

    // 提交不同优先级的任务
    std::vector<std::future<void>> futures;

    // 提交关键任务（系统任务）
    for (int i = 0; i < 5; ++i) {
        futures.push_back(pool.submit_critical([i]() {
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
            Logger::info("Critical task " + std::to_string(i) + " completed");
        }));
    }

    // 提交高优先级任务（用户查询）
    for (int i = 0; i < 10; ++i) {
        futures.push_back(pool.submit_high_priority([i]() {
            std::this_thread::sleep_for(std::chrono::milliseconds(200));
            Logger::info("High priority task " + std::to_string(i) + " completed");
        }));
    }

    // 提交普通任务
    for (int i = 0; i < 15; ++i) {
        futures.push_back(pool.submit([i]() {
            std::this_thread::sleep_for(std::chrono::milliseconds(300));
            Logger::info("Normal task " + std::to_string(i) + " completed");
        }));
    }

    // 提交低优先级任务
    for (int i = 0; i < 8; ++i) {
        futures.push_back(pool.submit_with_priority([](int id) {
            std::this_thread::sleep_for(std::chrono::milliseconds(400));
            Logger::info("Low priority task " + std::to_string(id) + " completed");
        }, TaskPriority::LOW, TaskType::IO, "Low priority task",
        std::chrono::milliseconds(15000), i));
    }

    // 提交后台任务
    for (int i = 0; i < 6; ++i) {
        futures.push_back(pool.submit_background([i]() {
            std::this_thread::sleep_for(std::chrono::milliseconds(500));
            Logger::info("Background task " + std::to_string(i) + " completed");
        }));
    }

    // 提交一些超时任务用于测试
    for (int i = 0; i < 3; ++i) {
        futures.push_back(pool.submit_with_priority([i]() {
            std::this_thread::sleep_for(std::chrono::milliseconds(3000));
            Logger::info("Long running task " + std::to_string(i) + " completed");
        }, TaskPriority::NORMAL, TaskType::QUERY, "Long running task",
        std::chrono::milliseconds(1000)));
    }

    // 模拟资源密集型任务
    for (int i = 0; i < 4; ++i) {
        futures.push_back(pool.submit([i]() {
            // 模拟CPU密集型操作
            auto start = std::chrono::steady_clock::now();
            while (std::chrono::steady_clock::now() - start < std::chrono::milliseconds(800)) {
                volatile int x = 0;
                for (int j = 0; j < 1000; ++j) {
                    x += j;
                }
            }
            Logger::info("CPU intensive task " + std::to_string(i) + " completed");
        }));
    }

    std::cout << "Submitted " << futures.size() << " tasks with different priorities" << std::endl;

    // 监控线程池状态
    for (int i = 0; i < 10; ++i) {
        std::this_thread::sleep_for(std::chrono::seconds(1));

        std::cout << "\n--- ThreadPool Status (Round " << (i + 1) << ") ---" << std::endl;
        std::cout << "  Total threads: " << pool.get_total_threads() << std::endl;
        std::cout << "  Active threads: " << pool.get_active_threads() << std::endl;
        std::cout << "  Queued tasks: " << pool.get_queued_tasks() << std::endl;
        std::cout << "  Completed tasks: " << pool.get_completed_tasks() << std::endl;
        std::cout << "  Failed tasks: " << pool.get_failed_tasks() << std::endl;

        // 获取详细统计信息
        const auto* stats = pool.get_stats();
        if (stats) {
            std::cout << "  Resource usage:" << std::endl;
            std::cout << "    CPU time: " << stats->resource_usage.cpu_time_ms.load() << "ms" << std::endl;
            std::cout << "    Memory: " << stats->resource_usage.memory_usage_kb.load() << "KB" << std::endl;
            std::cout << "    IO operations: " << stats->resource_usage.io_operations.load() << std::endl;
            std::cout << "    Network bytes: " << stats->resource_usage.network_bytes.load() << std::endl;

            // 显示各队列统计
            for (const auto& [priority, queue_stats] : stats->queue_stats) {
                std::string priority_name;
                switch (priority) {
                    case TaskPriority::CRITICAL: priority_name = "CRITICAL"; break;
                    case TaskPriority::HIGH: priority_name = "HIGH"; break;
                    case TaskPriority::NORMAL: priority_name = "NORMAL"; break;
                    case TaskPriority::LOW: priority_name = "LOW"; break;
                    case TaskPriority::BACKGROUND: priority_name = "BACKGROUND"; break;
                }
                std::cout << "    " << priority_name << " queue: "
                          << queue_stats.queued_tasks.load() << " queued, "
                          << queue_stats.completed_tasks.load() << " completed, "
                          << queue_stats.failed_tasks.load() << " failed" << std::endl;
            }
        }
    }

    // 等待所有任务完成
    std::cout << "\nWaiting for all tasks to complete..." << std::endl;
    for (auto& future : futures) {
        try {
            future.wait();
        } catch (const std::exception& e) {
            std::cout << "Task failed with exception: " << e.what() << std::endl;
        }
    }

    // 获取最终统计信息
    std::cout << "\n--- Final ThreadPool Statistics ---" << std::endl;
    std::cout << "  Total threads: " << pool.get_total_threads() << std::endl;
    std::cout << "  Active threads: " << pool.get_active_threads() << std::endl;
    std::cout << "  Queued tasks: " << pool.get_queued_tasks() << std::endl;
    std::cout << "  Completed tasks: " << pool.get_completed_tasks() << std::endl;
    std::cout << "  Failed tasks: " << pool.get_failed_tasks() << std::endl;

    const auto* final_stats = pool.get_stats();
    if (final_stats) {
        std::cout << "  Total timeout tasks: " << final_stats->total_timeout_tasks.load() << std::endl;
        std::cout << "  Total resource usage:" << std::endl;
        std::cout << "    CPU time: " << final_stats->resource_usage.cpu_time_ms.load() << "ms" << std::endl;
        std::cout << "    Memory: " << final_stats->resource_usage.memory_usage_kb.load() << "KB" << std::endl;
        std::cout << "    IO operations: " << final_stats->resource_usage.io_operations.load() << std::endl;
        std::cout << "    Network bytes: " << final_stats->resource_usage.network_bytes.load() << std::endl;
    }

    std::cout << "Advanced ThreadPool test completed successfully!" << std::endl;
}

void test_connection_manager() {
    std::cout << "\n=== Testing ConnectionManager ===" << std::endl;

    // 创建连接管理器
    ConnectionManager manager(100);

    // 模拟一些连接
    for (int i = 0; i < 5; ++i) {
        // 创建连接配置
        ConnectionConfig config;
        config.host = "127.0.0.1";
        config.port = 3306 + i;
        config.timeout = std::chrono::milliseconds(30000);

        // 创建连接
        auto conn = manager.create_connection(config);
        if (conn) {
            std::cout << "Created connection: " << conn->get_connection_id() << std::endl;

            // 注册连接
            manager.register_connection(conn);

            // 模拟连接活动
            conn->update_last_activity();
            conn->increment_bytes_sent(1024);
            conn->increment_bytes_received(2048);
        }
    }

    // 获取连接统计信息
    auto manager_stats = manager.get_stats();
    std::cout << "Connection Manager Statistics:" << std::endl;
    std::cout << "  Total connections: " << manager_stats.total_connections << std::endl;
    std::cout << "  Active connections: " << manager_stats.active_connections << std::endl;
    std::cout << "  Idle connections: " << manager_stats.idle_connections << std::endl;
    std::cout << "  Failed connections: " << manager_stats.failed_connections << std::endl;

    // 列出所有连接
    auto connections = manager.get_all_connections();
    for (const auto& conn : connections) {
        auto stats = conn->get_stats();
        std::cout << "  Connection " << conn->get_connection_id() << ":" << std::endl;
        std::cout << "    State: " << static_cast<int>(conn->get_state()) << std::endl;
        std::cout << "    Bytes sent: " << stats.bytes_sent << std::endl;
        std::cout << "    Bytes received: " << stats.bytes_received << std::endl;
        std::cout << "    Last activity: " << std::chrono::duration_cast<std::chrono::seconds>(
            std::chrono::steady_clock::now() - stats.last_activity).count() << "s ago" << std::endl;
    }

    // 关闭一些连接
    if (!connections.empty()) {
        manager.unregister_connection(connections[0]->get_connection_id());
        if (connections.size() > 2) {
            manager.unregister_connection(connections[2]->get_connection_id());
        }
    }

    manager_stats = manager.get_stats();
    std::cout << "After closing connections: " << manager_stats.total_connections << " remaining" << std::endl;

    std::cout << "ConnectionManager test completed" << std::endl;
}

int main() {
    try {
        test_advanced_thread_pool();
        test_connection_manager();

        std::cout << "\nAll tests completed successfully!" << std::endl;
        return 0;
    } catch (const std::exception& e) {
        std::cerr << "Test failed with exception: " << e.what() << std::endl;
        return 1;
    }
}
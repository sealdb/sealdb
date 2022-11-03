#include <iostream>
#include <signal.h>
#include <memory>
#include <thread>
#include <chrono>
#include "sealdb/sealdb.h"

using namespace sealdb;

std::unique_ptr<SealDB> g_sealdb;

void signal_handler(int signal) {
    std::cout << "收到信号 " << signal << "，正在关闭 SealDB..." << std::endl;
    if (g_sealdb) {
        g_sealdb->stop();
    }
    exit(0);
}

int main(int argc, char* argv[]) {
    // 设置信号处理
    signal(SIGINT, signal_handler);
    signal(SIGTERM, signal_handler);

    try {
        // 创建 SealDB 实例
        g_sealdb = std::make_unique<SealDB>();

        // 加载配置
        Config config;
        if (argc > 1) {
            config.load_from_file(argv[1]);
        } else {
            config.load_from_env();
        }

        // 初始化数据库
        auto init_result = g_sealdb->initialize(config);
        if (init_result != ErrorCode::SUCCESS) {
            std::cerr << "初始化 SealDB 失败" << std::endl;
            return 1;
        }

        std::cout << "SealDB 初始化成功" << std::endl;

        // 启动数据库服务
        auto start_result = g_sealdb->start();
        if (start_result != ErrorCode::SUCCESS) {
            std::cerr << "启动 SealDB 失败" << std::endl;
            return 1;
        }

        std::cout << "SealDB 启动成功，正在运行..." << std::endl;

        // 等待信号
        while (true) {
            std::this_thread::sleep_for(std::chrono::seconds(1));
        }

    } catch (const std::exception& e) {
        std::cerr << "SealDB 运行异常: " << e.what() << std::endl;
        return 1;
    }

    return 0;
}
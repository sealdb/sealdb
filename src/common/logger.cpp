#include "logger.h"
#include <iostream>
#include <chrono>
#include <iomanip>

namespace sealdb {

void Logger::log(LogLevel level, const std::string& message) {
    auto now = std::chrono::system_clock::now();
    auto time_t = std::chrono::system_clock::to_time_t(now);

    std::cout << std::put_time(std::localtime(&time_t), "%Y-%m-%d %H:%M:%S") << " ";

    switch (level) {
        case LogLevel::DEBUG:
            std::cout << "[DEBUG] ";
            break;
        case LogLevel::INFO:
            std::cout << "[INFO] ";
            break;
        case LogLevel::WARN:
            std::cout << "[WARN] ";
            break;
        case LogLevel::ERROR:
            std::cout << "[ERROR] ";
            break;
    }

    std::cout << message << std::endl;
}

void Logger::debug(const std::string& message) {
    log(LogLevel::DEBUG, message);
}

void Logger::info(const std::string& message) {
    log(LogLevel::INFO, message);
}

void Logger::warn(const std::string& message) {
    log(LogLevel::WARN, message);
}

void Logger::error(const std::string& message) {
    log(LogLevel::ERROR, message);
}

} // namespace sealdb
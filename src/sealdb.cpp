#include "sealdb/sealdb.h"
#include "sealdb/logger.h"
#include <iostream>

namespace sealdb {

class SealDB::Impl {
public:
    bool initialized_ = false;
    bool running_ = false;
    Config config_;
};

SealDB::SealDB() : pimpl_(std::make_unique<Impl>()) {
}

SealDB::~SealDB() = default;

ErrorCode SealDB::initialize(const Config& config) {
    pimpl_->config_ = config;
    pimpl_->initialized_ = true;
    Logger::info("SealDB 初始化成功");
    return ErrorCode::SUCCESS;
}

ErrorCode SealDB::start() {
    if (!pimpl_->initialized_) {
        Logger::error("SealDB 未初始化");
        return ErrorCode::INVALID_ARGUMENT;
    }

    pimpl_->running_ = true;
    Logger::info("SealDB 启动成功");
    return ErrorCode::SUCCESS;
}

ErrorCode SealDB::stop() {
    pimpl_->running_ = false;
    Logger::info("SealDB 已停止");
    return ErrorCode::SUCCESS;
}

Result<std::string> SealDB::execute(const std::string& sql) {
    if (!pimpl_->running_) {
        return Result<std::string>(Error(ErrorCode::INVALID_ARGUMENT, "SealDB 未运行"));
    }

    Logger::info("执行 SQL: " + sql);
    return Result<std::string>("OK");
}

} // namespace sealdb
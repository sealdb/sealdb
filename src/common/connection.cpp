#include "sealdb/connection.h"
#include "sealdb/logger.h"
#include <sstream>
#include <unordered_map>

namespace sealdb {

// 静态成员初始化
std::atomic<uint64_t> Connection::next_connection_id_{1};

Connection::Connection(const ConnectionConfig& config)
    : config_(config), connection_id_(next_connection_id_++) {

    Logger::debug("Connection created: " + std::to_string(connection_id_));
}

Connection::~Connection() {
    if (get_state() != ConnectionState::DISCONNECTED) {
        disconnect();
    }
    Logger::debug("Connection destroyed: " + std::to_string(connection_id_));
}

ConnectionStats Connection::get_stats() const {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    return stats_;
}

void Connection::set_state(ConnectionState new_state) {
    ConnectionState old_state = state_.load();
    state_.store(new_state);

    if (state_change_callback_) {
        state_change_callback_(old_state, new_state);
    }

    Logger::debug("Connection " + std::to_string(connection_id_) +
                 " state changed: " + std::to_string(static_cast<int>(old_state)) +
                 " -> " + std::to_string(static_cast<int>(new_state)));
}

void Connection::update_last_activity() {
    stats_.last_activity = std::chrono::steady_clock::now();
}

void Connection::increment_bytes_sent(size_t bytes) {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    stats_.bytes_sent += bytes;
    update_last_activity();
}

void Connection::increment_bytes_received(size_t bytes) {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    stats_.bytes_received += bytes;
    update_last_activity();
}

std::string Connection::get_connection_string() const {
    std::ostringstream oss;
    oss << config_.host << ":" << config_.port;
    if (!config_.database.empty()) {
        oss << "/" << config_.database;
    }
    return oss.str();
}

// ConnectionManager 实现
ConnectionManager::ConnectionManager(size_t max_connections)
    : max_connections_(max_connections) {

    Logger::info("ConnectionManager initialized with max connections: " + std::to_string(max_connections));
}

ConnectionManager::~ConnectionManager() {
    close_all_connections();
}

std::shared_ptr<Connection> ConnectionManager::create_connection(const ConnectionConfig& config) {
    // 这里应该根据配置创建具体的连接类型
    // 目前返回 nullptr，需要子类实现
    (void)config; // 标记参数为已使用，避免编译告警
    Logger::warn("create_connection not implemented");
    return nullptr;
}

ErrorCode ConnectionManager::register_connection(std::shared_ptr<Connection> connection) {
    if (!connection) {
        return ErrorCode::INVALID_ARGUMENT;
    }

    std::lock_guard<std::mutex> lock(connections_mutex_);

    if (connections_.size() >= max_connections_) {
        Logger::warn("Connection limit reached: " + std::to_string(max_connections_));
        return ErrorCode::PERMISSION_DENIED;
    }

    uint64_t connection_id = connection->get_connection_id();
    connections_[connection_id] = connection;
    total_connections_++;

    Logger::info("Connection registered: " + std::to_string(connection_id) +
                 " (total: " + std::to_string(connections_.size()) + ")");

    return ErrorCode::SUCCESS;
}

ErrorCode ConnectionManager::unregister_connection(uint64_t connection_id) {
    std::lock_guard<std::mutex> lock(connections_mutex_);

    auto it = connections_.find(connection_id);
    if (it == connections_.end()) {
        return ErrorCode::NOT_FOUND;
    }

    connections_.erase(it);
    total_connections_--;

    Logger::info("Connection unregistered: " + std::to_string(connection_id) +
                 " (total: " + std::to_string(connections_.size()) + ")");

    return ErrorCode::SUCCESS;
}

std::shared_ptr<Connection> ConnectionManager::get_connection(uint64_t connection_id) {
    std::lock_guard<std::mutex> lock(connections_mutex_);

    auto it = connections_.find(connection_id);
    if (it == connections_.end()) {
        return nullptr;
    }

    return it->second;
}

std::vector<std::shared_ptr<Connection>> ConnectionManager::get_all_connections() const {
    std::lock_guard<std::mutex> lock(connections_mutex_);

    std::vector<std::shared_ptr<Connection>> result;
    result.reserve(connections_.size());

    for (const auto& pair : connections_) {
        result.push_back(pair.second);
    }

    return result;
}

void ConnectionManager::close_all_connections() {
    std::lock_guard<std::mutex> lock(connections_mutex_);

    Logger::info("Closing all connections...");

    for (auto& pair : connections_) {
        auto connection = pair.second;
        if (connection && connection->is_valid()) {
            connection->disconnect();
        }
    }

    connections_.clear();
    total_connections_ = 0;
    active_connections_ = 0;

    Logger::info("All connections closed");
}

ConnectionManager::ManagerStats ConnectionManager::get_stats() const {
    std::lock_guard<std::mutex> lock(connections_mutex_);

    ManagerStats stats;
    stats.total_connections = connections_.size();

    for (const auto& pair : connections_) {
        auto connection = pair.second;
        if (connection) {
            auto state = connection->get_state();
            if (state == ConnectionState::READY || state == ConnectionState::BUSY) {
                stats.active_connections++;
            } else if (state == ConnectionState::ERROR) {
                stats.failed_connections++;
            } else {
                stats.idle_connections++;
            }
        }
    }

    return stats;
}

} // namespace sealdb
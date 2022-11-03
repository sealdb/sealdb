#pragma once

#include "protocol.h"
#include <memory>
#include <map>
#include <vector>
#include <functional>

namespace sealdb {

// 协议配置
struct ProtocolConfig {
    ProtocolType type;
    uint16_t port;
    bool enabled;
    uint32_t max_connections;
    uint32_t timeout_ms;

    ProtocolConfig(ProtocolType t = ProtocolType::MYSQL, uint16_t p = 3306)
        : type(t), port(p), enabled(true), max_connections(1000), timeout_ms(30000) {}
};

// 协议统计信息
struct ProtocolStats {
    uint64_t total_connections;
    uint64_t active_connections;
    uint64_t total_requests;
    uint64_t total_errors;
    uint64_t bytes_received;
    uint64_t bytes_sent;
    std::chrono::steady_clock::time_point start_time;

    ProtocolStats() : total_connections(0), active_connections(0),
                     total_requests(0), total_errors(0),
                     bytes_received(0), bytes_sent(0) {}
};

// 协议管理器
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

private:
    std::map<ProtocolType, ProtocolConfig> configs_;
    std::map<ProtocolType, std::unique_ptr<ProtocolHandler>> handlers_;
    std::map<ProtocolType, ProtocolStats> stats_;
    bool initialized_;

    // 内部方法
    ErrorCode validate_config(const ProtocolConfig& config);
    void update_connection_stats(ProtocolType type, bool connected);
    void update_request_stats(ProtocolType type, bool success);
    void update_bytes_stats(ProtocolType type, uint64_t received, uint64_t sent);
};

} // namespace sealdb
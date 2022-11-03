#pragma once

#include "protocol.h"
#include <map>

namespace sealdb {

class PostgreSQLProtocolHandler : public ProtocolHandler {
public:
    PostgreSQLProtocolHandler();
    ~PostgreSQLProtocolHandler() override = default;

    // 协议类型
    ProtocolType get_protocol_type() const override { return ProtocolType::POSTGRESQL; }
    ProtocolVersion get_protocol_version() const override { return ProtocolVersion(3, 0, 0); }

    // 连接管理
    ErrorCode handle_connection(const std::string& client_data) override;
    ErrorCode handle_disconnection() override;
    ConnectionState get_connection_state() const override { return state_; }

    // 认证处理
    ErrorCode handle_authentication(const AuthInfo& auth_info) override;
    bool is_authenticated() const override { return authenticated_; }

    // 查询处理
    ErrorCode handle_query(const QueryRequest& request, QueryResponse& response) override;
    ErrorCode handle_prepared_statement(const QueryRequest& request, QueryResponse& response) override;

    // 事务处理
    ErrorCode handle_begin_transaction() override;
    ErrorCode handle_commit_transaction() override;
    ErrorCode handle_rollback_transaction() override;

    // 数据序列化/反序列化
    std::string serialize_response(const QueryResponse& response) override;
    ErrorCode deserialize_request(const std::string& data, QueryRequest& request) override;

    // 错误处理
    std::string format_error_message(ErrorCode code, const std::string& message) override;

    // 统计信息
    uint64_t get_requests_processed() const override { return requests_processed_; }
    uint64_t get_bytes_received() const override { return bytes_received_; }
    uint64_t get_bytes_sent() const override { return bytes_sent_; }

private:
    ConnectionState state_;
    bool authenticated_;
    uint32_t process_id_;
    std::string user_;
    std::string database_;
    std::map<std::string, std::string> parameters_;

    // 统计信息
    uint64_t requests_processed_;
    uint64_t bytes_received_;
    uint64_t bytes_sent_;

    // PostgreSQL 特定方法
    std::string create_startup_message();
    ErrorCode parse_startup_message(const std::string& data);
    std::string create_authentication_ok();
    std::string create_ready_for_query();
    std::string create_error_response(const std::string& message);
    std::string create_row_description(const QueryResponse& response);
    std::string create_data_row(const QueryResponse& response);
    std::string create_command_complete(const std::string& tag);
};

} // namespace sealdb
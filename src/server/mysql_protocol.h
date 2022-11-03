#pragma once

#include "protocol.h"
#include <map>

namespace sealdb {

class MySQLProtocolHandler : public ProtocolHandler {
public:
    MySQLProtocolHandler();
    ~MySQLProtocolHandler() override = default;

    // 协议类型
    ProtocolType get_protocol_type() const override { return ProtocolType::MYSQL; }
    ProtocolVersion get_protocol_version() const override { return ProtocolVersion(5, 7, 0); }

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
    uint32_t connection_id_;
    uint32_t capabilities_;
    std::string server_version_;
    std::string charset_;

    // 统计信息
    uint64_t requests_processed_;
    uint64_t bytes_received_;
    uint64_t bytes_sent_;

    // MySQL 特定方法
    std::string create_handshake_packet();
    ErrorCode parse_handshake_response(const std::string& data);
    std::string create_ok_packet(uint8_t header, uint64_t affected_rows = 0);
    std::string create_error_packet(uint16_t error_code, const std::string& message);
    std::string create_eof_packet(uint16_t status_flags = 0);
    std::string create_result_set_packet(const QueryResponse& response);
};

} // namespace sealdb
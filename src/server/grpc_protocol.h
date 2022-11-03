#pragma once

#include "protocol.h"
#include <map>

namespace sealdb {

class GRPCProtocolHandler : public ProtocolHandler {
public:
    GRPCProtocolHandler();
    ~GRPCProtocolHandler() override = default;

    // 协议类型
    ProtocolType get_protocol_type() const override { return ProtocolType::GRPC; }
    ProtocolVersion get_protocol_version() const override { return ProtocolVersion(1, 0, 0); }

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
    std::string service_name_;
    std::string method_name_;
    std::map<std::string, std::string> metadata_;

    // 统计信息
    uint64_t requests_processed_;
    uint64_t bytes_received_;
    uint64_t bytes_sent_;

    // gRPC 特定方法
    std::string create_grpc_frame(const std::string& data, bool compressed = false);
    ErrorCode parse_grpc_frame(const std::string& data, std::string& payload);
    std::string create_grpc_response(const QueryResponse& response);
    std::string create_grpc_error_response(ErrorCode code, const std::string& message);
    ErrorCode parse_grpc_request(const std::string& data, QueryRequest& request);
    std::string serialize_protobuf_message(const QueryResponse& response);
    ErrorCode deserialize_protobuf_message(const std::string& data, QueryRequest& request);
};

} // namespace sealdb
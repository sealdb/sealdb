#pragma once

#include <memory>
#include <string>
#include <vector>
#include <functional>
#include "sealdb/error.h"
#include "sealdb/result.h"

namespace sealdb {

// 协议类型枚举
enum class ProtocolType {
    MYSQL = 0,
    POSTGRESQL = 1,
    GRPC = 2,
    HTTP = 3
};

// 协议版本
struct ProtocolVersion {
    uint8_t major;
    uint8_t minor;
    uint8_t patch;

    ProtocolVersion(uint8_t m = 1, uint8_t mi = 0, uint8_t p = 0)
        : major(m), minor(mi), patch(p) {}

    std::string to_string() const {
        return std::to_string(major) + "." +
               std::to_string(minor) + "." +
               std::to_string(patch);
    }
};

// 连接状态
enum class ConnectionState {
    DISCONNECTED = 0,
    CONNECTING = 1,
    CONNECTED = 2,
    AUTHENTICATING = 3,
    READY = 4,
    ERROR = 5
};

// 认证信息
struct AuthInfo {
    std::string username;
    std::string password;
    std::string database;
    std::string charset;
    uint32_t capabilities;

    AuthInfo() : capabilities(0) {}
};

// 查询请求
struct QueryRequest {
    std::string sql;
    std::vector<std::string> parameters;
    uint32_t timeout_ms;
    bool is_prepared;

    QueryRequest() : timeout_ms(30000), is_prepared(false) {}
};

// 查询响应
struct QueryResponse {
    std::string result_data;
    uint64_t affected_rows;
    uint64_t insert_id;
    uint16_t status_flags;
    std::string error_message;
    ErrorCode error_code;

    QueryResponse() : affected_rows(0), insert_id(0), status_flags(0), error_code(ErrorCode::SUCCESS) {}
};

// 协议处理器接口
class ProtocolHandler {
public:
    virtual ~ProtocolHandler() = default;

    // 协议类型
    virtual ProtocolType get_protocol_type() const = 0;
    virtual ProtocolVersion get_protocol_version() const = 0;

    // 连接管理
    virtual ErrorCode handle_connection(const std::string& client_data) = 0;
    virtual ErrorCode handle_disconnection() = 0;
    virtual ConnectionState get_connection_state() const = 0;

    // 认证处理
    virtual ErrorCode handle_authentication(const AuthInfo& auth_info) = 0;
    virtual bool is_authenticated() const = 0;

    // 查询处理
    virtual ErrorCode handle_query(const QueryRequest& request, QueryResponse& response) = 0;
    virtual ErrorCode handle_prepared_statement(const QueryRequest& request, QueryResponse& response) = 0;

    // 事务处理
    virtual ErrorCode handle_begin_transaction() = 0;
    virtual ErrorCode handle_commit_transaction() = 0;
    virtual ErrorCode handle_rollback_transaction() = 0;

    // 数据序列化/反序列化
    virtual std::string serialize_response(const QueryResponse& response) = 0;
    virtual ErrorCode deserialize_request(const std::string& data, QueryRequest& request) = 0;

    // 错误处理
    virtual std::string format_error_message(ErrorCode code, const std::string& message) = 0;

    // 统计信息
    virtual uint64_t get_requests_processed() const = 0;
    virtual uint64_t get_bytes_received() const = 0;
    virtual uint64_t get_bytes_sent() const = 0;
};

// 协议工厂
class ProtocolFactory {
public:
    static std::unique_ptr<ProtocolHandler> create_handler(ProtocolType type);
    static std::string get_protocol_name(ProtocolType type);
    static bool is_protocol_supported(ProtocolType type);
};

} // namespace sealdb
#ifndef SEALDB_ERROR_H
#define SEALDB_ERROR_H

#include <string>
#include <system_error>

namespace sealdb {

/**
 * @brief 错误码枚举
 */
enum class ErrorCode {
    SUCCESS = 0,
    INVALID_ARGUMENT,
    CONNECTION_FAILED,
    TIMEOUT,
    NOT_FOUND,
    ALREADY_EXISTS,
    PERMISSION_DENIED,
    INTERNAL_ERROR,
    UNKNOWN_ERROR
};

/**
 * @brief 错误信息类
 */
class Error {
public:
    Error(ErrorCode code, const std::string& message = "");

    ErrorCode code() const { return code_; }
    const std::string& message() const { return message_; }

    bool is_success() const { return code_ == ErrorCode::SUCCESS; }
    bool is_error() const { return !is_success(); }

    std::string to_string() const;

private:
    ErrorCode code_;
    std::string message_;
};

/**
 * @brief 结果类，包含错误信息和数据
 */
template<typename T>
class Result {
public:
    Result(const T& data) : data_(data), error_(ErrorCode::SUCCESS) {}
    Result(const Error& error) : error_(error) {}

    bool is_success() const { return error_.is_success(); }
    bool is_error() const { return error_.is_error(); }

    const T& data() const { return data_; }
    const Error& error() const { return error_; }

    T& data() { return data_; }
    Error& error() { return error_; }

private:
    T data_;
    Error error_;
};

} // namespace sealdb

#endif // SEALDB_ERROR_H
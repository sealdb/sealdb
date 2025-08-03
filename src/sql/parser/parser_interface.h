#ifndef SEALDB_PARSER_INTERFACE_H
#define SEALDB_PARSER_INTERFACE_H

#include <string>
#include <vector>
#include <memory>

namespace sealdb {

/**
 * 解析错误信息
 */
struct ParseError {
    std::string message;
    int line;
    int column;

    ParseError(const std::string& msg, int l = 0, int c = 0)
        : message(msg), line(l), column(c) {}
};

/**
 * 解析结果
 */
struct ParseResult {
    std::shared_ptr<void> ast;  // 抽象语法树
    std::vector<ParseError> errors;
    bool success;

    ParseResult() : success(false) {}

    ParseResult(std::shared_ptr<void> ast_ptr)
        : ast(ast_ptr), success(true) {}

    ParseResult(const std::vector<ParseError>& errs)
        : errors(errs), success(false) {}
};

/**
 * 解析器接口
 * 定义所有SQL解析器必须实现的接口
 */
class ParserInterface {
public:
    virtual ~ParserInterface() = default;

    /**
     * 解析SQL语句
     * @param sql SQL语句字符串
     * @return 解析结果
     */
    virtual ParseResult parse(const std::string& sql) = 0;

    /**
     * 获取解析器名称
     * @return 解析器名称
     */
    virtual std::string getName() const = 0;

    /**
     * 检查解析器是否可用
     * @return 是否可用
     */
    virtual bool isAvailable() const = 0;
};

} // namespace sealdb

#endif // SEALDB_PARSER_INTERFACE_H
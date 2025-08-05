#ifndef SEALDB_SEAL_PARSER_H
#define SEALDB_SEAL_PARSER_H

#include "../parser_interface.h"
#include "ast.h"
#include "lexer.h"
#include "parser.h"
#include <memory>
#include <string>

namespace sealdb {

/**
 * @brief Seal解析器
 * 
 * 基于递归下降算法的SQL解析器实现
 */
class SealParser : public ParserInterface {
public:
    SealParser() = default;
    ~SealParser() override = default;

    /**
     * @brief 解析SQL语句
     * @param sql SQL语句字符串
     * @return 解析结果
     */
    ParseResult parse(const std::string& sql) override;

    /**
     * @brief 获取解析器名称
     * @return 解析器名称
     */
    std::string getName() const override { return "SealParser"; }

    /**
     * @brief 检查解析器是否可用
     * @return 是否可用
     */
    bool isAvailable() const override { return true; }

private:
    // 将ParseResult中的ast转换为Statement
    std::shared_ptr<Statement> convertToStatement(std::shared_ptr<void> ast);
};

} // namespace sealdb

#endif // SEALDB_SEAL_PARSER_H 
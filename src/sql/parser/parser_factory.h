#ifndef SEALDB_PARSER_FACTORY_H
#define SEALDB_PARSER_FACTORY_H

#include <memory>
#include <string>
#include "parser_interface.h"

namespace sealdb {

/**
 * 解析器类型枚举
 */
enum class ParserType {
    ANTLR4,         // ANTLR4解析器
    POSTGRESQL,     // PostgreSQL风格解析器 (Flex + Bison)
    SEAL            // Seal解析器 (递归下降算法)
};

/**
 * 解析器工厂类
 * 用于创建和管理不同类型的SQL解析器
 */
class ParserFactory {
public:
    /**
     * 创建解析器
     * @param type 解析器类型
     * @return 解析器指针
     */
    static std::unique_ptr<ParserInterface> createParser(ParserType type);

    /**
     * 创建默认解析器 (ANTLR4)
     * @return 解析器指针
     */
    static std::unique_ptr<ParserInterface> createDefaultParser();

    /**
     * 根据配置创建解析器
     * @param config 配置字符串 ("antlr4", "postgresql" 或 "seal")
     * @return 解析器指针
     */
    static std::unique_ptr<ParserInterface> createParserFromConfig(const std::string& config);

    /**
     * 获取可用的解析器类型列表
     * @return 解析器类型列表
     */
    static std::vector<ParserType> getAvailableParserTypes();

    /**
     * 检查解析器类型是否可用
     * @param type 解析器类型
     * @return 是否可用
     */
    static bool isParserTypeAvailable(ParserType type);

private:
    ParserFactory() = delete;
    ~ParserFactory() = delete;
};

} // namespace sealdb

#endif // SEALDB_PARSER_FACTORY_H
#include "parser_factory.h"
#include "antlr4/antlr4_parser.h"
#include <vector>

namespace sealdb {

std::unique_ptr<ParserInterface> ParserFactory::createParser(ParserType type) {
    switch (type) {
        case ParserType::ANTLR4:
            return std::make_unique<Antlr4Parser>();
        case ParserType::POSTGRESQL:
            // PostgreSQL风格解析器暂未实现，返回nullptr
            return nullptr;
        default:
            return nullptr;
    }
}

std::unique_ptr<ParserInterface> ParserFactory::createDefaultParser() {
    return createParser(ParserType::ANTLR4);
}

std::unique_ptr<ParserInterface> ParserFactory::createParserFromConfig(const std::string& config) {
    if (config == "antlr4") {
        return createParser(ParserType::ANTLR4);
    } else if (config == "postgresql") {
        return createParser(ParserType::POSTGRESQL);
    } else {
        // 默认使用ANTLR4
        return createDefaultParser();
    }
}

std::vector<ParserType> ParserFactory::getAvailableParserTypes() {
    std::vector<ParserType> availableTypes;

    // 检查ANTLR4解析器是否可用
    if (isParserTypeAvailable(ParserType::ANTLR4)) {
        availableTypes.push_back(ParserType::ANTLR4);
    }

    // 检查PostgreSQL解析器是否可用
    if (isParserTypeAvailable(ParserType::POSTGRESQL)) {
        availableTypes.push_back(ParserType::POSTGRESQL);
    }

    return availableTypes;
}

bool ParserFactory::isParserTypeAvailable(ParserType type) {
    switch (type) {
        case ParserType::ANTLR4:
            // 检查ANTLR4运行时是否可用
            // 这里可以添加更详细的检查逻辑
            return true;
        case ParserType::POSTGRESQL:
            // 检查Flex和Bison是否可用
            // 这里可以添加更详细的检查逻辑
            return false; // 暂未实现
        default:
            return false;
    }
}

} // namespace sealdb
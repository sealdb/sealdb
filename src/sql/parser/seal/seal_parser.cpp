#include "seal_parser.h"
#include <sstream>

namespace sealdb {

ParseResult SealParser::parse(const std::string& sql) {
    try {
        // 创建解析器
        Parser parser(sql);

        // 解析SQL语句
        auto statement = parser.parse();

        if (statement && !parser.has_error()) {
            // 解析成功
            return ParseResult(std::static_pointer_cast<void>(statement));
        } else {
            // 解析失败
            std::vector<ParseError> errors;
            if (!parser.get_error().empty()) {
                errors.emplace_back(parser.get_error(), 0, 0);
            }
            return ParseResult(errors);
        }
    } catch (const std::exception& e) {
        // 异常处理
        std::vector<ParseError> errors;
        errors.emplace_back("Parser exception: " + std::string(e.what()), 0, 0);
        return ParseResult(errors);
    }
}

std::shared_ptr<Statement> SealParser::convertToStatement(std::shared_ptr<void> ast) {
    return std::static_pointer_cast<Statement>(ast);
}

} // namespace sealdb
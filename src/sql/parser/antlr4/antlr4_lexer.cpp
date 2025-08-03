#include "antlr4_lexer.h"
#include <sstream>

namespace sealdb {

Antlr4Lexer::Antlr4Lexer() {
    errorListener_ = std::make_unique<ErrorListener>(this);
}

Antlr4Lexer::~Antlr4Lexer() = default;

std::unique_ptr<antlr4::CommonTokenStream> Antlr4Lexer::tokenize(const std::string& input) {
    clearErrors();

    try {
        // 创建输入流
        antlr4::ANTLRInputStream inputStream(input);

        // 创建词法分析器
        SQLLexer lexer(&inputStream);
        lexer.removeErrorListeners();
        lexer.addErrorListener(errorListener_.get());

        // 创建词法符号流
        auto tokens = std::make_unique<antlr4::CommonTokenStream>(&lexer);
        tokens->fill();

        return tokens;

    } catch (const std::exception& e) {
        addError("词法分析异常: " + std::string(e.what()));
        return nullptr;
    }
}

std::vector<antlr4::Token*> Antlr4Lexer::getTokens(const std::string& input) {
    std::vector<antlr4::Token*> tokens;

    auto tokenStream = tokenize(input);
    if (!tokenStream) {
        return tokens;
    }

    // 获取所有词法符号
    for (auto token : tokenStream->getTokens()) {
        if (token->getType() != antlr4::Token::EOF) {
            tokens.push_back(token);
        }
    }

    return tokens;
}

void Antlr4Lexer::addError(const std::string& error) {
    errors_.push_back(error);
}

void Antlr4Lexer::ErrorListener::syntaxError(antlr4::Recognizer* recognizer, antlr4::Token* offendingSymbol,
                                            size_t line, size_t charPositionInLine, const std::string& msg,
                                            std::exception_ptr e) {
    std::ostringstream oss;
    oss << "词法错误 (行 " << line << ", 列 " << charPositionInLine << "): " << msg;
    lexer_->addError(oss.str());
}

} // namespace sealdb
#ifndef SEALDB_ANTLR4_LEXER_H
#define SEALDB_ANTLR4_LEXER_H

#include <string>
#include <vector>
#include <memory>
#include <antlr4-runtime.h>
#include "SQLLexer.h"

namespace sealdb {

/**
 * ANTLR4 SQL词法分析器
 * 提供基于ANTLR4的SQL词法分析功能
 */
class Antlr4Lexer {
public:
    Antlr4Lexer();
    ~Antlr4Lexer();

    /**
     * 词法分析
     * @param input 输入字符串
     * @return 词法符号流
     */
    std::unique_ptr<antlr4::CommonTokenStream> tokenize(const std::string& input);

    /**
     * 获取词法符号列表
     * @param input 输入字符串
     * @return 词法符号列表
     */
    std::vector<antlr4::Token*> getTokens(const std::string& input);

    /**
     * 获取词法错误信息
     * @return 错误信息列表
     */
    const std::vector<std::string>& getErrors() const { return errors_; }

    /**
     * 清除错误信息
     */
    void clearErrors() { errors_.clear(); }

private:
    /**
     * 自定义错误监听器
     */
    class ErrorListener : public antlr4::BaseErrorListener {
    public:
        ErrorListener(Antlr4Lexer* lexer) : lexer_(lexer) {}

        void syntaxError(antlr4::Recognizer* recognizer, antlr4::Token* offendingSymbol,
                        size_t line, size_t charPositionInLine, const std::string& msg,
                        std::exception_ptr e) override;

    private:
        Antlr4Lexer* lexer_;
    };

    /**
     * 添加错误信息
     * @param error 错误信息
     */
    void addError(const std::string& error);

private:
    std::vector<std::string> errors_;
    std::unique_ptr<ErrorListener> errorListener_;
};

} // namespace sealdb

#endif // SEALDB_ANTLR4_LEXER_H
#ifndef SEALDB_ANTLR4_PARSER_H
#define SEALDB_ANTLR4_PARSER_H

#include <memory>
#include <string>
#include <vector>

// 检查ANTLR4是否可用
#ifdef USE_ANTLR4_PARSER
// 尝试不同的头文件包含方式
#if __has_include(<antlr4-runtime.h>)
#include <antlr4-runtime.h>
#elif __has_include(<antlr4-runtime/antlr4-runtime.h>)
#include <antlr4-runtime/antlr4-runtime.h>
#else
#warning "ANTLR4 runtime headers not found, falling back to basic implementation"
#define ANTLR4_NOT_AVAILABLE
#endif

// 只有在找到ANTLR4头文件时才包含生成的解析器头文件
#ifndef ANTLR4_NOT_AVAILABLE
#include "SQLParser.h"
#include "SQLLexer.h"
#endif
#endif

#include "ast.h"
#include "../parser_interface.h"

namespace sealdb {

/**
 * ANTLR4 SQL解析器
 * 提供基于ANTLR4的SQL语句解析功能
 */
class Antlr4Parser : public ParserInterface {
public:
    Antlr4Parser();
    ~Antlr4Parser();

    /**
     * 解析SQL语句
     * @param sql SQL语句字符串
     * @return 解析结果，包含AST或错误信息
     */
    ParseResult parse(const std::string& sql) override;

    /**
     * 获取解析器名称
     * @return 解析器名称
     */
    std::string getName() const override { return "ANTLR4 Parser"; }

    /**
     * 检查解析器是否可用
     * @return 是否可用
     */
    bool isAvailable() const override { return true; }

    /**
     * 解析SQL语句并返回AST
     * @param sql SQL语句字符串
     * @return AST节点指针
     */
    std::shared_ptr<ASTNode> parseToAST(const std::string& sql);

    /**
     * 获取解析错误信息
     * @return 错误信息列表
     */
    const std::vector<std::string>& getErrors() const { return errors_; }

    /**
     * 清除错误信息
     */
    void clearErrors() { errors_.clear(); }

private:
    /**
     * 将ANTLR4语法树转换为SealDB AST
     * @param tree ANTLR4语法树
     * @return SealDB AST节点
     */
    std::shared_ptr<ASTNode> convertToAST(void* tree);

    /**
     * 转换SELECT语句
     * @param ctx SELECT语句上下文
     * @return SELECT AST节点
     */
    std::shared_ptr<SelectStatement> convertSelectStatement(void* ctx);

    /**
     * 转换INSERT语句
     * @param ctx INSERT语句上下文
     * @return INSERT AST节点
     */
    std::shared_ptr<InsertStatement> convertInsertStatement(void* ctx);

    /**
     * 转换UPDATE语句
     * @param ctx UPDATE语句上下文
     * @return UPDATE AST节点
     */
    std::shared_ptr<UpdateStatement> convertUpdateStatement(void* ctx);

    /**
     * 转换DELETE语句
     * @param ctx DELETE语句上下文
     * @return DELETE AST节点
     */
    std::shared_ptr<DeleteStatement> convertDeleteStatement(void* ctx);

    /**
     * 转换CREATE TABLE语句
     * @param ctx CREATE TABLE语句上下文
     * @return CREATE TABLE AST节点
     */
    std::shared_ptr<CreateTableStatement> convertCreateTableStatement(void* ctx);

    /**
     * 转换表达式
     * @param ctx 表达式上下文
     * @return 表达式AST节点
     */
    std::shared_ptr<Expression> convertExpression(void* ctx);

    /**
     * 转换列引用
     * @param ctx 列引用上下文
     * @return 列引用AST节点
     */
    std::shared_ptr<ColumnReference> convertColumnReference(void* ctx);

    /**
     * 转换函数调用
     * @param ctx 函数调用上下文
     * @return 函数调用AST节点
     */
    std::shared_ptr<FunctionCall> convertFunctionCall(void* ctx);

    /**
     * 转换字面量
     * @param ctx 字面量上下文
     * @return 字面量AST节点
     */
    std::shared_ptr<Literal> convertLiteral(void* ctx);

    /**
     * 添加错误信息
     * @param error 错误信息
     */
    void addError(const std::string& error);

    /**
     * 自定义错误监听器
     */
#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
    class ErrorListener : public antlr4::BaseErrorListener {
    public:
        ErrorListener(Antlr4Parser* parser) : parser_(parser) {}

        void syntaxError(antlr4::Recognizer* recognizer, antlr4::Token* offendingSymbol,
                        size_t line, size_t charPositionInLine, const std::string& msg,
                        std::exception_ptr e) override;

    private:
        Antlr4Parser* parser_;
    };
#endif
#endif

private:
    std::vector<std::string> errors_;
#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
    std::unique_ptr<ErrorListener> errorListener_;
#endif
#endif
};

} // namespace sealdb

#endif // SEALDB_ANTLR4_PARSER_H
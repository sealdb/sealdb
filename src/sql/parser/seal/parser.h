#ifndef SEALDB_PARSER_H
#define SEALDB_PARSER_H

#include "lexer.h"
#include "ast.h"
#include <memory>
#include <vector>

namespace sealdb {

/**
 * @brief SQL解析器
 *
 * 负责将Token序列解析为AST
 */
class Parser {
public:
    explicit Parser(const std::string& sql);
    ~Parser() = default;

    /**
     * @brief 解析SQL语句
     */
    std::shared_ptr<Statement> parse();

    /**
     * @brief 解析SELECT语句
     */
    std::shared_ptr<SelectStatement> parse_select();

    /**
     * @brief 解析INSERT语句
     */
    std::shared_ptr<InsertStatement> parse_insert();

    /**
     * @brief 解析UPDATE语句
     */
    std::shared_ptr<UpdateStatement> parse_update();

    /**
     * @brief 解析DELETE语句
     */
    std::shared_ptr<DeleteStatement> parse_delete();

    /**
     * @brief 解析CREATE TABLE语句
     */
    std::shared_ptr<CreateTableStatement> parse_create_table();

    /**
     * @brief 解析DROP TABLE语句
     */
    std::shared_ptr<Statement> parse_drop_table();

    /**
     * @brief 解析表达式
     */
    std::shared_ptr<Expression> parse_expression();

    /**
     * @brief 解析条件表达式
     */
    std::shared_ptr<Expression> parse_condition();

    /**
     * @brief 解析算术表达式
     */
    std::shared_ptr<Expression> parse_arithmetic_expression();

    /**
     * @brief 解析项表达式
     */
    std::shared_ptr<Expression> parse_term();

    /**
     * @brief 解析因子表达式
     */
    std::shared_ptr<Expression> parse_factor();

    /**
     * @brief 解析主表达式
     */
    std::shared_ptr<Expression> parse_primary();

    /**
     * @brief 获取解析错误信息
     */
    const std::string& get_error() const { return error_; }

    /**
     * @brief 检查是否有错误
     */
    bool has_error() const { return !error_.empty(); }

private:
    Lexer lexer_;
    std::vector<Token> tokens_;
    size_t current_token_;
    std::string error_;

    // 辅助方法
    Token current_token() const;
    Token peek_token() const;
    void advance();
    bool advance_and_return_true();
    bool match(TokenType type);
    bool match_keyword(const std::string& keyword);
    void consume(TokenType type, const std::string& error_msg = "");
    void consume_keyword(const std::string& keyword, const std::string& error_msg = "");

    // 错误处理
    void report_error(const std::string& message);
    void synchronize();

    // 解析辅助方法
    std::vector<std::shared_ptr<Expression>> parse_select_list();
    std::vector<std::string> parse_from_clause();
    std::shared_ptr<Expression> parse_where_clause();
    std::vector<std::shared_ptr<Expression>> parse_group_by_clause();
    std::shared_ptr<Expression> parse_having_clause();
    std::vector<std::shared_ptr<Expression>> parse_order_by_clause();
    std::shared_ptr<Expression> parse_limit_clause();
    std::shared_ptr<Expression> parse_offset_clause();

    std::vector<std::string> parse_column_list();
    std::vector<std::vector<std::shared_ptr<Expression>>> parse_values_list();
    std::vector<std::pair<std::string, std::shared_ptr<Expression>>> parse_set_clause();

    std::vector<std::shared_ptr<Expression>> parse_column_definitions();
    std::shared_ptr<Expression> parse_column_definition();

    // 表达式解析辅助方法
    std::shared_ptr<Expression> parse_function_call();
    std::shared_ptr<Expression> parse_column_reference();
    std::shared_ptr<Expression> parse_literal();

    // 工具方法
    bool is_arithmetic_operator(TokenType type);
    bool is_comparison_operator(TokenType type);
    bool is_logical_operator(TokenType type);
    BinaryExpression::Operator token_to_operator(TokenType type);
};

} // namespace sealdb

#endif // SEALDB_PARSER_H
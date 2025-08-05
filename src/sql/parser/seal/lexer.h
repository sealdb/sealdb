#ifndef SEALDB_LEXER_H
#define SEALDB_LEXER_H

#include <string>
#include <vector>
#include <memory>
#include <unordered_map>

namespace sealdb {

/**
 * @brief Token类型枚举
 */
enum class TokenType {
    // 关键字
    SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, TABLE, INDEX, VIEW,
    FROM, WHERE, GROUP, BY, ORDER, HAVING, LIMIT, OFFSET,
    JOIN, LEFT, RIGHT, INNER, OUTER, ON, AS,
                    AND, OR, NOT, IN, INTO, VALUES, EXISTS, BETWEEN, LIKE, IS, NULL_VALUE,
    DISTINCT, COUNT, SUM, AVG, MAX, MIN,
    PRIMARY, KEY, FOREIGN, REFERENCES, UNIQUE, CHECK, DEFAULT,
    CONSTRAINT, CASCADE, RESTRICT, SET, NULL_ACTION,

    // 数据类型
    INT, INTEGER, BIGINT, SMALLINT, TINYINT,
    FLOAT, DOUBLE, DECIMAL, NUMERIC,
    CHAR, VARCHAR, TEXT, BLOB,
    DATE, TIME, DATETIME, TIMESTAMP,
    BOOLEAN, BOOL,

    // 操作符
    PLUS, MINUS, MULTIPLY, DIVIDE, MOD,
    EQUAL, NOT_EQUAL, LESS, LESS_EQUAL, GREATER, GREATER_EQUAL,
    ASSIGN, DOT, COMMA, SEMICOLON, LPAREN, RPAREN, LBRACKET, RBRACKET, LBRACE, RBRACE,

    // 字面量
    IDENTIFIER, STRING_LITERAL, NUMBER_LITERAL, NULL_LITERAL,

    // 其他
    WHITESPACE, COMMENT, END_OF_FILE, ERROR
};

/**
 * @brief Token结构体
 */
struct Token {
    TokenType type;
    std::string value;
    size_t line;
    size_t column;

    Token(TokenType t, const std::string& v, size_t l, size_t c)
        : type(t), value(v), line(l), column(c) {}

    std::string to_string() const;
};

/**
 * @brief SQL词法分析器
 *
 * 负责将SQL字符串分解为Token序列
 */
class Lexer {
public:
    explicit Lexer(const std::string& input);
    ~Lexer() = default;

    /**
     * @brief 获取下一个Token
     */
    Token next_token();

    /**
     * @brief 查看下一个Token但不消费
     */
    Token peek_token();

    /**
     * @brief 重置词法分析器
     */
    void reset();

    /**
     * @brief 获取当前位置
     */
    size_t get_position() const { return position_; }

    /**
     * @brief 获取当前行号
     */
    size_t get_line() const { return line_; }

    /**
     * @brief 获取当前列号
     */
    size_t get_column() const { return column_; }

    /**
     * @brief 检查是否到达文件末尾
     */
    bool is_eof() const { return position_ >= input_.length(); }

    /**
     * @brief 获取所有Token
     */
    std::vector<Token> tokenize();

private:
    std::string input_;
    size_t position_;
    size_t line_;
    size_t column_;

    // 关键字映射
    static std::unordered_map<std::string, TokenType> keywords_;

    // 初始化关键字映射
    static void init_keywords();

    // 辅助方法
    char current_char() const;
    char peek_char() const;
    void advance();
    void skip_whitespace();
    void skip_comment();

    // Token识别方法
    Token read_identifier();
    Token read_string();
    Token read_number();
    Token read_operator();
    Token read_keyword_or_identifier();

    // 工具方法
    bool is_alpha(char c) const;
    bool is_digit(char c) const;
    bool is_alphanumeric(char c) const;
    bool is_whitespace(char c) const;
    bool is_operator_start(char c) const;

    // 错误处理
    Token create_error_token(const std::string& message);
};

} // namespace sealdb

#endif // SEALDB_LEXER_H
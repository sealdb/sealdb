#include "sealdb/lexer.h"
#include "sealdb/logger.h"
#include <iostream>
#include <cctype>
#include <algorithm>

namespace sealdb {

// 静态成员初始化
std::unordered_map<std::string, TokenType> Lexer::keywords_;

void Lexer::init_keywords() {
    if (!keywords_.empty()) return; // 已经初始化过了

    // SQL关键字
    keywords_["SELECT"] = TokenType::SELECT;
    keywords_["INSERT"] = TokenType::INSERT;
    keywords_["UPDATE"] = TokenType::UPDATE;
    keywords_["DELETE"] = TokenType::DELETE;
    keywords_["CREATE"] = TokenType::CREATE;
    keywords_["DROP"] = TokenType::DROP;
    keywords_["ALTER"] = TokenType::ALTER;
    keywords_["TABLE"] = TokenType::TABLE;
    keywords_["INDEX"] = TokenType::INDEX;

    keywords_["FROM"] = TokenType::FROM;
    keywords_["WHERE"] = TokenType::WHERE;
    keywords_["GROUP"] = TokenType::GROUP;
    keywords_["BY"] = TokenType::BY;
    keywords_["ORDER"] = TokenType::ORDER;
    keywords_["HAVING"] = TokenType::HAVING;
    keywords_["LIMIT"] = TokenType::LIMIT;
    keywords_["OFFSET"] = TokenType::OFFSET;

    keywords_["JOIN"] = TokenType::JOIN;
    keywords_["LEFT"] = TokenType::LEFT;
    keywords_["RIGHT"] = TokenType::RIGHT;
    keywords_["INNER"] = TokenType::INNER;
    keywords_["OUTER"] = TokenType::OUTER;
    keywords_["ON"] = TokenType::ON;
    keywords_["AS"] = TokenType::AS;

    keywords_["AND"] = TokenType::AND;
    keywords_["OR"] = TokenType::OR;
    keywords_["NOT"] = TokenType::NOT;
    keywords_["IN"] = TokenType::IN;
    keywords_["EXISTS"] = TokenType::EXISTS;
    keywords_["BETWEEN"] = TokenType::BETWEEN;
    keywords_["LIKE"] = TokenType::LIKE;
    keywords_["IS"] = TokenType::IS;
    keywords_["NULL"] = TokenType::NULL_VALUE;

    keywords_["DISTINCT"] = TokenType::DISTINCT;
    keywords_["COUNT"] = TokenType::COUNT;
    keywords_["SUM"] = TokenType::SUM;
    keywords_["AVG"] = TokenType::AVG;
    keywords_["MAX"] = TokenType::MAX;
    keywords_["MIN"] = TokenType::MIN;

    keywords_["PRIMARY"] = TokenType::PRIMARY;
    keywords_["KEY"] = TokenType::KEY;
    keywords_["FOREIGN"] = TokenType::FOREIGN;
    keywords_["REFERENCES"] = TokenType::REFERENCES;
    keywords_["UNIQUE"] = TokenType::UNIQUE;
    keywords_["CHECK"] = TokenType::CHECK;
    keywords_["DEFAULT"] = TokenType::DEFAULT;
    keywords_["CONSTRAINT"] = TokenType::CONSTRAINT;
    keywords_["CASCADE"] = TokenType::CASCADE;
    keywords_["RESTRICT"] = TokenType::RESTRICT;
    keywords_["SET"] = TokenType::SET;

    // 数据类型
    keywords_["INT"] = TokenType::INT;
    keywords_["INTEGER"] = TokenType::INTEGER;
    keywords_["BIGINT"] = TokenType::BIGINT;
    keywords_["SMALLINT"] = TokenType::SMALLINT;
    keywords_["TINYINT"] = TokenType::TINYINT;
    keywords_["FLOAT"] = TokenType::FLOAT;
    keywords_["DOUBLE"] = TokenType::DOUBLE;
    keywords_["DECIMAL"] = TokenType::DECIMAL;
    keywords_["NUMERIC"] = TokenType::NUMERIC;
    keywords_["CHAR"] = TokenType::CHAR;
    keywords_["VARCHAR"] = TokenType::VARCHAR;
    keywords_["TEXT"] = TokenType::TEXT;
    keywords_["BLOB"] = TokenType::BLOB;
    keywords_["DATE"] = TokenType::DATE;
    keywords_["TIME"] = TokenType::TIME;
    keywords_["DATETIME"] = TokenType::DATETIME;
    keywords_["TIMESTAMP"] = TokenType::TIMESTAMP;
    keywords_["BOOLEAN"] = TokenType::BOOLEAN;
    keywords_["BOOL"] = TokenType::BOOL;
}

Lexer::Lexer(const std::string& input)
    : input_(input), position_(0), line_(1), column_(1) {
    init_keywords();
}

Token Lexer::next_token() {
    skip_whitespace();

    if (is_eof()) {
        return Token(TokenType::END_OF_FILE, "", line_, column_);
    }

    char current = current_char();

    // 处理标识符和关键字
    if (is_alpha(current) || current == '_') {
        return read_keyword_or_identifier();
    }

    // 处理数字
    if (is_digit(current)) {
        return read_number();
    }

    // 处理字符串
    if (current == '\'' || current == '"') {
        return read_string();
    }

    // 处理操作符
    if (is_operator_start(current)) {
        return read_operator();
    }

    // 处理其他字符
    advance();
    return create_error_token("Unexpected character: " + std::string(1, current));
}

Token Lexer::peek_token() {
    size_t saved_position = position_;
    size_t saved_line = line_;
    size_t saved_column = column_;

    Token token = next_token();

    position_ = saved_position;
    line_ = saved_line;
    column_ = saved_column;

    return token;
}

void Lexer::reset() {
    position_ = 0;
    line_ = 1;
    column_ = 1;
}

std::vector<Token> Lexer::tokenize() {
    std::vector<Token> tokens;
    reset();

    while (!is_eof()) {
        Token token = next_token();
        if (token.type != TokenType::WHITESPACE && token.type != TokenType::COMMENT) {
            tokens.push_back(token);
        }
        if (token.type == TokenType::END_OF_FILE) {
            break;
        }
    }

    return tokens;
}

char Lexer::current_char() const {
    if (position_ >= input_.length()) {
        return '\0';
    }
    return input_[position_];
}

char Lexer::peek_char() const {
    if (position_ + 1 >= input_.length()) {
        return '\0';
    }
    return input_[position_ + 1];
}

void Lexer::advance() {
    if (current_char() == '\n') {
        line_++;
        column_ = 1;
    } else {
        column_++;
    }
    position_++;
}

void Lexer::skip_whitespace() {
    while (!is_eof() && is_whitespace(current_char())) {
        advance();
    }
}

void Lexer::skip_comment() {
    if (current_char() == '-' && peek_char() == '-') {
        // 单行注释
        while (!is_eof() && current_char() != '\n') {
            advance();
        }
    } else if (current_char() == '/' && peek_char() == '*') {
        // 多行注释
        advance(); // 跳过 '/'
        advance(); // 跳过 '*'
        while (!is_eof()) {
            if (current_char() == '*' && peek_char() == '/') {
                advance(); // 跳过 '*'
                advance(); // 跳过 '/'
                break;
            }
            advance();
        }
    }
}

Token Lexer::read_identifier() {
    size_t start_line = line_;
    size_t start_column = column_;
    std::string identifier;

    while (!is_eof() && is_alphanumeric(current_char())) {
        identifier += current_char();
        advance();
    }

    return Token(TokenType::IDENTIFIER, identifier, start_line, start_column);
}

Token Lexer::read_string() {
    char quote = current_char();
    size_t start_line = line_;
    size_t start_column = column_;
    std::string value;

    advance(); // 跳过引号

    while (!is_eof() && current_char() != quote) {
        if (current_char() == '\\') {
            advance(); // 跳过转义字符
            if (!is_eof()) {
                value += current_char();
                advance();
            }
        } else {
            value += current_char();
            advance();
        }
    }

    if (is_eof()) {
        return create_error_token("Unterminated string literal");
    }

    advance(); // 跳过结束引号
    return Token(TokenType::STRING_LITERAL, value, start_line, start_column);
}

Token Lexer::read_number() {
    size_t start_line = line_;
    size_t start_column = column_;
    std::string number;
    bool has_decimal = false;

    // 读取整数部分
    while (!is_eof() && is_digit(current_char())) {
        number += current_char();
        advance();
    }

    // 检查是否有小数点
    if (!is_eof() && current_char() == '.') {
        has_decimal = true;
        number += current_char();
        advance();

        // 读取小数部分
        while (!is_eof() && is_digit(current_char())) {
            number += current_char();
            advance();
        }
    }

    // 检查是否有指数
    if (!is_eof() && (current_char() == 'e' || current_char() == 'E')) {
        number += current_char();
        advance();

        // 检查指数的符号
        if (!is_eof() && (current_char() == '+' || current_char() == '-')) {
            number += current_char();
            advance();
        }

        // 读取指数部分
        while (!is_eof() && is_digit(current_char())) {
            number += current_char();
            advance();
        }
    }

    return Token(TokenType::NUMBER_LITERAL, number, start_line, start_column);
}

Token Lexer::read_operator() {
    size_t start_line = line_;
    size_t start_column = column_;
    std::string op;

    char current = current_char();
    char next = peek_char();

    // 处理双字符操作符
    if (current == '=' && next == '=') {
        op = "==";
        advance();
        advance();
        return Token(TokenType::EQUAL, op, start_line, start_column);
    } else if (current == '!' && next == '=') {
        op = "!=";
        advance();
        advance();
        return Token(TokenType::NOT_EQUAL, op, start_line, start_column);
    } else if (current == '<' && next == '=') {
        op = "<=";
        advance();
        advance();
        return Token(TokenType::LESS_EQUAL, op, start_line, start_column);
    } else if (current == '>' && next == '=') {
        op = ">=";
        advance();
        advance();
        return Token(TokenType::GREATER_EQUAL, op, start_line, start_column);
    }

    // 处理单字符操作符
    switch (current) {
        case '+': op = "+"; advance(); return Token(TokenType::PLUS, op, start_line, start_column);
        case '-': op = "-"; advance(); return Token(TokenType::MINUS, op, start_line, start_column);
        case '*': op = "*"; advance(); return Token(TokenType::MULTIPLY, op, start_line, start_column);
        case '/': op = "/"; advance(); return Token(TokenType::DIVIDE, op, start_line, start_column);
        case '%': op = "%"; advance(); return Token(TokenType::MOD, op, start_line, start_column);
        case '=': op = "="; advance(); return Token(TokenType::EQUAL, op, start_line, start_column);
        case '<': op = "<"; advance(); return Token(TokenType::LESS, op, start_line, start_column);
        case '>': op = ">"; advance(); return Token(TokenType::GREATER, op, start_line, start_column);
        case '.': op = "."; advance(); return Token(TokenType::DOT, op, start_line, start_column);
        case ',': op = ","; advance(); return Token(TokenType::COMMA, op, start_line, start_column);
        case ';': op = ";"; advance(); return Token(TokenType::SEMICOLON, op, start_line, start_column);
        case '(': op = "("; advance(); return Token(TokenType::LPAREN, op, start_line, start_column);
        case ')': op = ")"; advance(); return Token(TokenType::RPAREN, op, start_line, start_column);
        default:
            advance();
            return create_error_token("Unknown operator: " + std::string(1, current));
    }
}

Token Lexer::read_keyword_or_identifier() {
    size_t start_line = line_;
    size_t start_column = column_;
    std::string word;

    while (!is_eof() && is_alphanumeric(current_char())) {
        word += current_char();
        advance();
    }

    // 转换为大写进行关键字匹配
    std::string upper_word = word;
    std::transform(upper_word.begin(), upper_word.end(), upper_word.begin(), ::toupper);

    auto it = keywords_.find(upper_word);
    if (it != keywords_.end()) {
        return Token(it->second, word, start_line, start_column);
    }

    return Token(TokenType::IDENTIFIER, word, start_line, start_column);
}

bool Lexer::is_alpha(char c) const {
    return std::isalpha(c) || c == '_';
}

bool Lexer::is_digit(char c) const {
    return std::isdigit(c);
}

bool Lexer::is_alphanumeric(char c) const {
    return is_alpha(c) || is_digit(c);
}

bool Lexer::is_whitespace(char c) const {
    return std::isspace(c);
}

bool Lexer::is_operator_start(char c) const {
    return c == '+' || c == '-' || c == '*' || c == '/' || c == '%' ||
           c == '=' || c == '<' || c == '>' || c == '!' ||
           c == '.' || c == ',' || c == ';' || c == '(' || c == ')';
}

Token Lexer::create_error_token(const std::string& message) {
    return Token(TokenType::ERROR, message, line_, column_);
}

std::string Token::to_string() const {
    std::string type_str;
    switch (type) {
        case TokenType::SELECT: type_str = "SELECT"; break;
        case TokenType::INSERT: type_str = "INSERT"; break;
        case TokenType::UPDATE: type_str = "UPDATE"; break;
        case TokenType::DELETE: type_str = "DELETE"; break;
        case TokenType::CREATE: type_str = "CREATE"; break;
        case TokenType::DROP: type_str = "DROP"; break;
        case TokenType::ALTER: type_str = "ALTER"; break;
        case TokenType::TABLE: type_str = "TABLE"; break;
        case TokenType::FROM: type_str = "FROM"; break;
        case TokenType::WHERE: type_str = "WHERE"; break;
        case TokenType::AND: type_str = "AND"; break;
        case TokenType::OR: type_str = "OR"; break;
        case TokenType::IDENTIFIER: type_str = "IDENTIFIER"; break;
        case TokenType::STRING_LITERAL: type_str = "STRING_LITERAL"; break;
        case TokenType::NUMBER_LITERAL: type_str = "NUMBER_LITERAL"; break;
        case TokenType::PLUS: type_str = "PLUS"; break;
        case TokenType::MINUS: type_str = "MINUS"; break;
        case TokenType::MULTIPLY: type_str = "MULTIPLY"; break;
        case TokenType::DIVIDE: type_str = "DIVIDE"; break;
        case TokenType::EQUAL: type_str = "EQUAL"; break;
        case TokenType::NOT_EQUAL: type_str = "NOT_EQUAL"; break;
        case TokenType::LESS: type_str = "LESS"; break;
        case TokenType::GREATER: type_str = "GREATER"; break;
        case TokenType::LPAREN: type_str = "LPAREN"; break;
        case TokenType::RPAREN: type_str = "RPAREN"; break;
        case TokenType::COMMA: type_str = "COMMA"; break;
        case TokenType::SEMICOLON: type_str = "SEMICOLON"; break;
        case TokenType::NULL_VALUE: type_str = "NULL_VALUE"; break;
        case TokenType::END_OF_FILE: type_str = "END_OF_FILE"; break;
        case TokenType::ERROR: type_str = "ERROR"; break;
        default: type_str = "UNKNOWN"; break;
    }

    return "Token(" + type_str + ", '" + value + "', line=" + std::to_string(line) +
           ", col=" + std::to_string(column) + ")";
}

} // namespace sealdb

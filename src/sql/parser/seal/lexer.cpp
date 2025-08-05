#include "lexer.h"
#include <cctype>
#include <sstream>

namespace sealdb {

// 静态成员初始化
std::unordered_map<std::string, TokenType> Lexer::keywords_;

Lexer::Lexer(const std::string& input)
    : input_(input), position_(0), line_(1), column_(1) {
    init_keywords();
}

void Lexer::init_keywords() {
    if (!keywords_.empty()) return; // 已经初始化过了

    keywords_["SELECT"] = TokenType::SELECT;
    keywords_["INSERT"] = TokenType::INSERT;
    keywords_["UPDATE"] = TokenType::UPDATE;
    keywords_["DELETE"] = TokenType::DELETE;
    keywords_["CREATE"] = TokenType::CREATE;
    keywords_["DROP"] = TokenType::DROP;
    keywords_["ALTER"] = TokenType::ALTER;
    keywords_["TABLE"] = TokenType::TABLE;
    keywords_["INDEX"] = TokenType::INDEX;
    keywords_["VIEW"] = TokenType::VIEW;
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
    keywords_["INTO"] = TokenType::INTO;
    keywords_["VALUES"] = TokenType::VALUES;
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

Token Lexer::next_token() {
    skip_whitespace();

    if (is_eof()) {
        return Token(TokenType::END_OF_FILE, "", line_, column_);
    }

    char c = current_char();

    if (is_alpha(c)) {
        return read_keyword_or_identifier();
    } else if (is_digit(c)) {
        return read_number();
    } else if (c == '\'' || c == '"') {
        return read_string();
    } else if (is_operator_start(c)) {
        return read_operator();
    } else {
        return create_error_token("Unexpected character: " + std::string(1, c));
    }
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
    return position_ < input_.length() ? input_[position_] : '\0';
}

char Lexer::peek_char() const {
    return (position_ + 1) < input_.length() ? input_[position_ + 1] : '\0';
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
    size_t start = position_;
    while (!is_eof() && is_alphanumeric(current_char())) {
        advance();
    }

    std::string value = input_.substr(start, position_ - start);
    return Token(TokenType::IDENTIFIER, value, line_, column_ - value.length());
}

Token Lexer::read_string() {
    char quote = current_char();
    advance(); // 跳过引号

    size_t start = position_;
    while (!is_eof() && current_char() != quote) {
        if (current_char() == '\\') {
            advance(); // 跳过转义字符
        }
        advance();
    }

    if (is_eof()) {
        return create_error_token("Unterminated string literal");
    }

    std::string value = input_.substr(start, position_ - start);
    advance(); // 跳过结束引号

    return Token(TokenType::STRING_LITERAL, value, line_, column_ - value.length() - 2);
}

Token Lexer::read_number() {
    size_t start = position_;
    bool has_decimal = false;

    while (!is_eof() && is_digit(current_char())) {
        advance();
    }

    if (!is_eof() && current_char() == '.') {
        has_decimal = true;
        advance();
        while (!is_eof() && is_digit(current_char())) {
            advance();
        }
    }

    std::string value = input_.substr(start, position_ - start);
    return Token(TokenType::NUMBER_LITERAL, value, line_, column_ - value.length());
}

Token Lexer::read_operator() {
    char c = current_char();
    char next = peek_char();

    if (c == '+' && next == '=') {
        advance(); advance();
        return Token(TokenType::PLUS, "+=", line_, column_ - 2);
    } else if (c == '-' && next == '=') {
        advance(); advance();
        return Token(TokenType::MINUS, "-=", line_, column_ - 2);
    } else if (c == '*' && next == '=') {
        advance(); advance();
        return Token(TokenType::MULTIPLY, "*=", line_, column_ - 2);
    } else if (c == '/' && next == '=') {
        advance(); advance();
        return Token(TokenType::DIVIDE, "/=", line_, column_ - 2);
    } else if (c == '=' && next == '=') {
        advance(); advance();
        return Token(TokenType::EQUAL, "==", line_, column_ - 2);
    } else if (c == '!' && next == '=') {
        advance(); advance();
        return Token(TokenType::NOT_EQUAL, "!=", line_, column_ - 2);
    } else if (c == '<' && next == '=') {
        advance(); advance();
        return Token(TokenType::LESS_EQUAL, "<=", line_, column_ - 2);
    } else if (c == '>' && next == '=') {
        advance(); advance();
        return Token(TokenType::GREATER_EQUAL, ">=", line_, column_ - 2);
    } else {
        advance();
        switch (c) {
            case '+': return Token(TokenType::PLUS, "+", line_, column_ - 1);
            case '-': return Token(TokenType::MINUS, "-", line_, column_ - 1);
            case '*': return Token(TokenType::MULTIPLY, "*", line_, column_ - 1);
            case '/': return Token(TokenType::DIVIDE, "/", line_, column_ - 1);
            case '%': return Token(TokenType::MOD, "%", line_, column_ - 1);
            case '=': return Token(TokenType::ASSIGN, "=", line_, column_ - 1);
            case '<': return Token(TokenType::LESS, "<", line_, column_ - 1);
            case '>': return Token(TokenType::GREATER, ">", line_, column_ - 1);
            case '.': return Token(TokenType::DOT, ".", line_, column_ - 1);
            case ',': return Token(TokenType::COMMA, ",", line_, column_ - 1);
            case ';': return Token(TokenType::SEMICOLON, ";", line_, column_ - 1);
            case '(': return Token(TokenType::LPAREN, "(", line_, column_ - 1);
            case ')': return Token(TokenType::RPAREN, ")", line_, column_ - 1);
            case '[': return Token(TokenType::LBRACKET, "[", line_, column_ - 1);
            case ']': return Token(TokenType::RBRACKET, "]", line_, column_ - 1);
            case '{': return Token(TokenType::LBRACE, "{", line_, column_ - 1);
            case '}': return Token(TokenType::RBRACE, "}", line_, column_ - 1);
            default: return create_error_token("Unknown operator: " + std::string(1, c));
        }
    }
}

Token Lexer::read_keyword_or_identifier() {
    size_t start = position_;
    while (!is_eof() && is_alphanumeric(current_char())) {
        advance();
    }

    std::string value = input_.substr(start, position_ - start);

    // 转换为大写进行关键字匹配
    std::string upper_value = value;
    std::transform(upper_value.begin(), upper_value.end(), upper_value.begin(), ::toupper);

    auto it = keywords_.find(upper_value);
    if (it != keywords_.end()) {
        return Token(it->second, value, line_, column_ - value.length());
    } else {
        return Token(TokenType::IDENTIFIER, value, line_, column_ - value.length());
    }
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
           c == '.' || c == ',' || c == ';' ||
           c == '(' || c == ')' || c == '[' || c == ']' || c == '{' || c == '}';
}

Token Lexer::create_error_token(const std::string& message) {
    return Token(TokenType::ERROR, message, line_, column_);
}

std::string Token::to_string() const {
    std::ostringstream oss;
    oss << "Token(" << static_cast<int>(type) << ", \"" << value << "\", " << line << ", " << column << ")";
    return oss.str();
}

} // namespace sealdb
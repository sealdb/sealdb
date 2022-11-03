#include "sealdb/parser.h"
#include "sealdb/logger.h"
#include <iostream>
#include <algorithm>

namespace sealdb {

Parser::Parser(const std::string& sql)
    : lexer_(sql), current_token_(0) {
    // 词法分析，获取所有Token
    tokens_ = lexer_.tokenize();
}

std::unique_ptr<Statement> Parser::parse() {
    if (tokens_.empty()) {
        report_error("Empty SQL statement");
        return nullptr;
    }

    Token first_token = tokens_[0];

    switch (first_token.type) {
        case TokenType::SELECT:
            return parse_select();
        case TokenType::INSERT:
            return parse_insert();
        case TokenType::UPDATE:
            return parse_update();
        case TokenType::DELETE:
            return parse_delete();
        case TokenType::CREATE:
            return parse_create_table();
        case TokenType::DROP:
            return parse_drop_table();
        default:
            report_error("Unknown statement type: " + first_token.value);
            return nullptr;
    }
}

std::unique_ptr<SelectStatement> Parser::parse_select() {
    consume_keyword("SELECT", "Expected SELECT");

    auto select_list = parse_select_list();

    std::vector<std::string> from_tables;
    if (match_keyword("FROM")) {
        from_tables = parse_from_clause();
    }

    std::unique_ptr<Expression> where_clause;
    if (match_keyword("WHERE")) {
        where_clause = parse_where_clause();
    }

    std::vector<std::unique_ptr<Expression>> group_by;
    if (match_keyword("GROUP")) {
        consume_keyword("BY", "Expected BY after GROUP");
        group_by = parse_group_by_clause();
    }

    std::unique_ptr<Expression> having_clause;
    if (match_keyword("HAVING")) {
        having_clause = parse_having_clause();
    }

    std::vector<std::unique_ptr<Expression>> order_by;
    if (match_keyword("ORDER")) {
        consume_keyword("BY", "Expected BY after ORDER");
        order_by = parse_order_by_clause();
    }

    std::unique_ptr<Expression> limit;
    if (match_keyword("LIMIT")) {
        limit = parse_limit_clause();
    }

    std::unique_ptr<Expression> offset;
    if (match_keyword("OFFSET")) {
        offset = parse_offset_clause();
    }

    return std::make_unique<SelectStatement>(
        std::move(select_list),
        std::move(from_tables),
        std::move(where_clause),
        std::move(group_by),
        std::move(having_clause),
        std::move(order_by),
        std::move(limit),
        std::move(offset)
    );
}

std::unique_ptr<InsertStatement> Parser::parse_insert() {
    consume_keyword("INSERT", "Expected INSERT");
    consume_keyword("INTO", "Expected INTO after INSERT");

    std::string table_name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected table name");

    std::vector<std::string> columns;
    if (match(TokenType::LPAREN)) {
        columns = parse_column_list();
        consume(TokenType::RPAREN, "Expected ) after column list");
    }

    consume_keyword("VALUES", "Expected VALUES");

    auto values = parse_values_list();

    return std::make_unique<InsertStatement>(
        table_name,
        std::move(columns),
        std::move(values)
    );
}

std::unique_ptr<UpdateStatement> Parser::parse_update() {
    consume_keyword("UPDATE", "Expected UPDATE");

    std::string table_name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected table name");

    consume_keyword("SET", "Expected SET");

    auto set_clause = parse_set_clause();

    std::unique_ptr<Expression> where_clause;
    if (match_keyword("WHERE")) {
        where_clause = parse_where_clause();
    }

    return std::make_unique<UpdateStatement>(
        table_name,
        std::move(set_clause),
        std::move(where_clause)
    );
}

std::unique_ptr<DeleteStatement> Parser::parse_delete() {
    consume_keyword("DELETE", "Expected DELETE");
    consume_keyword("FROM", "Expected FROM after DELETE");

    std::string table_name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected table name");

    std::unique_ptr<Expression> where_clause;
    if (match_keyword("WHERE")) {
        where_clause = parse_where_clause();
    }

    return std::make_unique<DeleteStatement>(
        table_name,
        std::move(where_clause)
    );
}

std::unique_ptr<CreateTableStatement> Parser::parse_create_table() {
    consume_keyword("CREATE", "Expected CREATE");
    consume_keyword("TABLE", "Expected TABLE");

    std::string table_name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected table name");

    consume(TokenType::LPAREN, "Expected ( after table name");

    auto columns = parse_column_definitions();

    consume(TokenType::RPAREN, "Expected ) after column definitions");

    return std::make_unique<CreateTableStatement>(
        table_name,
        std::move(columns)
    );
}

std::unique_ptr<DropTableStatement> Parser::parse_drop_table() {
    consume_keyword("DROP", "Expected DROP");
    consume_keyword("TABLE", "Expected TABLE");

    std::string table_name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected table name");

    return std::make_unique<DropTableStatement>(table_name);
}

std::unique_ptr<Expression> Parser::parse_expression() {
    return parse_condition();
}

std::unique_ptr<Expression> Parser::parse_condition() {
    auto left = parse_arithmetic_expression();

    while (is_comparison_operator(current_token().type) ||
           is_logical_operator(current_token().type)) {
        auto op = token_to_operator(current_token().type);
        advance();
        auto right = parse_arithmetic_expression();

        left = std::make_unique<BinaryExpression>(op, std::move(left), std::move(right));
    }

    return left;
}

std::unique_ptr<Expression> Parser::parse_arithmetic_expression() {
    auto left = parse_term();

    while (is_arithmetic_operator(current_token().type)) {
        auto op = token_to_operator(current_token().type);
        advance();
        auto right = parse_term();

        left = std::make_unique<BinaryExpression>(op, std::move(left), std::move(right));
    }

    return left;
}

std::unique_ptr<Expression> Parser::parse_term() {
    auto left = parse_factor();

    while (current_token().type == TokenType::MULTIPLY ||
           current_token().type == TokenType::DIVIDE ||
           current_token().type == TokenType::MOD) {
        auto op = token_to_operator(current_token().type);
        advance();
        auto right = parse_factor();

        left = std::make_unique<BinaryExpression>(op, std::move(left), std::move(right));
    }

    return left;
}

std::unique_ptr<Expression> Parser::parse_factor() {
    if (current_token().type == TokenType::MINUS) {
        advance();
        auto expr = parse_primary();
        // 这里可以创建一个一元表达式，暂时用0减去表达式的形式
        auto zero = std::make_unique<LiteralExpression>(LiteralExpression::Type::INTEGER, "0");
        return std::make_unique<BinaryExpression>(
            BinaryExpression::Operator::SUBTRACT,
            std::move(zero),
            std::move(expr)
        );
    }

    return parse_primary();
}

std::unique_ptr<Expression> Parser::parse_primary() {
    Token token = current_token();

    switch (token.type) {
        case TokenType::IDENTIFIER:
            advance();
            if (match(TokenType::LPAREN)) {
                // 函数调用
                advance(); // 跳过左括号
                std::vector<std::unique_ptr<Expression>> args;

                if (!match(TokenType::RPAREN)) {
                    do {
                        args.push_back(parse_expression());
                    } while (match(TokenType::COMMA) && (advance(), true));

                    consume(TokenType::RPAREN, "Expected ) after function arguments");
                } else {
                    advance(); // 跳过右括号
                }

                return std::make_unique<FunctionCallExpression>(token.value, std::move(args));
            } else {
                // 简单标识符
                return std::make_unique<IdentifierExpression>(token.value);
            }

        case TokenType::NUMBER_LITERAL:
            advance();
            return std::make_unique<LiteralExpression>(LiteralExpression::Type::INTEGER, token.value);

        case TokenType::STRING_LITERAL:
            advance();
            return std::make_unique<LiteralExpression>(LiteralExpression::Type::STRING, token.value);

        case TokenType::LPAREN: {
            advance();
            auto expr = parse_expression();
            consume(TokenType::RPAREN, "Expected ) after expression");
            return expr;
        }
        default:
            report_error("Unexpected token type in parse_primary: " + token.value);
            return nullptr;
    }
}

// 辅助方法实现
Token Parser::current_token() const {
    if (current_token_ >= tokens_.size()) {
        return Token(TokenType::END_OF_FILE, "", 0, 0);
    }
    return tokens_[current_token_];
}

Token Parser::peek_token() const {
    if (current_token_ + 1 >= tokens_.size()) {
        return Token(TokenType::END_OF_FILE, "", 0, 0);
    }
    return tokens_[current_token_ + 1];
}

void Parser::advance() {
    if (current_token_ < tokens_.size()) {
        current_token_++;
    }
}

bool Parser::advance_and_return_true() {
    advance();
    return true;
}

bool Parser::match(TokenType type) {
    return current_token().type == type;
}

bool Parser::match_keyword(const std::string& keyword) {
    Token token = current_token();
    std::string upper_token = token.value;
    std::transform(upper_token.begin(), upper_token.end(), upper_token.begin(), ::toupper);
    return upper_token == keyword;
}

void Parser::consume(TokenType type, const std::string& error_msg) {
    if (match(type)) {
        advance();
    } else {
        report_error(error_msg.empty() ?
            "Expected token type " + std::to_string(static_cast<int>(type)) :
            error_msg);
    }
}

void Parser::consume_keyword(const std::string& keyword, const std::string& error_msg) {
    if (match_keyword(keyword)) {
        advance();
    } else {
        report_error(error_msg.empty() ?
            "Expected keyword " + keyword :
            error_msg);
    }
}

void Parser::report_error(const std::string& message) {
    if (error_.empty()) {
        error_ = "Parse error at line " + std::to_string(current_token().line) +
                ", column " + std::to_string(current_token().column) + ": " + message;
    }
}

void Parser::synchronize() {
    advance();

    while (current_token().type != TokenType::END_OF_FILE && !match(TokenType::SEMICOLON)) {
        switch (current_token().type) {
            case TokenType::SELECT:
            case TokenType::INSERT:
            case TokenType::UPDATE:
            case TokenType::DELETE:
            case TokenType::CREATE:
            case TokenType::DROP:
                return;
            default:
                advance();
                break;
        }
    }
}

// 解析辅助方法实现
std::vector<std::unique_ptr<Expression>> Parser::parse_select_list() {
    std::vector<std::unique_ptr<Expression>> select_list;

    do {
        select_list.push_back(parse_expression());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return select_list;
}

std::vector<std::string> Parser::parse_from_clause() {
    std::vector<std::string> tables;

    do {
        tables.push_back(current_token().value);
        consume(TokenType::IDENTIFIER, "Expected table name");
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return tables;
}

std::unique_ptr<Expression> Parser::parse_where_clause() {
    return parse_expression();
}

std::vector<std::unique_ptr<Expression>> Parser::parse_group_by_clause() {
    std::vector<std::unique_ptr<Expression>> group_by;

    do {
        group_by.push_back(parse_expression());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return group_by;
}

std::unique_ptr<Expression> Parser::parse_having_clause() {
    return parse_expression();
}

std::vector<std::unique_ptr<Expression>> Parser::parse_order_by_clause() {
    std::vector<std::unique_ptr<Expression>> order_by;

    do {
        order_by.push_back(parse_expression());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return order_by;
}

std::unique_ptr<Expression> Parser::parse_limit_clause() {
    return parse_expression();
}

std::unique_ptr<Expression> Parser::parse_offset_clause() {
    return parse_expression();
}

std::vector<std::string> Parser::parse_column_list() {
    std::vector<std::string> columns;

    do {
        columns.push_back(current_token().value);
        consume(TokenType::IDENTIFIER, "Expected column name");
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::vector<std::vector<std::unique_ptr<Expression>>> Parser::parse_values_list() {
    std::vector<std::vector<std::unique_ptr<Expression>>> values;

    do {
        consume(TokenType::LPAREN, "Expected ( before values");

        std::vector<std::unique_ptr<Expression>> row_values;
        do {
            row_values.push_back(parse_expression());
        } while (match(TokenType::COMMA) && advance_and_return_true());

        consume(TokenType::RPAREN, "Expected ) after values");
        values.push_back(std::move(row_values));

    } while (match(TokenType::COMMA) && advance_and_return_true());

    return values;
}

std::vector<std::pair<std::string, std::unique_ptr<Expression>>> Parser::parse_set_clause() {
    std::vector<std::pair<std::string, std::unique_ptr<Expression>>> set_clause;

    do {
        std::string column_name = current_token().value;
        consume(TokenType::IDENTIFIER, "Expected column name");

        consume(TokenType::ASSIGN, "Expected = after column name");

        auto value = parse_expression();
        set_clause.emplace_back(column_name, std::move(value));

    } while (match(TokenType::COMMA) && advance_and_return_true());

    return set_clause;
}

std::vector<CreateTableStatement::ColumnDefinition> Parser::parse_column_definitions() {
    std::vector<CreateTableStatement::ColumnDefinition> columns;

    do {
        columns.push_back(parse_column_definition());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

CreateTableStatement::ColumnDefinition Parser::parse_column_definition() {
    std::string name = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected column name");

    std::string data_type = current_token().value;
    consume(TokenType::IDENTIFIER, "Expected data type");

    CreateTableStatement::ColumnDefinition column(name, data_type);

    // 解析可选的约束
    while (current_token().type != TokenType::COMMA &&
           current_token().type != TokenType::RPAREN &&
           current_token().type != TokenType::END_OF_FILE) {

        if (match_keyword("NOT")) {
            consume_keyword("NULL", "Expected NULL after NOT");
            column.is_nullable = false;
        } else if (match_keyword("PRIMARY")) {
            consume_keyword("KEY", "Expected KEY after PRIMARY");
            column.is_primary_key = true;
        } else if (match_keyword("UNIQUE")) {
            column.is_unique = true;
        } else if (match_keyword("DEFAULT")) {
            column.default_value = parse_expression();
        } else {
            // 跳过未知的约束，但确保不会无限循环
            advance();
            break; // 遇到未知约束就退出循环
        }
    }

    return column;
}

// 工具方法实现
bool Parser::is_arithmetic_operator(TokenType type) {
    return type == TokenType::PLUS || type == TokenType::MINUS ||
           type == TokenType::MULTIPLY || type == TokenType::DIVIDE ||
           type == TokenType::MOD;
}

bool Parser::is_comparison_operator(TokenType type) {
    return type == TokenType::EQUAL || type == TokenType::NOT_EQUAL ||
           type == TokenType::LESS || type == TokenType::LESS_EQUAL ||
           type == TokenType::GREATER || type == TokenType::GREATER_EQUAL;
}

bool Parser::is_logical_operator(TokenType type) {
    return type == TokenType::AND || type == TokenType::OR;
}

BinaryExpression::Operator Parser::token_to_operator(TokenType type) {
    switch (type) {
        case TokenType::PLUS: return BinaryExpression::Operator::ADD;
        case TokenType::MINUS: return BinaryExpression::Operator::SUBTRACT;
        case TokenType::MULTIPLY: return BinaryExpression::Operator::MULTIPLY;
        case TokenType::DIVIDE: return BinaryExpression::Operator::DIVIDE;
        case TokenType::MOD: return BinaryExpression::Operator::MOD;
        case TokenType::EQUAL: return BinaryExpression::Operator::EQUAL;
        case TokenType::NOT_EQUAL: return BinaryExpression::Operator::NOT_EQUAL;
        case TokenType::LESS: return BinaryExpression::Operator::LESS;
        case TokenType::LESS_EQUAL: return BinaryExpression::Operator::LESS_EQUAL;
        case TokenType::GREATER: return BinaryExpression::Operator::GREATER;
        case TokenType::GREATER_EQUAL: return BinaryExpression::Operator::GREATER_EQUAL;
        case TokenType::AND: return BinaryExpression::Operator::AND;
        case TokenType::OR: return BinaryExpression::Operator::OR;
        default:
            return BinaryExpression::Operator::ADD; // 默认值
    }
}

} // namespace sealdb

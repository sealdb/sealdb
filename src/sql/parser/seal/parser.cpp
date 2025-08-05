#include "parser.h"
#include <algorithm>
#include <sstream>

namespace sealdb {

Parser::Parser(const std::string& sql)
    : lexer_(sql), current_token_(0) {
    tokens_ = lexer_.tokenize();
}

std::shared_ptr<Statement> Parser::parse() {
    if (tokens_.empty()) {
        report_error("Empty input");
        return nullptr;
    }

    Token token = current_token();

    switch (token.type) {
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
            report_error("Unexpected token: " + token.value);
            return nullptr;
    }
}

std::shared_ptr<SelectStatement> Parser::parse_select() {
    auto stmt = std::make_shared<SelectStatement>();

    // 解析 SELECT
    consume(TokenType::SELECT, "Expected SELECT");

    // 解析选择列表
    stmt->selectList = parse_select_list();

    // 解析 FROM 子句
    if (match(TokenType::FROM)) {
        advance();
        if (match(TokenType::IDENTIFIER)) {
            stmt->fromTable = current_token().value;
            advance();
        } else {
            report_error("Expected table name");
            return nullptr;
        }
    }

    // 解析 WHERE 子句
    if (match(TokenType::WHERE)) {
        advance();
        stmt->whereClause = parse_where_clause();
    }

    // 解析 GROUP BY 子句
    if (match(TokenType::GROUP)) {
        advance();
        consume(TokenType::BY, "Expected BY after GROUP");
        stmt->groupBy = parse_group_by_clause();
    }

    // 解析 HAVING 子句
    if (match(TokenType::HAVING)) {
        advance();
        stmt->havingClause = parse_having_clause();
    }

    // 解析 ORDER BY 子句
    if (match(TokenType::ORDER)) {
        advance();
        consume(TokenType::BY, "Expected BY after ORDER");
        stmt->orderBy = parse_order_by_clause();
    }

    // 解析 LIMIT 子句
    if (match(TokenType::LIMIT)) {
        advance();
        stmt->limitClause = parse_limit_clause();
    }

    // 解析 OFFSET 子句
    if (match(TokenType::OFFSET)) {
        advance();
        stmt->offsetClause = parse_offset_clause();
    }

    return stmt;
}

std::shared_ptr<InsertStatement> Parser::parse_insert() {
    auto stmt = std::make_shared<InsertStatement>();

    // 解析 INSERT
    consume(TokenType::INSERT, "Expected INSERT");

    // 解析 INTO
    consume(TokenType::INTO, "Expected INTO");

    // 解析表名
    if (match(TokenType::IDENTIFIER)) {
        stmt->tableName = current_token().value;
        advance();
    } else {
        report_error("Expected table name");
        return nullptr;
    }

    // 解析列名列表
    if (match(TokenType::LPAREN)) {
        advance();
        stmt->columns = parse_column_list();
        consume(TokenType::RPAREN, "Expected )");
    }

    // 解析 VALUES
    consume(TokenType::VALUES, "Expected VALUES");

    // 解析值列表
    stmt->values = parse_values_list();

    return stmt;
}

std::shared_ptr<UpdateStatement> Parser::parse_update() {
    auto stmt = std::make_shared<UpdateStatement>();

    // 解析 UPDATE
    consume(TokenType::UPDATE, "Expected UPDATE");

    // 解析表名
    if (match(TokenType::IDENTIFIER)) {
        stmt->tableName = current_token().value;
        advance();
    } else {
        report_error("Expected table name");
        return nullptr;
    }

    // 解析 SET
    consume(TokenType::SET, "Expected SET");

    // 解析 SET 子句
    stmt->setClause = parse_set_clause();

    // 解析 WHERE 子句
    if (match(TokenType::WHERE)) {
        advance();
        stmt->whereClause = parse_where_clause();
    }

    return stmt;
}

std::shared_ptr<DeleteStatement> Parser::parse_delete() {
    auto stmt = std::make_shared<DeleteStatement>();

    // 解析 DELETE
    consume(TokenType::DELETE, "Expected DELETE");

    // 解析 FROM
    consume(TokenType::FROM, "Expected FROM");

    // 解析表名
    if (match(TokenType::IDENTIFIER)) {
        stmt->tableName = current_token().value;
        advance();
    } else {
        report_error("Expected table name");
        return nullptr;
    }

    // 解析 WHERE 子句
    if (match(TokenType::WHERE)) {
        advance();
        stmt->whereClause = parse_where_clause();
    }

    return stmt;
}

std::shared_ptr<CreateTableStatement> Parser::parse_create_table() {
    auto stmt = std::make_shared<CreateTableStatement>();

    // 解析 CREATE
    consume(TokenType::CREATE, "Expected CREATE");

    // 解析 TABLE
    consume(TokenType::TABLE, "Expected TABLE");

    // 解析表名
    if (match(TokenType::IDENTIFIER)) {
        stmt->tableName = current_token().value;
        advance();
    } else {
        report_error("Expected table name");
        return nullptr;
    }

    // 解析列定义
    consume(TokenType::LPAREN, "Expected (");
    stmt->columns = parse_column_definitions();
    consume(TokenType::RPAREN, "Expected )");

    return stmt;
}

std::shared_ptr<Statement> Parser::parse_drop_table() {
    // 简化实现，返回nullptr
    consume(TokenType::DROP, "Expected DROP");
    consume(TokenType::TABLE, "Expected TABLE");

    if (match(TokenType::IDENTIFIER)) {
        advance();
    } else {
        report_error("Expected table name");
        return nullptr;
    }

    return nullptr; // 暂时返回nullptr
}

std::shared_ptr<Expression> Parser::parse_expression() {
    return parse_condition();
}

std::shared_ptr<Expression> Parser::parse_condition() {
    auto left = parse_arithmetic_expression();

    while (match(TokenType::AND) || match(TokenType::OR)) {
        TokenType op_type = current_token().type;
        advance();

        auto right = parse_arithmetic_expression();

        auto binary_expr = std::make_shared<BinaryExpression>(left, right, token_to_operator(op_type));

        left = binary_expr;
    }

    return left;
}

std::shared_ptr<Expression> Parser::parse_arithmetic_expression() {
    auto left = parse_term();

    while (match(TokenType::PLUS) || match(TokenType::MINUS)) {
        TokenType op_type = current_token().type;
        advance();

        auto right = parse_term();

        auto binary_expr = std::make_shared<BinaryExpression>(left, right, token_to_operator(op_type));

        left = binary_expr;
    }

    return left;
}

std::shared_ptr<Expression> Parser::parse_term() {
    auto left = parse_factor();

    while (match(TokenType::MULTIPLY) || match(TokenType::DIVIDE) || match(TokenType::MOD)) {
        TokenType op_type = current_token().type;
        advance();

        auto right = parse_factor();

        auto binary_expr = std::make_shared<BinaryExpression>(left, right, token_to_operator(op_type));

        left = binary_expr;
    }

    return left;
}

std::shared_ptr<Expression> Parser::parse_factor() {
    if (match(TokenType::LPAREN)) {
        advance();
        auto expr = parse_expression();
        consume(TokenType::RPAREN, "Expected )");
        return expr;
    } else if (match(TokenType::IDENTIFIER)) {
        return parse_column_reference();
    } else if (match(TokenType::NUMBER_LITERAL) || match(TokenType::STRING_LITERAL)) {
        return parse_literal();
    } else {
        report_error("Unexpected token in expression: " + current_token().value);
        return nullptr;
    }
}

std::shared_ptr<Expression> Parser::parse_primary() {
    if (match(TokenType::IDENTIFIER)) {
        return parse_column_reference();
    } else if (match(TokenType::NUMBER_LITERAL) || match(TokenType::STRING_LITERAL)) {
        return parse_literal();
    } else {
        report_error("Unexpected token in primary expression: " + current_token().value);
        return nullptr;
    }
}

std::vector<std::shared_ptr<Expression>> Parser::parse_select_list() {
    std::vector<std::shared_ptr<Expression>> columns;

    do {
        if (match(TokenType::MULTIPLY)) {
            // SELECT *
            auto star_expr = std::make_shared<ColumnReference>("", "*");
            columns.push_back(star_expr);
            advance();
            break;
        } else {
            columns.push_back(parse_expression());
        }
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::vector<std::string> Parser::parse_from_clause() {
    std::vector<std::string> tables;

    do {
        if (match(TokenType::IDENTIFIER)) {
            tables.push_back(current_token().value);
            advance();
        } else {
            report_error("Expected table name");
            break;
        }
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return tables;
}

std::shared_ptr<Expression> Parser::parse_where_clause() {
    return parse_condition();
}

std::vector<std::shared_ptr<Expression>> Parser::parse_group_by_clause() {
    std::vector<std::shared_ptr<Expression>> columns;

    do {
        columns.push_back(parse_expression());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::shared_ptr<Expression> Parser::parse_having_clause() {
    return parse_condition();
}

std::vector<std::shared_ptr<Expression>> Parser::parse_order_by_clause() {
    std::vector<std::shared_ptr<Expression>> columns;

    do {
        columns.push_back(parse_expression());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::shared_ptr<Expression> Parser::parse_limit_clause() {
    return parse_expression();
}

std::shared_ptr<Expression> Parser::parse_offset_clause() {
    return parse_expression();
}

std::vector<std::string> Parser::parse_column_list() {
    std::vector<std::string> columns;

    do {
        if (match(TokenType::IDENTIFIER)) {
            columns.push_back(current_token().value);
            advance();
        } else {
            report_error("Expected column name");
            break;
        }
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::vector<std::vector<std::shared_ptr<Expression>>> Parser::parse_values_list() {
    std::vector<std::vector<std::shared_ptr<Expression>>> values_list;

    do {
        consume(TokenType::LPAREN, "Expected (");

        std::vector<std::shared_ptr<Expression>> row_values;
        do {
            row_values.push_back(parse_expression());
        } while (match(TokenType::COMMA) && advance_and_return_true());

        consume(TokenType::RPAREN, "Expected )");
        values_list.push_back(std::move(row_values));

    } while (match(TokenType::COMMA) && advance_and_return_true());

    return values_list;
}

std::vector<std::pair<std::string, std::shared_ptr<Expression>>> Parser::parse_set_clause() {
    std::vector<std::pair<std::string, std::shared_ptr<Expression>>> set_items;

    do {
        if (match(TokenType::IDENTIFIER)) {
            std::string column_name = current_token().value;
            advance();

            consume(TokenType::ASSIGN, "Expected =");

            auto value = parse_expression();
            set_items.emplace_back(column_name, value);
        } else {
            report_error("Expected column name");
            break;
        }
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return set_items;
}

std::vector<std::shared_ptr<Expression>> Parser::parse_column_definitions() {
    std::vector<std::shared_ptr<Expression>> columns;

    do {
        columns.push_back(parse_column_definition());
    } while (match(TokenType::COMMA) && advance_and_return_true());

    return columns;
}

std::shared_ptr<Expression> Parser::parse_column_definition() {
    if (match(TokenType::IDENTIFIER)) {
        auto col_ref = std::make_shared<ColumnReference>("", current_token().value);
        advance();
        return col_ref;
    } else {
        report_error("Expected column name");
        return nullptr;
    }
}

std::shared_ptr<Expression> Parser::parse_function_call() {
    // 简化实现，只支持基本的函数调用
    if (match(TokenType::IDENTIFIER)) {
        std::string func_name = current_token().value;
        advance();

        consume(TokenType::LPAREN, "Expected (");

        std::vector<std::shared_ptr<Expression>> args;
        if (!match(TokenType::RPAREN)) {
            do {
                args.push_back(parse_expression());
            } while (match(TokenType::COMMA) && advance_and_return_true());
        }

        consume(TokenType::RPAREN, "Expected )");

        auto func_call = std::make_shared<FunctionCall>(func_name);
        func_call->arguments = std::move(args);

        return func_call;
    }

    report_error("Expected function name");
    return nullptr;
}

std::shared_ptr<Expression> Parser::parse_column_reference() {
    if (match(TokenType::IDENTIFIER)) {
        auto col_ref = std::make_shared<ColumnReference>("", current_token().value);
        advance();
        return col_ref;
    }

    report_error("Expected column name");
    return nullptr;
}

std::shared_ptr<Expression> Parser::parse_literal() {
    if (match(TokenType::NUMBER_LITERAL)) {
        auto literal = std::make_shared<Literal>(Literal::Type::INTEGER, current_token().value);
        advance();
        return literal;
    } else if (match(TokenType::STRING_LITERAL)) {
        auto literal = std::make_shared<Literal>(Literal::Type::STRING, current_token().value);
        advance();
        return literal;
    }

    report_error("Expected literal");
    return nullptr;
}

Token Parser::current_token() const {
    if (current_token_ < tokens_.size()) {
        return tokens_[current_token_];
    }
    return Token(TokenType::END_OF_FILE, "", 0, 0);
}

Token Parser::peek_token() const {
    if (current_token_ + 1 < tokens_.size()) {
        return tokens_[current_token_ + 1];
    }
    return Token(TokenType::END_OF_FILE, "", 0, 0);
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
    return current_token().type == TokenType::IDENTIFIER &&
           current_token().value == keyword;
}

void Parser::consume(TokenType type, const std::string& error_msg) {
    if (match(type)) {
        advance();
    } else {
        report_error(error_msg + ", got: " + current_token().value);
    }
}

void Parser::consume_keyword(const std::string& keyword, const std::string& error_msg) {
    if (match_keyword(keyword)) {
        advance();
    } else {
        report_error(error_msg + ", got: " + current_token().value);
    }
}

void Parser::report_error(const std::string& message) {
    error_ = message;
}

void Parser::synchronize() {
    advance();

    while (!is_eof() && !match(TokenType::SEMICOLON)) {
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
    return type == TokenType::AND || type == TokenType::OR || type == TokenType::NOT;
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
        default: return BinaryExpression::Operator::ADD; // 默认值
    }
}

} // namespace sealdb
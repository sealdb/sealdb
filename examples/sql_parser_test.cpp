#include "sealdb/lexer.h"
#include "sealdb/parser.h"
#include "sealdb/logger.h"
#include <iostream>
#include <memory>

using namespace sealdb;

/**
 * @brief AST打印访问者
 */
class ASTPrinter : public ASTVisitor {
public:
    void visit(LiteralExpression* expr) override {
        std::cout << "Literal(" << expr->get_value() << ")";
    }

    void visit(IdentifierExpression* expr) override {
        std::cout << "Identifier(" << expr->get_name() << ")";
    }

    void visit(BinaryExpression* expr) override {
        std::cout << "Binary(";
        expr->get_left()->accept(*this);
        std::cout << " " << operator_to_string(expr->get_operator()) << " ";
        expr->get_right()->accept(*this);
        std::cout << ")";
    }

    void visit(FunctionCallExpression* expr) override {
        std::cout << "Function(" << expr->get_name() << "(";
        for (size_t i = 0; i < expr->get_arguments().size(); ++i) {
            if (i > 0) std::cout << ", ";
            expr->get_arguments()[i]->accept(*this);
        }
        std::cout << "))";
    }

    void visit(ColumnReference* expr) override {
        std::cout << "Column(" << expr->get_table_name() << "." << expr->get_column_name() << ")";
    }

    void visit(SelectStatement* stmt) override {
        std::cout << "Select(";
        for (size_t i = 0; i < stmt->get_select_list().size(); ++i) {
            if (i > 0) std::cout << ", ";
            stmt->get_select_list()[i]->accept(*this);
        }
        std::cout << " FROM ";
        for (size_t i = 0; i < stmt->get_from_tables().size(); ++i) {
            if (i > 0) std::cout << ", ";
            std::cout << stmt->get_from_tables()[i];
        }
        if (stmt->get_where_clause()) {
            std::cout << " WHERE ";
            stmt->get_where_clause()->accept(*this);
        }
        std::cout << ")";
    }

    void visit(InsertStatement* stmt) override {
        std::cout << "Insert(" << stmt->get_table_name() << ")";
    }

    void visit(UpdateStatement* stmt) override {
        std::cout << "Update(" << stmt->get_table_name() << ")";
    }

    void visit(DeleteStatement* stmt) override {
        std::cout << "Delete(" << stmt->get_table_name() << ")";
    }

    void visit(CreateTableStatement* stmt) override {
        std::cout << "CreateTable(" << stmt->get_table_name() << ")";
    }

    void visit(DropTableStatement* stmt) override {
        std::cout << "DropTable(" << stmt->get_table_name() << ")";
    }

private:
    std::string operator_to_string(BinaryExpression::Operator op) {
        switch (op) {
            case BinaryExpression::Operator::ADD: return "+";
            case BinaryExpression::Operator::SUBTRACT: return "-";
            case BinaryExpression::Operator::MULTIPLY: return "*";
            case BinaryExpression::Operator::DIVIDE: return "/";
            case BinaryExpression::Operator::MOD: return "%";
            case BinaryExpression::Operator::EQUAL: return "=";
            case BinaryExpression::Operator::NOT_EQUAL: return "!=";
            case BinaryExpression::Operator::LESS: return "<";
            case BinaryExpression::Operator::LESS_EQUAL: return "<=";
            case BinaryExpression::Operator::GREATER: return ">";
            case BinaryExpression::Operator::GREATER_EQUAL: return ">=";
            case BinaryExpression::Operator::AND: return "AND";
            case BinaryExpression::Operator::OR: return "OR";
            default: return "?";
        }
    }
};

void test_lexer() {
    std::cout << "\n=== Testing Lexer ===" << std::endl;

    std::string sql = "SELECT id, name, age FROM users WHERE age > 18 AND name = 'John'";
    Lexer lexer(sql);

    std::cout << "SQL: " << sql << std::endl;
    std::cout << "Tokens:" << std::endl;

    std::vector<Token> tokens = lexer.tokenize();
    for (const auto& token : tokens) {
        std::cout << "  " << token.to_string() << std::endl;
    }
}

void test_parser(const std::string& sql) {
    std::cout << "\n=== Testing Parser ===" << std::endl;
    std::cout << "SQL: " << sql << std::endl;

    Parser parser(sql);
    auto statement = parser.parse();

    if (parser.has_error()) {
        std::cout << "Parse Error: " << parser.get_error() << std::endl;
        return;
    }

    if (statement) {
        ASTPrinter printer;
        statement->accept(printer);
        std::cout << std::endl;
    } else {
        std::cout << "Failed to parse statement" << std::endl;
    }
}

int main() {
    Logger::info("Starting SQL Parser Test");

    // 测试词法分析器
    test_lexer();

    // 测试各种SQL语句
    std::vector<std::string> test_sqls = {
        "SELECT id, name, age FROM users WHERE age > 18",
        "SELECT * FROM users WHERE name = 'John' AND age >= 25",
        "INSERT INTO users (name, age) VALUES ('Alice', 25), ('Bob', 30)",
        "UPDATE users SET age = 26 WHERE name = 'Alice'",
        "DELETE FROM users WHERE age < 18",
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), age INT)",
        "DROP TABLE users"
    };

    for (const auto& sql : test_sqls) {
        test_parser(sql);
    }

    Logger::info("SQL Parser Test completed");
    return 0;
}
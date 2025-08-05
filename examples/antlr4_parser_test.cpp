#include "sealdb/parser_interface.h"
#include "sealdb/logger.h"
#include <iostream>
#include <chrono>
#include <vector>
#include <memory>

using namespace sealdb;

/**
 * @brief 测试ANTLR4解析器的基本功能
 */
void test_basic_functionality() {
    std::cout << "\n=== Testing ANTLR4 Parser Basic Functionality ===" << std::endl;

    // 创建ANTLR4解析器
    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    if (!parser) {
        std::cout << "Failed to create ANTLR4 parser" << std::endl;
        return;
    }

    std::cout << "Parser name: " << parser->getName() << std::endl;
    std::cout << "Parser available: " << (parser->isAvailable() ? "Yes" : "No") << std::endl;

    // 基本SQL语句测试
    std::vector<std::string> basic_sqls = {
        "SELECT * FROM users",
        "SELECT id, name FROM users",
        "SELECT id, name FROM users WHERE age > 18",
        "INSERT INTO users (name, age) VALUES ('John', 25)",
        "UPDATE users SET age = 26 WHERE name = 'John'",
        "DELETE FROM users WHERE age < 18",
        "CREATE TABLE users (id INT, name VARCHAR(50))",
        "DROP TABLE users"
    };

    for (const auto& sql : basic_sqls) {
        std::cout << "\nTesting SQL: " << sql << std::endl;

        auto result = parser->parse(sql);

        if (result.success) {
            std::cout << "  ✓ Parse Success" << std::endl;
            if (result.ast) {
                std::cout << "  ✓ AST created successfully" << std::endl;
            }
        } else {
            std::cout << "  ✗ Parse Failed:" << std::endl;
            for (const auto& error : result.errors) {
                std::cout << "    Error: " << error.message;
                if (error.line > 0) {
                    std::cout << " at line " << error.line;
                }
                if (error.column > 0) {
                    std::cout << ", column " << error.column;
                }
                std::cout << std::endl;
            }
        }
    }
}

/**
 * @brief 测试ANTLR4解析器的高级功能
 */
void test_advanced_functionality() {
    std::cout << "\n=== Testing ANTLR4 Parser Advanced Functionality ===" << std::endl;

    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    if (!parser) {
        std::cout << "Failed to create ANTLR4 parser" << std::endl;
        return;
    }

    // 高级SQL语句测试
    std::vector<std::string> advanced_sqls = {
        // 复杂SELECT语句
        "SELECT u.id, u.name, COUNT(o.id) as order_count FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.age > 18 GROUP BY u.id, u.name HAVING COUNT(o.id) > 0 ORDER BY order_count DESC LIMIT 10",

        // 子查询
        "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders WHERE amount > 100)",

        // 聚合函数
        "SELECT department, AVG(salary) as avg_salary, MAX(salary) as max_salary, MIN(salary) as min_salary FROM employees GROUP BY department HAVING AVG(salary) > 50000",

        // 复杂INSERT语句
        "INSERT INTO users (name, email, age, created_at) VALUES ('Alice', 'alice@example.com', 25, NOW()), ('Bob', 'bob@example.com', 30, NOW())",

        // 复杂UPDATE语句
        "UPDATE users SET last_login = NOW(), login_count = login_count + 1 WHERE id = 123 AND status = 'active'",

        // 复杂DELETE语句
        "DELETE FROM users WHERE last_login < DATE_SUB(NOW(), INTERVAL 1 YEAR) AND status = 'inactive'",

        // 复杂CREATE TABLE语句
        "CREATE TABLE products (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(100) NOT NULL, price DECIMAL(10,2) DEFAULT 0.00, category_id INT, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, FOREIGN KEY (category_id) REFERENCES categories(id))",

        // 索引创建
        "CREATE INDEX idx_users_email ON users(email)",
        "CREATE UNIQUE INDEX idx_users_username ON users(username)"
    };

    for (const auto& sql : advanced_sqls) {
        std::cout << "\nTesting Advanced SQL: " << sql << std::endl;

        auto result = parser->parse(sql);

        if (result.success) {
            std::cout << "  ✓ Advanced Parse Success" << std::endl;
            if (result.ast) {
                std::cout << "  ✓ Advanced AST created successfully" << std::endl;
            }
        } else {
            std::cout << "  ✗ Advanced Parse Failed:" << std::endl;
            for (const auto& error : result.errors) {
                std::cout << "    Error: " << error.message;
                if (error.line > 0) {
                    std::cout << " at line " << error.line;
                }
                if (error.column > 0) {
                    std::cout << ", column " << error.column;
                }
                std::cout << std::endl;
            }
        }
    }
}

/**
 * @brief 测试ANTLR4解析器的错误处理
 */
void test_error_handling() {
    std::cout << "\n=== Testing ANTLR4 Parser Error Handling ===" << std::endl;

    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    if (!parser) {
        std::cout << "Failed to create ANTLR4 parser" << std::endl;
        return;
    }

    // 包含错误的SQL语句
    std::vector<std::string> error_sqls = {
        "SELECT * FROM",  // 缺少表名
        "SELECT * FROM users WHERE",  // 缺少条件
        "INSERT INTO users VALUES",  // 缺少值
        "UPDATE users SET",  // 缺少SET子句
        "DELETE FROM",  // 缺少表名
        "CREATE TABLE",  // 缺少表定义
        "SELECT * FROM users WHERE age > 'invalid'",  // 类型错误
        "SELECT * FROM users GROUP BY",  // 缺少分组列
        "SELECT * FROM users ORDER BY",  // 缺少排序列
        "SELECT * FROM users LIMIT",  // 缺少限制数量
    };

    for (const auto& sql : error_sqls) {
        std::cout << "\nTesting Error SQL: " << sql << std::endl;

        auto result = parser->parse(sql);

        if (!result.success) {
            std::cout << "  ✓ Error correctly detected:" << std::endl;
            for (const auto& error : result.errors) {
                std::cout << "    Error: " << error.message;
                if (error.line > 0) {
                    std::cout << " at line " << error.line;
                }
                if (error.column > 0) {
                    std::cout << ", column " << error.column;
                }
                std::cout << std::endl;
            }
        } else {
            std::cout << "  ✗ Error not detected (unexpected success)" << std::endl;
        }
    }
}

/**
 * @brief 测试ANTLR4解析器的性能
 */
void test_performance() {
    std::cout << "\n=== Testing ANTLR4 Parser Performance ===" << std::endl;

    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    if (!parser) {
        std::cout << "Failed to create ANTLR4 parser" << std::endl;
        return;
    }

    // 测试SQL语句
    std::vector<std::string> test_sqls = {
        "SELECT * FROM users WHERE age > 18",
        "SELECT id, name, email FROM users WHERE status = 'active' AND age BETWEEN 18 AND 65",
        "INSERT INTO users (name, email, age) VALUES ('John', 'john@example.com', 25)",
        "UPDATE users SET last_login = NOW() WHERE id = 123",
        "DELETE FROM users WHERE last_login < DATE_SUB(NOW(), INTERVAL 1 YEAR)",
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100), email VARCHAR(255))"
    };

    const int iterations = 1000;

    for (const auto& sql : test_sqls) {
        std::cout << "\nTesting SQL: " << sql << std::endl;

        auto start = std::chrono::high_resolution_clock::now();

        for (int i = 0; i < iterations; ++i) {
            auto result = parser->parse(sql);

            if (!result.success && i == 0) {
                std::cout << "  Parse Error: ";
                for (const auto& error : result.errors) {
                    std::cout << error.message;
                    if (error.line > 0) {
                        std::cout << " at line " << error.line;
                    }
                    if (error.column > 0) {
                        std::cout << ", column " << error.column;
                    }
                }
                std::cout << std::endl;
                break;
            }
        }

        auto end = std::chrono::high_resolution_clock::now();
        auto duration = std::chrono::duration_cast<std::chrono::microseconds>(end - start);

        std::cout << "  Performance: " << iterations << " iterations in "
                 << duration.count() << " microseconds" << std::endl;
        std::cout << "  Average: " << (duration.count() / iterations) << " microseconds per parse" << std::endl;
    }
}

/**
 * @brief 测试ANTLR4解析器与其他解析器的比较
 */
void test_parser_comparison() {
    std::cout << "\n=== Testing Parser Comparison ===" << std::endl;

    std::string test_sql = "SELECT id, name, age FROM users WHERE age > 18 AND status = 'active' ORDER BY name LIMIT 10";
    const int iterations = 100;

    // 测试所有可用的解析器
    auto availableTypes = ParserFactory::getAvailableParserTypes();

    for (auto type : availableTypes) {
        auto parser = ParserFactory::createParser(type);
        if (!parser) continue;

        std::cout << "\nTesting " << parser->getName() << std::endl;

        // 功能测试
        auto result = parser->parse(test_sql);
        if (result.success) {
            std::cout << "  ✓ Functionality: PASS" << std::endl;
        } else {
            std::cout << "  ✗ Functionality: FAIL" << std::endl;
            for (const auto& error : result.errors) {
                std::cout << "    Error: " << error.message << std::endl;
            }
        }

        // 性能测试
        auto start = std::chrono::high_resolution_clock::now();

        for (int i = 0; i < iterations; ++i) {
            parser->parse(test_sql);
        }

        auto end = std::chrono::high_resolution_clock::now();
        auto duration = std::chrono::duration_cast<std::chrono::microseconds>(end - start);

        std::cout << "  Performance: " << iterations << " iterations in "
                 << duration.count() << " microseconds" << std::endl;
        std::cout << "  Average: " << (duration.count() / iterations) << " microseconds per parse" << std::endl;
    }
}

/**
 * @brief 测试ANTLR4解析器的语法特性支持
 */
void test_syntax_features() {
    std::cout << "\n=== Testing ANTLR4 Parser Syntax Features ===" << std::endl;

    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    if (!parser) {
        std::cout << "Failed to create ANTLR4 parser" << std::endl;
        return;
    }

    // 测试各种语法特性
    std::vector<std::pair<std::string, std::string>> syntax_tests = {
        {"Basic SELECT", "SELECT * FROM users"},
        {"Column Aliases", "SELECT id as user_id, name as user_name FROM users"},
        {"Table Aliases", "SELECT u.id, u.name FROM users u"},
        {"WHERE Conditions", "SELECT * FROM users WHERE age > 18 AND status = 'active'"},
        {"ORDER BY", "SELECT * FROM users ORDER BY name ASC, age DESC"},
        {"LIMIT/OFFSET", "SELECT * FROM users LIMIT 10 OFFSET 20"},
        {"GROUP BY", "SELECT department, COUNT(*) FROM employees GROUP BY department"},
        {"HAVING", "SELECT department, AVG(salary) FROM employees GROUP BY department HAVING AVG(salary) > 50000"},
        {"JOIN", "SELECT u.name, o.order_date FROM users u JOIN orders o ON u.id = o.user_id"},
        {"LEFT JOIN", "SELECT u.name, o.order_date FROM users u LEFT JOIN orders o ON u.id = o.user_id"},
        {"Subquery", "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders)"},
        {"Aggregate Functions", "SELECT COUNT(*), SUM(amount), AVG(amount) FROM orders"},
        {"String Functions", "SELECT CONCAT(first_name, ' ', last_name) as full_name FROM users"},
        {"Date Functions", "SELECT * FROM orders WHERE order_date > DATE_SUB(NOW(), INTERVAL 1 MONTH)"},
        {"CASE Statement", "SELECT name, CASE WHEN age < 18 THEN 'minor' WHEN age < 65 THEN 'adult' ELSE 'senior' END as age_group FROM users"}
    };

    for (const auto& [feature, sql] : syntax_tests) {
        std::cout << "\nTesting " << feature << ": " << sql << std::endl;

        auto result = parser->parse(sql);

        if (result.success) {
            std::cout << "  ✓ " << feature << " supported" << std::endl;
        } else {
            std::cout << "  ✗ " << feature << " not supported:" << std::endl;
            for (const auto& error : result.errors) {
                std::cout << "    Error: " << error.message << std::endl;
            }
        }
    }
}

int main() {
    std::cout << "=== ANTLR4 Parser Test Suite ===" << std::endl;

    // 检查ANTLR4解析器是否可用
    if (!ParserFactory::isParserTypeAvailable(ParserType::ANTLR4)) {
        std::cout << "ANTLR4 parser is not available. Please check your installation." << std::endl;
        return 1;
    }

    // 运行所有测试
    test_basic_functionality();
    test_advanced_functionality();
    test_error_handling();
    test_performance();
    test_parser_comparison();
    test_syntax_features();

    std::cout << "\n=== ANTLR4 Parser Test Complete ===" << std::endl;
    return 0;
}
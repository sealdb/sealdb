#include "sealdb/parser_factory.h"
#include "sealdb/logger.h"
#include <iostream>
#include <chrono>
#include <vector>

using namespace sealdb;

/**
 * @brief 测试解析器的性能
 */
void test_performance() {
    std::cout << "\n=== Testing Parser Performance ===" << std::endl;

    // 测试SQL语句
    std::vector<std::string> test_sqls = {
        "SELECT id, name, age FROM users WHERE age > 18",
        "SELECT * FROM users WHERE name = 'John' AND age >= 25",
        "INSERT INTO users (name, age) VALUES ('Alice', 25)",
        "UPDATE users SET age = 26 WHERE name = 'Alice'",
        "DELETE FROM users WHERE age < 18",
        "CREATE TABLE users (id INT, name VARCHAR(50), age INT)",
        "DROP TABLE users"
    };

    // 创建解析器
    auto parser = ParserFactory::createDefaultParser();
    if (!parser) {
        std::cout << "Failed to create parser" << std::endl;
        return;
    }

    // 性能测试
    const int iterations = 10000;

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
 * @brief 测试解析器功能
 */
void test_parser() {
    std::cout << "\n=== Testing Parser ===" << std::endl;

    std::vector<std::string> test_sqls = {
        "SELECT id, name, age FROM users WHERE age > 18",
        "SELECT * FROM users WHERE name = 'John' AND age >= 25",
        "INSERT INTO users (name, age) VALUES ('Alice', 25)",
        "UPDATE users SET age = 26 WHERE name = 'Alice'",
        "DELETE FROM users WHERE age < 18",
        "CREATE TABLE users (id INT, name VARCHAR(50), age INT)",
        "DROP TABLE users"
    };

    // 创建解析器
    auto parser = ParserFactory::createDefaultParser();
    if (!parser) {
        std::cout << "Failed to create parser" << std::endl;
        return;
    }

    for (const auto& sql : test_sqls) {
        std::cout << "\nTesting SQL: " << sql << std::endl;

        auto result = parser->parse(sql);

        if (result.success) {
            std::cout << "  Parse Success: " << sql << std::endl;
            if (result.ast) {
                std::cout << "  AST created successfully" << std::endl;
            }
        } else {
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
        }
    }
}

/**
 * @brief 测试不同解析器的性能比较
 */
void performance_comparison() {
    std::cout << "\n=== Performance Comparison ===" << std::endl;

    std::string sql = "SELECT id, name, age FROM users WHERE age > 18 AND name = 'John'";
    const int iterations = 1000;

    // 测试可用的解析器
    auto availableTypes = ParserFactory::getAvailableParserTypes();
    
    for (auto type : availableTypes) {
        auto parser = ParserFactory::createParser(type);
        if (!parser) continue;

        std::cout << "\nTesting " << parser->getName() << std::endl;

        auto start = std::chrono::high_resolution_clock::now();

        for (int i = 0; i < iterations; ++i) {
            auto result = parser->parse(sql);
            if (!result.success && i == 0) {
                std::cout << "  Parse failed" << std::endl;
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

int main() {
    std::cout << "=== Flex + Bison Parser Test ===" << std::endl;

    // 测试解析器功能
    test_parser();

    // 测试性能
    test_performance();

    // 性能比较
    performance_comparison();

    std::cout << "\n=== Test Complete ===" << std::endl;
    return 0;
}
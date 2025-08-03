#include <gtest/gtest.h>
#include <memory>
#include <string>
#include "sealdb/parser_interface.h"
#include "sealdb/parser_factory.h"

using namespace sealdb;

class SQLParserTest : public ::testing::Test {
protected:
    void SetUp() override {
        parser_ = ParserFactory::createDefaultParser();
    }

    void TearDown() override {
        parser_.reset();
    }

    std::unique_ptr<ParserInterface> parser_;
};

TEST_F(SQLParserTest, ParseSimpleSelect) {
    std::string sql = "SELECT * FROM users";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    // 验证AST类型
    auto selectStmt = std::dynamic_pointer_cast<SelectStatement>(result.ast);
    ASSERT_TRUE(selectStmt != nullptr);
}

TEST_F(SQLParserTest, ParseSelectWithWhere) {
    std::string sql = "SELECT id, name FROM users WHERE age > 18";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    auto selectStmt = std::dynamic_pointer_cast<SelectStatement>(result.ast);
    ASSERT_TRUE(selectStmt != nullptr);
    ASSERT_TRUE(selectStmt->getWhereClause() != nullptr);
}

TEST_F(SQLParserTest, ParseInsertStatement) {
    std::string sql = "INSERT INTO users (id, name, age) VALUES (1, 'John', 25)";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    auto insertStmt = std::dynamic_pointer_cast<InsertStatement>(result.ast);
    ASSERT_TRUE(insertStmt != nullptr);
    ASSERT_EQ(insertStmt->getTableName(), "users");
}

TEST_F(SQLParserTest, ParseUpdateStatement) {
    std::string sql = "UPDATE users SET age = 26 WHERE id = 1";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    auto updateStmt = std::dynamic_pointer_cast<UpdateStatement>(result.ast);
    ASSERT_TRUE(updateStmt != nullptr);
    ASSERT_EQ(updateStmt->getTableName(), "users");
}

TEST_F(SQLParserTest, ParseDeleteStatement) {
    std::string sql = "DELETE FROM users WHERE id = 1";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    auto deleteStmt = std::dynamic_pointer_cast<DeleteStatement>(result.ast);
    ASSERT_TRUE(deleteStmt != nullptr);
    ASSERT_EQ(deleteStmt->getTableName(), "users");
}

TEST_F(SQLParserTest, ParseCreateTable) {
    std::string sql = "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), age INT)";
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast != nullptr);
    ASSERT_TRUE(result.errors.empty());
    
    auto createTableStmt = std::dynamic_pointer_cast<CreateTableStatement>(result.ast);
    ASSERT_TRUE(createTableStmt != nullptr);
    ASSERT_EQ(createTableStmt->getTableName(), "users");
}

TEST_F(SQLParserTest, ParseInvalidSQL) {
    std::string sql = "SELECT * FROM"; // 不完整的SQL
    
    auto result = parser_->parse(sql);
    
    ASSERT_TRUE(result.ast == nullptr);
    ASSERT_FALSE(result.errors.empty());
}

TEST_F(SQLParserTest, ParserFactoryTest) {
    // 测试解析器工厂
    auto antlr4Parser = ParserFactory::createParser(ParserType::ANTLR4);
    ASSERT_TRUE(antlr4Parser != nullptr);
    
    auto postgresqlParser = ParserFactory::createParser(ParserType::POSTGRESQL);
    ASSERT_TRUE(postgresqlParser == nullptr); // 暂未实现
    
    auto defaultParser = ParserFactory::createDefaultParser();
    ASSERT_TRUE(defaultParser != nullptr);
    
    auto configParser = ParserFactory::createParserFromConfig("antlr4");
    ASSERT_TRUE(configParser != nullptr);
}

TEST_F(SQLParserTest, AvailableParserTypesTest) {
    auto availableTypes = ParserFactory::getAvailableParserTypes();
    
    // 至少应该有ANTLR4解析器可用
    ASSERT_FALSE(availableTypes.empty());
    
    // 检查ANTLR4解析器是否可用
    ASSERT_TRUE(ParserFactory::isParserTypeAvailable(ParserType::ANTLR4));
    
    // PostgreSQL解析器暂未实现
    ASSERT_FALSE(ParserFactory::isParserTypeAvailable(ParserType::POSTGRESQL));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
} 
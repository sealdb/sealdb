#include "antlr4_parser.h"
#include <iostream>
#include <sstream>
#include <algorithm>
#include <cctype>

namespace sealdb {

Antlr4Parser::Antlr4Parser() {
#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
    errorListener_ = std::make_unique<ErrorListener>(this);
#endif
#endif
}

Antlr4Parser::~Antlr4Parser() = default;

ParseResult Antlr4Parser::parse(const std::string& sql) {
    clearErrors();

    try {
#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
        // 创建输入流
        antlr4::ANTLRInputStream input(sql);

        // 创建词法分析器
        SQLLexer lexer(&input);
        lexer.removeErrorListeners();
        lexer.addErrorListener(errorListener_.get());

        // 创建词法符号流
        antlr4::CommonTokenStream tokens(&lexer);

        // 创建语法分析器
        SQLParser parser(&tokens);
        parser.removeErrorListeners();
        parser.addErrorListener(errorListener_.get());

        // 解析SQL语句
        SQLParser::ParseContext* tree = parser.parse();

        // 检查是否有错误
        if (!errors_.empty()) {
            std::vector<ParseError> parseErrors;
            for (const auto& error : errors_) {
                parseErrors.emplace_back(error);
            }
            return ParseResult(parseErrors);
        }

        // 转换为AST
        auto ast = convertToAST(tree);
        if (ast) {
            return ParseResult(ast);
        } else {
            return ParseResult({ParseError("Failed to convert to AST")});
        }
#else
        // ANTLR4运行时不可用，使用基本实现
        return parseBasic(sql);
#endif
#else
        // ANTLR4解析器未启用，使用基本实现
        return parseBasic(sql);
#endif
    } catch (const std::exception& e) {
        addError("Parse error: " + std::string(e.what()));
        return ParseResult({ParseError("Parse error: " + std::string(e.what()))});
    }
}

ParseResult Antlr4Parser::parseBasic(const std::string& sql) {
    // 简单的语法检查（当ANTLR4不可用时使用）
    if (sql.empty()) {
        addError("Empty SQL statement");
        return ParseResult({ParseError("Empty SQL statement")});
    }

    // 检查基本的SQL关键字
    std::string upperSql = sql;
    std::transform(upperSql.begin(), upperSql.end(), upperSql.begin(), ::toupper);

    if (upperSql.find("SELECT") != std::string::npos) {
        // 简单的SELECT语句解析
        auto selectStmt = std::make_shared<SelectStatement>();
        // 这里可以添加更复杂的解析逻辑
        return ParseResult(selectStmt);
    } else if (upperSql.find("INSERT") != std::string::npos) {
        auto insertStmt = std::make_shared<InsertStatement>();
        return ParseResult(insertStmt);
    } else if (upperSql.find("UPDATE") != std::string::npos) {
        auto updateStmt = std::make_shared<UpdateStatement>();
        return ParseResult(updateStmt);
    } else if (upperSql.find("DELETE") != std::string::npos) {
        auto deleteStmt = std::make_shared<DeleteStatement>();
        return ParseResult(deleteStmt);
    } else if (upperSql.find("CREATE") != std::string::npos) {
        auto createStmt = std::make_shared<CreateTableStatement>();
        return ParseResult(createStmt);
    } else {
        addError("Unsupported SQL statement type");
        return ParseResult({ParseError("Unsupported SQL statement type")});
    }
}

std::shared_ptr<ASTNode> Antlr4Parser::parseToAST(const std::string& sql) {
    auto result = parse(sql);
    if (result.success && result.ast) {
        return std::static_pointer_cast<ASTNode>(result.ast);
    }
    return nullptr;
}

std::shared_ptr<ASTNode> Antlr4Parser::convertToAST(void* tree) {
#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
    if (!tree) {
        return nullptr;
    }

    // 这里应该实现真正的ANTLR4语法树转换
    // 由于需要具体的ANTLR4类型，这里暂时返回nullptr
    addError("ANTLR4 AST conversion not implemented yet");
    return nullptr;
#else
    // 当ANTLR4不可用时，返回nullptr
    return nullptr;
#endif
#else
    // 当ANTLR4不可用时，返回nullptr
    return nullptr;
#endif
}

std::shared_ptr<SelectStatement> Antlr4Parser::convertSelectStatement(void* ctx) {
    // 基本实现
    return std::make_shared<SelectStatement>();
}

std::shared_ptr<InsertStatement> Antlr4Parser::convertInsertStatement(void* ctx) {
    // 基本实现
    return std::make_shared<InsertStatement>();
}

std::shared_ptr<UpdateStatement> Antlr4Parser::convertUpdateStatement(void* ctx) {
    // 基本实现
    return std::make_shared<UpdateStatement>();
}

std::shared_ptr<DeleteStatement> Antlr4Parser::convertDeleteStatement(void* ctx) {
    // 基本实现
    return std::make_shared<DeleteStatement>();
}

std::shared_ptr<CreateTableStatement> Antlr4Parser::convertCreateTableStatement(void* ctx) {
    // 基本实现
    return std::make_shared<CreateTableStatement>();
}

std::shared_ptr<Expression> Antlr4Parser::convertExpression(void* ctx) {
    // 基本实现
    return std::make_shared<Literal>(Literal::Type::STRING, "");
}

std::shared_ptr<ColumnReference> Antlr4Parser::convertColumnReference(void* ctx) {
    // 基本实现
    return std::make_shared<ColumnReference>("", "");
}

std::shared_ptr<FunctionCall> Antlr4Parser::convertFunctionCall(void* ctx) {
    // 基本实现
    return std::make_shared<FunctionCall>("");
}

std::shared_ptr<Literal> Antlr4Parser::convertLiteral(void* ctx) {
    // 基本实现
    return std::make_shared<Literal>(Literal::Type::STRING, "");
}

void Antlr4Parser::addError(const std::string& error) {
    errors_.push_back(error);
}

#ifdef USE_ANTLR4_PARSER
#ifndef ANTLR4_NOT_AVAILABLE
void Antlr4Parser::ErrorListener::syntaxError(antlr4::Recognizer* recognizer, antlr4::Token* offendingSymbol,
                                              size_t line, size_t charPositionInLine, const std::string& msg,
                                              std::exception_ptr e) {
    std::ostringstream oss;
    oss << "Syntax error at line " << line << ":" << charPositionInLine << ": " << msg;
    parser_->addError(oss.str());
}
#endif
#endif

} // namespace sealdb
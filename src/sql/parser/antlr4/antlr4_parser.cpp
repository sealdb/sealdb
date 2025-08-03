#include "antlr4_parser.h"
#include <iostream>
#include <sstream>
#include <algorithm>
#include <cctype>

// 检查ANTLR4是否可用
#ifdef USE_ANTLR4_PARSER
// 尝试不同的头文件包含方式
#if __has_include(<antlr4-runtime.h>)
#include <antlr4-runtime.h>
#elif __has_include(<antlr4-runtime/antlr4-runtime.h>)
#include <antlr4-runtime/antlr4-runtime.h>
#else
#warning "ANTLR4 runtime headers not found, falling back to basic implementation"
#define ANTLR4_NOT_AVAILABLE
#endif

// 只有在找到ANTLR4头文件时才包含生成的解析器头文件
#ifndef ANTLR4_NOT_AVAILABLE
#include "SQLParser.h"
#include "SQLLexer.h"
#endif
#endif

namespace sealdb {

Antlr4Parser::Antlr4Parser() {
    errorListener_ = std::make_unique<ErrorListener>(this);
}

Antlr4Parser::~Antlr4Parser() = default;

ParseResult Antlr4Parser::parse(const std::string& sql) {
    clearErrors();

    try {
#ifdef USE_ANTLR4_PARSER
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
#endif
    } catch (const std::exception& e) {
        addError("Parse error: " + std::string(e.what()));
        return ParseResult({ParseError("Parse error: " + std::string(e.what()))});
    }
}

std::shared_ptr<ASTNode> Antlr4Parser::parseToAST(const std::string& sql) {
    auto result = parse(sql);
    if (result.success && result.ast) {
        return std::static_pointer_cast<ASTNode>(result.ast);
    }
    return nullptr;
}

#ifdef USE_ANTLR4_PARSER
std::shared_ptr<ASTNode> Antlr4Parser::convertToAST(antlr4::tree::ParseTree* tree) {
    if (!tree) {
        return nullptr;
    }

    // 根据节点类型进行转换
    if (auto ctx = dynamic_cast<SQLParser::SelectStatementContext*>(tree)) {
        return convertSelectStatement(ctx);
    } else if (auto ctx = dynamic_cast<SQLParser::InsertStatementContext*>(tree)) {
        return convertInsertStatement(ctx);
    } else if (auto ctx = dynamic_cast<SQLParser::UpdateStatementContext*>(tree)) {
        return convertUpdateStatement(ctx);
    } else if (auto ctx = dynamic_cast<SQLParser::DeleteStatementContext*>(tree)) {
        return convertDeleteStatement(ctx);
    } else if (auto ctx = dynamic_cast<SQLParser::CreateTableStatementContext*>(tree)) {
        return convertCreateTableStatement(ctx);
    }

    // 其他类型的语句可以在这里添加
    addError("Unsupported statement type");
    return nullptr;
}

std::shared_ptr<SelectStatement> Antlr4Parser::convertSelectStatement(SQLParser::SelectStatementContext* ctx) {
    if (!ctx) return nullptr;

    auto selectStmt = std::make_shared<SelectStatement>();

    // 转换选择列表
    if (ctx->selectList()) {
        for (auto item : ctx->selectList()->selectItem()) {
            if (auto all = item->selectAll()) {
                // 处理 SELECT *
            } else if (auto col = item->selectColumn()) {
                selectStmt->selectList.push_back(convertColumnReference(col));
            } else if (auto expr = item->selectExpression()) {
                auto expression = convertExpression(expr->expression());
                selectStmt->selectList.push_back(expression);
            }
        }
    }

    // 转换FROM子句
    if (ctx->tableReferenceList()) {
        for (auto ref : ctx->tableReferenceList()->tableReference()) {
            if (auto simple = ref->tableReferenceSimple()) {
                selectStmt->fromTable = simple->tableName()->getText();
            }
        }
    }

    // 转换WHERE子句
    if (ctx->whereClause()) {
        selectStmt->whereClause = convertExpression(ctx->whereClause()->expression());
    }

    return selectStmt;
}

std::shared_ptr<InsertStatement> Antlr4Parser::convertInsertStatement(SQLParser::InsertStatementContext* ctx) {
    if (!ctx) return nullptr;

    auto insertStmt = std::make_shared<InsertStatement>();

    // 设置表名
    insertStmt->tableName = ctx->tableName()->getText();

    // 转换列列表
    if (ctx->columnList()) {
        for (auto col : ctx->columnList()->identifier()) {
            insertStmt->columns.push_back(col->getText());
        }
    }

    // 转换值列表
    for (auto valueList : ctx->valueList()) {
        std::vector<std::shared_ptr<Expression>> values;
        for (auto expr : valueList->expression()) {
            values.push_back(convertExpression(expr));
        }
        insertStmt->values.push_back(values);
    }

    return insertStmt;
}

std::shared_ptr<UpdateStatement> Antlr4Parser::convertUpdateStatement(SQLParser::UpdateStatementContext* ctx) {
    if (!ctx) return nullptr;

    auto updateStmt = std::make_shared<UpdateStatement>();

    // 设置表名
    updateStmt->tableName = ctx->tableName()->getText();

    // 转换SET子句
    for (auto setClause : ctx->setClause()) {
        auto columnRef = convertColumnReference(setClause->columnReference());
        auto expression = convertExpression(setClause->expression());
        updateStmt->setClause.emplace_back(setClause->columnReference()->getText(), expression);
    }

    // 转换WHERE子句
    if (ctx->whereClause()) {
        updateStmt->whereClause = convertExpression(ctx->whereClause()->expression());
    }

    return updateStmt;
}

std::shared_ptr<DeleteStatement> Antlr4Parser::convertDeleteStatement(SQLParser::DeleteStatementContext* ctx) {
    if (!ctx) return nullptr;

    auto deleteStmt = std::make_shared<DeleteStatement>();

    // 设置表名
    deleteStmt->tableName = ctx->tableName()->getText();

    // 转换WHERE子句
    if (ctx->whereClause()) {
        deleteStmt->whereClause = convertExpression(ctx->whereClause()->expression());
    }

    return deleteStmt;
}

std::shared_ptr<CreateTableStatement> Antlr4Parser::convertCreateTableStatement(SQLParser::CreateTableStatementContext* ctx) {
    if (!ctx) return nullptr;

    auto createTableStmt = std::make_shared<CreateTableStatement>();

    // 设置表名
    createTableStmt->tableName = ctx->tableName()->getText();

    // 转换列定义
    for (auto colDef : ctx->columnDefinition()) {
        auto columnDef = std::make_shared<Expression>();
        createTableStmt->columns.push_back(columnDef);
    }

    return createTableStmt;
}

std::shared_ptr<Expression> Antlr4Parser::convertExpression(SQLParser::ExpressionContext* ctx) {
    if (!ctx) return nullptr;

    // 字面量表达式
    if (ctx->literalExpression()) {
        return convertLiteral(ctx->literalExpression()->literal());
    }

    // 列引用表达式
    if (ctx->columnExpression()) {
        return convertColumnReference(ctx->columnExpression()->columnReference());
    }

    // 括号表达式
    if (ctx->parenthesizedExpression()) {
        return convertExpression(ctx->parenthesizedExpression()->expression());
    }

    // 二元表达式
    if (ctx->binaryExpression()) {
        auto left = convertExpression(ctx->binaryExpression()->expression(0));
        auto right = convertExpression(ctx->binaryExpression()->expression(1));
        // 这里应该创建二元表达式节点
        return left; // 临时返回左操作数
    }

    // 函数表达式
    if (ctx->functionExpression()) {
        return convertFunctionCall(ctx->functionExpression()->functionCall());
    }

    // 一元表达式
    if (ctx->unaryExpression()) {
        auto expr = convertExpression(ctx->unaryExpression()->expression());
        return expr; // 临时返回表达式
    }

    // 其他表达式类型可以在这里添加
    addError("Unsupported expression type");
    return nullptr;
}

std::shared_ptr<ColumnReference> Antlr4Parser::convertColumnReference(SQLParser::ColumnReferenceContext* ctx) {
    if (!ctx) return nullptr;

    std::string tableName;
    std::string columnName;

    if (ctx->tableName()) {
        tableName = ctx->tableName()->getText();
    }

    if (ctx->identifier()) {
        columnName = ctx->identifier()->getText();
    }

    return std::make_shared<ColumnReference>(tableName, columnName);
}

std::shared_ptr<FunctionCall> Antlr4Parser::convertFunctionCall(SQLParser::FunctionCallContext* ctx) {
    if (!ctx) return nullptr;

    std::string functionName = ctx->functionName()->getText();

    std::vector<std::shared_ptr<Expression>> arguments;
    if (ctx->expression()) {
        for (auto expr : ctx->expression()) {
            arguments.push_back(convertExpression(expr));
        }
    }

    auto funcCall = std::make_shared<FunctionCall>(functionName);
    funcCall->arguments = arguments;
    return funcCall;
}

std::shared_ptr<Literal> Antlr4Parser::convertLiteral(SQLParser::LiteralContext* ctx) {
    if (!ctx) return nullptr;

    if (ctx->STRING_LITERAL()) {
        std::string value = ctx->STRING_LITERAL()->getText();
        // 移除引号
        if (value.length() >= 2) {
            value = value.substr(1, value.length() - 2);
        }
        return std::make_shared<Literal>(Literal::Type::STRING, value);
    } else if (ctx->INTEGER_LITERAL()) {
        std::string value = ctx->INTEGER_LITERAL()->getText();
        return std::make_shared<Literal>(Literal::Type::INTEGER, value);
    } else if (ctx->FLOAT_LITERAL()) {
        std::string value = ctx->FLOAT_LITERAL()->getText();
        return std::make_shared<Literal>(Literal::Type::FLOAT, value);
    } else if (ctx->BOOLEAN_LITERAL()) {
        std::string value = ctx->BOOLEAN_LITERAL()->getText();
        return std::make_shared<Literal>(Literal::Type::BOOLEAN, value);
    } else if (ctx->NULL_LITERAL()) {
        return std::make_shared<Literal>(Literal::Type::NULL_VALUE, "NULL");
    }

    addError("Unsupported literal type");
    return nullptr;
}
#else
std::shared_ptr<ASTNode> Antlr4Parser::convertToAST(void* tree) {
    // 当ANTLR4不可用时，返回nullptr
    return nullptr;
}

std::shared_ptr<SelectStatement> Antlr4Parser::convertSelectStatement(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<SelectStatement>();
}

std::shared_ptr<InsertStatement> Antlr4Parser::convertInsertStatement(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<InsertStatement>();
}

std::shared_ptr<UpdateStatement> Antlr4Parser::convertUpdateStatement(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<UpdateStatement>();
}

std::shared_ptr<DeleteStatement> Antlr4Parser::convertDeleteStatement(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<DeleteStatement>();
}

std::shared_ptr<CreateTableStatement> Antlr4Parser::convertCreateTableStatement(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<CreateTableStatement>();
}

std::shared_ptr<Expression> Antlr4Parser::convertExpression(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<Literal>(Literal::Type::STRING, "");
}

std::shared_ptr<ColumnReference> Antlr4Parser::convertColumnReference(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<ColumnReference>("", "");
}

std::shared_ptr<FunctionCall> Antlr4Parser::convertFunctionCall(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<FunctionCall>("");
}

std::shared_ptr<Literal> Antlr4Parser::convertLiteral(void* ctx) {
    // 当ANTLR4不可用时，返回基本实现
    return std::make_shared<Literal>(Literal::Type::STRING, "");
}
#endif

void Antlr4Parser::addError(const std::string& error) {
    errors_.push_back(error);
}

void Antlr4Parser::ErrorListener::syntaxError(antlr4::Recognizer* recognizer, antlr4::Token* offendingSymbol,
                                              size_t line, size_t charPositionInLine, const std::string& msg,
                                              std::exception_ptr e) {
    std::ostringstream oss;
    oss << "Syntax error at line " << line << ":" << charPositionInLine << ": " << msg;
    parser_->addError(oss.str());
}

} // namespace sealdb
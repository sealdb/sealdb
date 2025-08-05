#ifndef SEALDB_AST_H
#define SEALDB_AST_H

#include <memory>
#include <string>
#include <vector>

namespace sealdb {

/**
 * AST节点基类
 */
class ASTNode {
public:
    virtual ~ASTNode() = default;
    virtual std::string getType() const = 0;
};

/**
 * 表达式基类
 */
class Expression : public ASTNode {
public:
    virtual ~Expression() = default;
    std::string getType() const override { return "Expression"; }
};

/**
 * 列引用
 */
class ColumnReference : public Expression {
public:
    std::string tableName;
    std::string columnName;

    ColumnReference(const std::string& table, const std::string& column)
        : tableName(table), columnName(column) {}

    std::string getType() const override { return "ColumnReference"; }
};

/**
 * 字面量
 */
class Literal : public Expression {
public:
    enum class Type {
        STRING,
        INTEGER,
        FLOAT,
        BOOLEAN,
        NULL_VALUE
    };

    Type type;
    std::string value;

    Literal(Type t, const std::string& val) : type(t), value(val) {}

    std::string getType() const override { return "Literal"; }
};

/**
 * 二元表达式
 */
class BinaryExpression : public Expression {
public:
    enum class Operator {
        ADD, SUBTRACT, MULTIPLY, DIVIDE, MOD,
        EQUAL, NOT_EQUAL, LESS, LESS_EQUAL, GREATER, GREATER_EQUAL,
        AND, OR
    };

    std::shared_ptr<Expression> left;
    std::shared_ptr<Expression> right;
    Operator op;

    BinaryExpression() = default;
    BinaryExpression(std::shared_ptr<Expression> l, std::shared_ptr<Expression> r, Operator o)
        : left(l), right(r), op(o) {}

    std::string getType() const override { return "BinaryExpression"; }
};

/**
 * 函数调用
 */
class FunctionCall : public Expression {
public:
    std::string functionName;
    std::vector<std::shared_ptr<Expression>> arguments;

    FunctionCall(const std::string& name) : functionName(name) {}

    std::string getType() const override { return "FunctionCall"; }
};

/**
 * SQL语句基类
 */
class Statement : public ASTNode {
public:
    virtual ~Statement() = default;
    std::string getType() const override { return "Statement"; }
};

/**
 * SELECT语句
 */
class SelectStatement : public Statement {
public:
    std::vector<std::shared_ptr<Expression>> selectList;
    std::string fromTable;
    std::shared_ptr<Expression> whereClause;
    std::vector<std::shared_ptr<Expression>> groupBy;
    std::shared_ptr<Expression> havingClause;
    std::vector<std::shared_ptr<Expression>> orderBy;
    std::shared_ptr<Expression> limitClause;
    std::shared_ptr<Expression> offsetClause;

    std::string getType() const override { return "SelectStatement"; }
};

/**
 * INSERT语句
 */
class InsertStatement : public Statement {
public:
    std::string tableName;
    std::vector<std::string> columns;
    std::vector<std::vector<std::shared_ptr<Expression>>> values;

    std::string getType() const override { return "InsertStatement"; }
};

/**
 * UPDATE语句
 */
class UpdateStatement : public Statement {
public:
    std::string tableName;
    std::vector<std::pair<std::string, std::shared_ptr<Expression>>> setClause;
    std::shared_ptr<Expression> whereClause;

    std::string getType() const override { return "UpdateStatement"; }
};

/**
 * DELETE语句
 */
class DeleteStatement : public Statement {
public:
    std::string tableName;
    std::shared_ptr<Expression> whereClause;

    std::string getType() const override { return "DeleteStatement"; }
};

/**
 * CREATE TABLE语句
 */
class CreateTableStatement : public Statement {
public:
    std::string tableName;
    std::vector<std::shared_ptr<Expression>> columns;

    std::string getType() const override { return "CreateTableStatement"; }
};

} // namespace sealdb

#endif // SEALDB_AST_H
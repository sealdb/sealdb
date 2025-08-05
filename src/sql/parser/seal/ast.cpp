#include "sealdb/ast.h"

namespace sealdb {

// LiteralExpression
void LiteralExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

std::unique_ptr<Expression> LiteralExpression::clone() const {
    return std::make_unique<LiteralExpression>(type_, value_);
}

// IdentifierExpression
void IdentifierExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

std::unique_ptr<Expression> IdentifierExpression::clone() const {
    return std::make_unique<IdentifierExpression>(name_);
}

// BinaryExpression
void BinaryExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

std::unique_ptr<Expression> BinaryExpression::clone() const {
    return std::make_unique<BinaryExpression>(op_, left_->clone(), right_->clone());
}

// FunctionCallExpression
void FunctionCallExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

std::unique_ptr<Expression> FunctionCallExpression::clone() const {
    std::vector<std::unique_ptr<Expression>> cloned_args;
    for (const auto& arg : arguments_) {
        cloned_args.push_back(arg->clone());
    }
    return std::make_unique<FunctionCallExpression>(name_, std::move(cloned_args));
}

// ColumnReference
void ColumnReference::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

std::unique_ptr<Expression> ColumnReference::clone() const {
    return std::make_unique<ColumnReference>(table_name_, column_name_);
}

// SelectStatement
void SelectStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// InsertStatement
void InsertStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// UpdateStatement
void UpdateStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// DeleteStatement
void DeleteStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// CreateTableStatement
void CreateTableStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// DropTableStatement
void DropTableStatement::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

} // namespace sealdb

#include "sealdb/ast.h"

namespace sealdb {

// LiteralExpression
void LiteralExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// IdentifierExpression
void IdentifierExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// BinaryExpression
void BinaryExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// FunctionCallExpression
void FunctionCallExpression::accept(ASTVisitor& visitor) {
    visitor.visit(this);
}

// ColumnReference
void ColumnReference::accept(ASTVisitor& visitor) {
    visitor.visit(this);
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

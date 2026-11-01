use super::dialect::dialect;
use crate::ast::Statement;
use crate::ast::dml::{SelectField, SelectStmt};
use crate::ast::expressions::{
    BinaryOpExpr, ExprNode, FuncCallExpr, FuncCastExpr, UnaryOpExpr, ValueExpr,
};
use crate::opcode::Op;
use sealdb_errno::{ERR_PARSE_ERROR, ERR_UNSUPPORTED, Error, Result};
use sealdb_types::Datum;
use sqlparser::ast::{
    BinaryOperator, DataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    SelectItem, SetExpr, Statement as SqlStatement, UnaryOperator, Value,
};
use sqlparser::parser::Parser;

pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let stmts = Parser::parse_sql(&dialect(), sql).map_err(|e| {
        Error::new(
            ERR_PARSE_ERROR,
            format!("You have an error in your SQL syntax: {e}"),
        )
    })?;
    stmts.iter().map(lower_statement).collect()
}

fn lower_statement(stmt: &SqlStatement) -> Result<Statement> {
    match stmt {
        SqlStatement::Query(query) => match query.body.as_ref() {
            SetExpr::Select(select) => {
                let fields = select
                    .projection
                    .iter()
                    .map(lower_select_item)
                    .collect::<Result<Vec<_>>>()?;
                let from = select.from.first().and_then(|t| match &t.relation {
                    sqlparser::ast::TableFactor::Table { name, .. } => Some(name.to_string()),
                    _ => None,
                });
                Ok(Statement::Select(SelectStmt {
                    fields,
                    from,
                    text: query.to_string(),
                }))
            }
            other => Err(Error::new(
                ERR_UNSUPPORTED,
                format!("query body {:?} is not available in this phase", other),
            )),
        },
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("statement {other} is not available in this phase"),
        )),
    }
}

fn lower_select_item(item: &SelectItem) -> Result<SelectField> {
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: None,
        }),
        SelectItem::ExprWithAlias { expr, alias } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: Some(alias.value.clone()),
        }),
        SelectItem::ExprWithAliases { expr, aliases } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: aliases.first().map(|a| a.value.clone()),
        }),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => Ok(SelectField {
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
        }),
    }
}

fn lower_expr(expr: &Expr) -> Result<ExprNode> {
    match expr {
        Expr::Value(v) => Ok(ExprNode::Value(ValueExpr {
            datum: lower_value(&v.value)?,
        })),
        Expr::BinaryOp { left, op, right } => Ok(ExprNode::Binary(BinaryOpExpr {
            op: lower_binary_op(op)?,
            left: Box::new(lower_expr(left)?),
            right: Box::new(lower_expr(right)?),
        })),
        Expr::UnaryOp { op, expr } => Ok(ExprNode::Unary(UnaryOpExpr {
            op: lower_unary_op(op)?,
            expr: Box::new(lower_expr(expr)?),
        })),
        Expr::Nested(inner) => lower_expr(inner),
        Expr::Cast {
            expr, data_type, ..
        } => Ok(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(lower_expr(expr)?),
            target: data_type_name(data_type),
        })),
        Expr::Function(func) => lower_function(func),
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("null") => {
            Ok(ExprNode::Value(ValueExpr { datum: Datum::Null }))
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("expression {other} is not available in this phase"),
        )),
    }
}

fn lower_function(func: &Function) -> Result<ExprNode> {
    let name = func.name.to_string();
    let args = match &func.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => lower_expr(expr),
                _ => Err(Error::unsupported("function argument form")),
            })
            .collect::<Result<Vec<_>>>()?,
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => {
            return Err(Error::unsupported("function subquery argument"));
        }
    };
    Ok(ExprNode::Func(FuncCallExpr { name, args }))
}

fn lower_value(value: &Value) -> Result<Datum> {
    match value {
        Value::Number(n, _) => {
            if n.contains('.') {
                n.parse::<f64>()
                    .map(Datum::F64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            } else {
                n.parse::<i64>()
                    .map(Datum::I64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            }
        }
        Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => {
            Ok(Datum::Bytes(s.as_bytes().to_vec()))
        }
        Value::Boolean(b) => Ok(Datum::I64(i64::from(*b))),
        Value::Null => Ok(Datum::Null),
        other => Err(Error::unsupported(format!("literal {other}"))),
    }
}

fn lower_binary_op(op: &BinaryOperator) -> Result<Op> {
    Ok(match op {
        BinaryOperator::Plus => Op::Plus,
        BinaryOperator::Minus => Op::Minus,
        BinaryOperator::Multiply => Op::Mul,
        BinaryOperator::Divide => Op::Div,
        BinaryOperator::Modulo => Op::Mod,
        BinaryOperator::Eq => Op::Eq,
        BinaryOperator::NotEq => Op::Ne,
        BinaryOperator::Lt => Op::Lt,
        BinaryOperator::LtEq => Op::Le,
        BinaryOperator::Gt => Op::Gt,
        BinaryOperator::GtEq => Op::Ge,
        BinaryOperator::And => Op::LogicAnd,
        BinaryOperator::Or => Op::LogicOr,
        _ => return Err(Error::unsupported(format!("operator {op}"))),
    })
}

fn lower_unary_op(op: &UnaryOperator) -> Result<Op> {
    Ok(match op {
        UnaryOperator::Plus => Op::Plus,
        UnaryOperator::Minus => Op::UnaryMinus,
        UnaryOperator::Not => Op::Not,
        _ => return Err(Error::unsupported(format!("unary operator {op}"))),
    })
}

fn data_type_name(tp: &DataType) -> String {
    match tp {
        DataType::Int(_) | DataType::Integer(_) | DataType::BigInt(_) | DataType::TinyInt(_) => {
            "SIGNED".to_string()
        }
        DataType::IntUnsigned(_)
        | DataType::IntegerUnsigned(_)
        | DataType::BigIntUnsigned(_)
        | DataType::TinyIntUnsigned(_)
        | DataType::Unsigned
        | DataType::UnsignedInteger => "UNSIGNED".to_string(),
        DataType::Double(_) | DataType::Float(_) | DataType::Real => "DOUBLE".to_string(),
        DataType::Decimal(_) | DataType::Numeric(_) => "DECIMAL".to_string(),
        DataType::Char(_) | DataType::Varchar(_) | DataType::Text | DataType::String(_) => {
            "CHAR".to_string()
        }
        other => other.to_string(),
    }
}

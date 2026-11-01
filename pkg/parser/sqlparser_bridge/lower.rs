use super::dialect::dialect;
use crate::ast::Statement;
use crate::ast::ddl::{
    ColumnDef, CreateDatabaseStmt, CreateTableStmt, DropDatabaseStmt, DropTableStmt, TableName,
};
use crate::ast::dml::{ByItem, InsertStmt, SelectField, SelectStmt};
use crate::ast::expressions::{
    BinaryOpExpr, ExprNode, FuncCallExpr, FuncCastExpr, UnaryOpExpr, ValueExpr,
};
use crate::ast::misc::{ShowStmt, ShowStmtKind, UseStmt};
use crate::opcode::Op;
use sealdb_errno::{ERR_PARSE_ERROR, ERR_UNSUPPORTED, Error, Result};
use sealdb_types::Datum;
use sqlparser::ast::{
    BinaryOperator, DataType, DescribeAlias, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, LimitClause, ObjectName, ObjectType, OrderByKind, OrderBySort, SchemaName,
    SelectItem, SetExpr, Statement as SqlStatement, TableObject, UnaryOperator, Use, Value, Values,
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
                let from = select
                    .from
                    .first()
                    .map(|t| match &t.relation {
                        sqlparser::ast::TableFactor::Table { name, .. } => object_table_name(name),
                        other => Err(Error::unsupported(format!("FROM {other}"))),
                    })
                    .transpose()?;
                let selection = select.selection.as_ref().map(lower_expr).transpose()?;
                let order_by = lower_order_by(query.order_by.as_ref())?;
                let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;
                Ok(Statement::Select(SelectStmt {
                    fields,
                    from,
                    selection,
                    order_by,
                    limit,
                    offset,
                    text: query.to_string(),
                }))
            }
            other => Err(Error::new(
                ERR_UNSUPPORTED,
                format!("query body {:?} is not available in this phase", other),
            )),
        },
        SqlStatement::CreateDatabase {
            db_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: object_name_last(db_name)?,
        })),
        SqlStatement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: schema_name_last(schema_name)?,
        })),
        SqlStatement::CreateTable(create) => {
            if create.temporary || create.query.is_some() || create.like.is_some() {
                return Err(Error::unsupported(
                    "TEMPORARY / LIKE / AS SELECT CREATE TABLE",
                ));
            }
            if !create.constraints.is_empty() {
                return Err(Error::unsupported("CREATE TABLE constraints"));
            }
            let cols = create
                .columns
                .iter()
                .map(|col| {
                    Ok(ColumnDef {
                        name: col.name.value.clone(),
                        type_name: col.data_type.to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if cols.is_empty() {
                return Err(Error::unsupported("CREATE TABLE without columns"));
            }
            Ok(Statement::CreateTable(CreateTableStmt {
                if_not_exists: create.if_not_exists,
                table: object_table_name(&create.name)?,
                cols,
            }))
        }
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            temporary,
            ..
        } => {
            if *temporary {
                return Err(Error::unsupported("DROP TEMPORARY"));
            }
            match object_type {
                ObjectType::Database | ObjectType::Schema => {
                    let name = names
                        .first()
                        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing database name"))?;
                    Ok(Statement::DropDatabase(DropDatabaseStmt {
                        if_exists: *if_exists,
                        name: object_name_last(name)?,
                    }))
                }
                ObjectType::Table => Ok(Statement::DropTable(DropTableStmt {
                    if_exists: *if_exists,
                    tables: names
                        .iter()
                        .map(object_table_name)
                        .collect::<Result<Vec<_>>>()?,
                })),
                other => Err(Error::unsupported(format!("DROP {other}"))),
            }
        }
        SqlStatement::ShowDatabases { .. } | SqlStatement::ShowSchemas { .. } => {
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Databases,
                db_name: None,
                table: None,
            }))
        }
        SqlStatement::ShowTables { show_options, .. } => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::Tables,
            db_name: show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .map(object_name_last)
                .transpose()?,
            table: None,
        })),
        SqlStatement::ShowColumns { show_options, .. } => {
            let table = show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing table name"))?;
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table)?),
            }))
        }
        SqlStatement::ExplainTable {
            describe_alias,
            table_name,
            ..
        } => {
            if !matches!(
                describe_alias,
                DescribeAlias::Desc | DescribeAlias::Describe
            ) {
                return Err(Error::unsupported(format!("EXPLAIN {describe_alias}")));
            }
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table_name)?),
            }))
        }
        SqlStatement::Insert(insert) => {
            let table = match &insert.table {
                TableObject::TableName(name) => object_table_name(name)?,
                other => return Err(Error::unsupported(format!("INSERT target {other}"))),
            };
            let columns = insert
                .columns
                .iter()
                .map(object_name_last)
                .collect::<Result<Vec<_>>>()?;
            let rows = match insert.source.as_deref().map(|q| q.body.as_ref()) {
                Some(SetExpr::Values(values)) => lower_values(values)?,
                Some(other) => {
                    return Err(Error::unsupported(format!("INSERT source {other}")));
                }
                None => return Err(Error::unsupported("INSERT without VALUES")),
            };
            Ok(Statement::Insert(InsertStmt {
                table,
                columns,
                rows,
            }))
        }
        SqlStatement::Use(use_stmt) => {
            let name = match use_stmt {
                Use::Catalog(name)
                | Use::Schema(name)
                | Use::Database(name)
                | Use::Object(name) => object_name_last(name)?,
                other => return Err(Error::unsupported(format!("USE {other}"))),
            };
            Ok(Statement::Use(UseStmt { db_name: name }))
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("statement {other} is not available in this phase"),
        )),
    }
}

fn lower_order_by(order_by: Option<&sqlparser::ast::OrderBy>) -> Result<Vec<ByItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    match &order_by.kind {
        OrderByKind::Expressions(items) => items
            .iter()
            .map(|item| {
                Ok(ByItem {
                    expr: lower_expr(&item.expr)?,
                    desc: matches!(item.options.sort, Some(OrderBySort::Desc)),
                })
            })
            .collect(),
        other => Err(Error::unsupported(format!("ORDER BY {other:?}"))),
    }
}

fn lower_limit(limit: Option<&LimitClause>) -> Result<(Option<u64>, Option<u64>)> {
    match limit {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset { limit, offset, .. }) => {
            let limit = limit.as_ref().map(expr_as_u64).transpose()?;
            let offset = offset.as_ref().map(|o| expr_as_u64(&o.value)).transpose()?;
            Ok((limit, offset))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok((Some(expr_as_u64(limit)?), Some(expr_as_u64(offset)?)))
        }
    }
}

fn expr_as_u64(expr: &Expr) -> Result<u64> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => n
                .parse::<u64>()
                .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string())),
            other => Err(Error::unsupported(format!("limit literal {other}"))),
        },
        other => Err(Error::unsupported(format!("limit expression {other}"))),
    }
}

fn lower_values(values: &Values) -> Result<Vec<Vec<crate::ast::expressions::ExprNode>>> {
    values
        .rows
        .iter()
        .map(|row| row.iter().map(lower_expr).collect())
        .collect()
}

fn object_table_name(name: &ObjectName) -> Result<TableName> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [table] => Ok(TableName::new(None, table.clone())),
        [schema, table] => Ok(TableName::new(Some(schema.clone()), table.clone())),
        _ => Err(Error::unsupported(format!("object name {name}"))),
    }
}

fn object_name_last(name: &ObjectName) -> Result<String> {
    object_name_parts(name)?
        .pop()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "empty object name"))
}

fn schema_name_last(name: &SchemaName) -> Result<String> {
    match name {
        SchemaName::Simple(name) => object_name_last(name),
        other => Err(Error::unsupported(format!("schema name {other}"))),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<String>> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.clone())
                .ok_or_else(|| Error::unsupported(format!("object name part {part}")))
        })
        .collect()
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
        Expr::Identifier(ident) => Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
            schema: None,
            table: None,
            name: ident.value.clone(),
        })),
        Expr::CompoundIdentifier(parts) => {
            let names: Vec<String> = parts.iter().map(|p| p.value.clone()).collect();
            let (schema, table, name) = match names.as_slice() {
                [name] => (None, None, name.clone()),
                [table, name] => (None, Some(table.clone()), name.clone()),
                [schema, table, name] => (Some(schema.clone()), Some(table.clone()), name.clone()),
                _ => return Err(Error::unsupported(format!("column name {expr}"))),
            };
            Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
                schema,
                table,
                name,
            }))
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

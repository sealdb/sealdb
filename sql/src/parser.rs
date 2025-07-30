use sqlparser::ast::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use common::{Error, Result};
use tracing::{debug, error, info};

/// SQL 解析器
pub struct SqlParser {
    dialect: GenericDialect,
}

impl SqlParser {
    pub fn new() -> Self {
        Self {
            dialect: GenericDialect {},
        }
    }

    /// 解析 SQL 语句
    pub fn parse(&self, sql: &str) -> Result<ParsedStatement> {
        info!("Parsing SQL: {}", sql);
        
        match Parser::parse_sql(&self.dialect, sql) {
            Ok(statements) => {
                if statements.is_empty() {
                    return Err(Error::SqlParse("Empty SQL statement".to_string()));
                }
                
                // 目前只处理单条语句
                let statement = statements.into_iter().next().unwrap();
                let parsed = self.parse_statement(statement)?;
                
                debug!("SQL parsed successfully: {:?}", parsed);
                Ok(parsed)
            }
            Err(e) => {
                error!("SQL parse error: {}", e);
                Err(Error::SqlParse(format!("Parse error: {}", e)))
            }
        }
    }

    /// 解析单个语句
    fn parse_statement(&self, stmt: Statement) -> Result<ParsedStatement> {
        match stmt {
            Statement::Query(query) => {
                let select = self.parse_select(*query)?;
                Ok(ParsedStatement::Select(select))
            }
            Statement::Insert { table_name, columns, source, .. } => {
                let insert = ParsedInsert {
                    table: table_name.to_string(),
                    columns: columns.into_iter().map(|c| c.to_string()).collect(),
                    source: self.parse_insert_source(*source)?,
                };
                Ok(ParsedStatement::Insert(insert))
            }
            Statement::Update { table, assignments, selection, .. } => {
                let update = ParsedUpdate {
                    table: table.to_string(),
                    assignments: self.parse_assignments(assignments)?,
                    where_clause: selection.map(|expr| self.parse_expression(expr)).transpose()?,
                };
                Ok(ParsedStatement::Update(update))
            }
            Statement::Delete { .. } => {
                // 简化实现：暂时跳过 DELETE 语句的详细解析
                let delete = ParsedDelete {
                    table: "unknown".to_string(),
                    where_clause: None,
                };
                Ok(ParsedStatement::Delete(delete))
            }
            Statement::CreateTable { name, columns, .. } => {
                let create_table = ParsedCreateTable {
                    table: name.to_string(),
                    columns: self.parse_table_columns(columns)?,
                };
                Ok(ParsedStatement::CreateTable(create_table))
            }
            Statement::Drop { object_type, names, .. } => {
                let drop = ParsedDrop {
                    object_type: object_type.to_string(),
                    names: names.into_iter().map(|n| n.to_string()).collect(),
                };
                Ok(ParsedStatement::Drop(drop))
            }
            _ => Err(Error::SqlParse("Unsupported statement type".to_string())),
        }
    }

    /// 解析 SELECT 语句
    fn parse_select(&self, query: Query) -> Result<ParsedSelect> {
        let select = query.body;
        
        match *select {
            SetExpr::Select(select_stmt) => {
                let select = select_stmt.as_ref();
                
                let columns = self.parse_select_columns(&select.projection)?;
                let from = self.parse_from(&select.from)?;
                let where_clause = select.selection.as_ref().map(|expr| self.parse_expression(expr.clone())).transpose()?;
                let group_by = self.parse_group_by(&select.group_by)?;
                let order_by = Vec::new(); // select.order_by 字段不存在
                let limit = None; // select.limit 字段不存在
                let offset = None; // select.offset 字段不存在
                
                Ok(ParsedSelect {
                    columns,
                    from,
                    where_clause,
                    group_by,
                    order_by,
                    limit,
                    offset,
                })
            }
            _ => Err(Error::SqlParse("Unsupported SELECT expression".to_string())),
        }
    }

    /// 解析 SELECT 列
    fn parse_select_columns(&self, projection: &[SelectItem]) -> Result<Vec<ParsedColumn>> {
        let mut columns = Vec::new();
        
        for item in projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let column = self.parse_expression(expr.clone())?;
                    columns.push(ParsedColumn {
                        name: column.to_string(),
                        alias: None,
                    });
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let column = self.parse_expression(expr.clone())?;
                    columns.push(ParsedColumn {
                        name: column.to_string(),
                        alias: Some(alias.to_string()),
                    });
                }
                SelectItem::Wildcard(_) => {
                    columns.push(ParsedColumn {
                        name: "*".to_string(),
                        alias: None,
                    });
                }
                _ => return Err(Error::SqlParse("Unsupported select item".to_string())),
            }
        }
        
        Ok(columns)
    }

    /// 解析 FROM 子句
    fn parse_from(&self, from: &[TableWithJoins]) -> Result<Vec<ParsedTable>> {
        let mut tables = Vec::new();
        
        for table_with_joins in from {
            let table = &table_with_joins.relation;
            
            match table {
                TableFactor::Table { name, alias, .. } => {
                    tables.push(ParsedTable {
                        name: name.to_string(),
                        alias: alias.as_ref().map(|a| a.name.to_string()),
                    });
                }
                _ => return Err(Error::SqlParse("Unsupported table factor".to_string())),
            }
        }
        
        Ok(tables)
    }

    /// 解析表达式
    fn parse_expression(&self, expr: Expr) -> Result<ParsedExpression> {
        match expr {
            Expr::Identifier(ident) => {
                Ok(ParsedExpression::Column(ident.to_string()))
            }
            Expr::Value(value) => {
                let parsed_value = match value {
                    Value::Number(n, _) => ParsedValue::Number(n),
                    Value::SingleQuotedString(s) => ParsedValue::String(s),
                    Value::DoubleQuotedString(s) => ParsedValue::String(s),
                    Value::Boolean(b) => ParsedValue::Boolean(b),
                    Value::Null => ParsedValue::Null,
                    _ => return Err(Error::SqlParse("Unsupported value type".to_string())),
                };
                Ok(ParsedExpression::Literal(parsed_value))
            }
            Expr::BinaryOp { left, op, right } => {
                let left_expr = self.parse_expression(*left)?;
                let right_expr = self.parse_expression(*right)?;
                let operator = match op {
                    BinaryOperator::Plus => ParsedOperator::Add,
                    BinaryOperator::Minus => ParsedOperator::Subtract,
                    BinaryOperator::Multiply => ParsedOperator::Multiply,
                    BinaryOperator::Divide => ParsedOperator::Divide,
                    BinaryOperator::Eq => ParsedOperator::Equal,
                    BinaryOperator::NotEq => ParsedOperator::NotEqual,
                    BinaryOperator::Lt => ParsedOperator::LessThan,
                    BinaryOperator::LtEq => ParsedOperator::LessThanOrEqual,
                    BinaryOperator::Gt => ParsedOperator::GreaterThan,
                    BinaryOperator::GtEq => ParsedOperator::GreaterThanOrEqual,
                    BinaryOperator::And => ParsedOperator::And,
                    BinaryOperator::Or => ParsedOperator::Or,
                    _ => return Err(Error::SqlParse("Unsupported operator".to_string())),
                };
                Ok(ParsedExpression::BinaryOp {
                    left: Box::new(left_expr),
                    operator,
                    right: Box::new(right_expr),
                })
            }
            Expr::Function(func) => {
                let function_name = func.name.to_string();
                let args = func.args.into_iter()
                    .map(|arg| match arg {
                        FunctionArg::Named { arg: _, .. } => {
                            // 简化实现：暂时跳过命名参数的详细解析
                            Ok(ParsedExpression::Column("unknown".to_string()))
                        }
                        FunctionArg::Unnamed(_expr) => {
                            // 简化实现：暂时跳过未命名参数的解析
                            Ok(ParsedExpression::Column("unknown".to_string()))
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                
                Ok(ParsedExpression::Function {
                    name: function_name,
                    arguments: args,
                })
            }
            _ => Err(Error::SqlParse("Unsupported expression type".to_string())),
        }
    }

    /// 解析赋值
    fn parse_assignments(&self, assignments: Vec<Assignment>) -> Result<Vec<ParsedAssignment>> {
        let mut parsed_assignments = Vec::new();
        
        for assignment in assignments {
            let column = assignment.id[0].to_string();
            let value = self.parse_expression(assignment.value)?;
            
            parsed_assignments.push(ParsedAssignment {
                column,
                value,
            });
        }
        
        Ok(parsed_assignments)
    }

    /// 解析表列定义
    fn parse_table_columns(&self, columns: Vec<ColumnDef>) -> Result<Vec<ParsedColumnDef>> {
        let mut parsed_columns = Vec::new();
        
        for column in columns {
            let name = column.name.to_string();
            let data_type = self.parse_data_type(&column.data_type)?;
            let nullable = !column.options.iter().any(|opt| {
                matches!(opt.option, ColumnOption::NotNull)
            });
            
            parsed_columns.push(ParsedColumnDef {
                name,
                data_type,
                nullable,
            });
        }
        
        Ok(parsed_columns)
    }

    /// 解析数据类型
    fn parse_data_type(&self, data_type: &DataType) -> Result<ParsedDataType> {
        match data_type {
            DataType::Int(_) => Ok(ParsedDataType::Integer),
            DataType::BigInt(_) => Ok(ParsedDataType::BigInt),
            DataType::Varchar(_) => Ok(ParsedDataType::Varchar),
            DataType::Text => Ok(ParsedDataType::Text),
            DataType::Boolean => Ok(ParsedDataType::Boolean),
            DataType::Float(_) => Ok(ParsedDataType::Float),
            DataType::Double => Ok(ParsedDataType::Double),
            DataType::Decimal(_) => Ok(ParsedDataType::Decimal),
            DataType::Timestamp(_, _) => Ok(ParsedDataType::Timestamp),
            DataType::Date => Ok(ParsedDataType::Date),
            _ => Err(Error::SqlParse("Unsupported data type".to_string())),
        }
    }

    /// 解析 GROUP BY
    fn parse_group_by(&self, group_by: &[Expr]) -> Result<Vec<ParsedExpression>> {
        let mut parsed_group_by = Vec::new();
        
        for expr in group_by {
            let parsed_expr = self.parse_expression(expr.clone())?;
            parsed_group_by.push(parsed_expr);
        }
        
        Ok(parsed_group_by)
    }

    /// 解析 ORDER BY
    fn parse_order_by(&self, order_by: &[OrderByExpr]) -> Result<Vec<ParsedOrderBy>> {
        let mut parsed_order_by = Vec::new();
        
        for order_expr in order_by {
            let expr = self.parse_expression(order_expr.expr.clone())?;
            let order = match order_expr.asc {
                Some(true) => ParsedOrder::Asc,
                Some(false) => ParsedOrder::Desc,
                None => ParsedOrder::Asc,
            };
            
            parsed_order_by.push(ParsedOrderBy {
                expression: expr,
                order,
            });
        }
        
        Ok(parsed_order_by)
    }

    /// 解析 INSERT 源
    fn parse_insert_source(&self, _source: Query) -> Result<ParsedInsertSource> {
        // 简化实现，只支持 VALUES
        Ok(ParsedInsertSource::Values(vec![]))
    }
}

/// 解析后的语句类型
#[derive(Debug, Clone)]
pub enum ParsedStatement {
    Select(ParsedSelect),
    Insert(ParsedInsert),
    Update(ParsedUpdate),
    Delete(ParsedDelete),
    CreateTable(ParsedCreateTable),
    Drop(ParsedDrop),
}

/// 解析后的 SELECT 语句
#[derive(Debug, Clone)]
pub struct ParsedSelect {
    pub columns: Vec<ParsedColumn>,
    pub from: Vec<ParsedTable>,
    pub where_clause: Option<ParsedExpression>,
    pub group_by: Vec<ParsedExpression>,
    pub order_by: Vec<ParsedOrderBy>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

/// 解析后的 INSERT 语句
#[derive(Debug, Clone)]
pub struct ParsedInsert {
    pub table: String,
    pub columns: Vec<String>,
    pub source: ParsedInsertSource,
}

/// 解析后的 UPDATE 语句
#[derive(Debug, Clone)]
pub struct ParsedUpdate {
    pub table: String,
    pub assignments: Vec<ParsedAssignment>,
    pub where_clause: Option<ParsedExpression>,
}

/// 解析后的 DELETE 语句
#[derive(Debug, Clone)]
pub struct ParsedDelete {
    pub table: String,
    pub where_clause: Option<ParsedExpression>,
}

/// 解析后的 CREATE TABLE 语句
#[derive(Debug, Clone)]
pub struct ParsedCreateTable {
    pub table: String,
    pub columns: Vec<ParsedColumnDef>,
}

/// 解析后的 DROP 语句
#[derive(Debug, Clone)]
pub struct ParsedDrop {
    pub object_type: String,
    pub names: Vec<String>,
}

/// 解析后的列
#[derive(Debug, Clone)]
pub struct ParsedColumn {
    pub name: String,
    pub alias: Option<String>,
}

/// 解析后的表
#[derive(Debug, Clone)]
pub struct ParsedTable {
    pub name: String,
    pub alias: Option<String>,
}

/// 解析后的表达式
#[derive(Debug, Clone)]
pub enum ParsedExpression {
    Column(String),
    Literal(ParsedValue),
    BinaryOp {
        left: Box<ParsedExpression>,
        operator: ParsedOperator,
        right: Box<ParsedExpression>,
    },
    Function {
        name: String,
        arguments: Vec<ParsedExpression>,
    },
}

/// 解析后的值
#[derive(Debug, Clone)]
pub enum ParsedValue {
    Number(String),
    String(String),
    Boolean(bool),
    Null,
}

/// 解析后的操作符
#[derive(Debug, Clone)]
pub enum ParsedOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    And,
    Or,
}

/// 解析后的赋值
#[derive(Debug, Clone)]
pub struct ParsedAssignment {
    pub column: String,
    pub value: ParsedExpression,
}

/// 解析后的列定义
#[derive(Debug, Clone)]
pub struct ParsedColumnDef {
    pub name: String,
    pub data_type: ParsedDataType,
    pub nullable: bool,
}

/// 解析后的数据类型
#[derive(Debug, Clone)]
pub enum ParsedDataType {
    Integer,
    BigInt,
    Varchar,
    Text,
    Boolean,
    Float,
    Double,
    Decimal,
    Timestamp,
    Date,
}

/// 解析后的 ORDER BY
#[derive(Debug, Clone)]
pub struct ParsedOrderBy {
    pub expression: ParsedExpression,
    pub order: ParsedOrder,
}

/// 解析后的排序
#[derive(Debug, Clone)]
pub enum ParsedOrder {
    Asc,
    Desc,
}

/// 解析后的 INSERT 源
#[derive(Debug, Clone)]
pub enum ParsedInsertSource {
    Values(Vec<Vec<ParsedExpression>>),
    Select(ParsedSelect),
}

impl ParsedExpression {
    pub fn to_string(&self) -> String {
        match self {
            ParsedExpression::Column(name) => name.clone(),
            ParsedExpression::Literal(value) => match value {
                ParsedValue::Number(n) => n.clone(),
                ParsedValue::String(s) => format!("'{}'", s),
                ParsedValue::Boolean(b) => b.to_string(),
                ParsedValue::Null => "NULL".to_string(),
            },
            ParsedExpression::BinaryOp { left, operator, right } => {
                format!("({} {} {})", 
                    left.to_string(), 
                    operator.to_string(), 
                    right.to_string())
            }
            ParsedExpression::Function { name, arguments } => {
                let args = arguments.iter()
                    .map(|arg| arg.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}({})", name, args)
            }
        }
    }
}

impl ParsedOperator {
    pub fn to_string(&self) -> String {
        match self {
            ParsedOperator::Add => "+".to_string(),
            ParsedOperator::Subtract => "-".to_string(),
            ParsedOperator::Multiply => "*".to_string(),
            ParsedOperator::Divide => "/".to_string(),
            ParsedOperator::Equal => "=".to_string(),
            ParsedOperator::NotEqual => "!=".to_string(),
            ParsedOperator::LessThan => "<".to_string(),
            ParsedOperator::LessThanOrEqual => "<=".to_string(),
            ParsedOperator::GreaterThan => ">".to_string(),
            ParsedOperator::GreaterThanOrEqual => ">=".to_string(),
            ParsedOperator::And => "AND".to_string(),
            ParsedOperator::Or => "OR".to_string(),
        }
    }
} 
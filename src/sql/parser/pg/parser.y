%{
#include "sealdb/parser.h"
#include "sealdb/ast.h"
#include "sealdb/lexer.h"
#include <memory>
#include <vector>
#include <string>

using namespace sealdb;

// 声明外部函数
int yylex();
void yyerror(const char* s);

// 当前解析的SQL语句
std::string current_sql;

// 语法分析器状态
class ParserState {
public:
    std::vector<std::unique_ptr<Statement>> statements;
    std::string error_message;
    int error_line;
    int error_column;

    void set_error(const std::string& msg, int line, int col) {
        error_message = msg;
        error_line = line;
        error_column = col;
    }

    void clear_error() {
        error_message.clear();
        error_line = 0;
        error_column = 0;
    }
};

static ParserState parser_state;

%}

/* 声明部分 */
%union {
    int token;
    std::string* string;
    std::unique_ptr<Expression>* expression;
    std::unique_ptr<Statement>* statement;
    std::vector<std::unique_ptr<Expression>>* expression_list;
    std::vector<std::string>* string_list;
    std::vector<std::pair<std::string, std::unique_ptr<Expression>>>* set_clause;
    std::vector<CreateTableStatement::ColumnDefinition>* column_definitions;
}

/* 终结符 */
%token <token> SELECT INSERT UPDATE DELETE CREATE DROP ALTER TABLE INDEX VIEW
%token <token> FROM WHERE GROUP BY ORDER HAVING LIMIT OFFSET
%token <token> JOIN LEFT RIGHT INNER OUTER ON AS
%token <token> DISTINCT UNIQUE PRIMARY KEY FOREIGN REFERENCES CONSTRAINT
%token <token> CASCADE RESTRICT SET NULL_VALUE DEFAULT CHECK
%token <token> IN EXISTS BETWEEN LIKE IS
%token <token> COUNT SUM AVG MAX MIN
%token <token> AND OR NOT
%token <token> INT BIGINT SMALLINT TINYINT FLOAT DOUBLE DECIMAL NUMERIC
%token <token> CHAR VARCHAR TEXT BLOB DATE TIME DATETIME TIMESTAMP BOOLEAN BOOL
%token <token> PLUS MINUS MULTIPLY DIVIDE MOD
%token <token> EQUAL NOT_EQUAL LESS LESS_EQUAL GREATER GREATER_EQUAL ASSIGN
%token <token> DOT COMMA SEMICOLON LPAREN RPAREN LBRACKET RBRACKET LBRACE RBRACE
%token <string> IDENTIFIER STRING_LITERAL NUMBER_LITERAL
%token END_OF_FILE

/* 非终结符类型 */
%type <statement> statement
%type <statement> select_statement insert_statement update_statement delete_statement
%type <statement> create_table_statement drop_table_statement
%type <expression> expression condition arithmetic_expression term factor primary
%type <expression_list> select_list expression_list
%type <string_list> table_list column_list
%type <set_clause> set_clause
%type <column_definitions> column_definitions column_definition
%type <string> data_type

/* 优先级和结合性 */
%left PLUS MINUS
%left MULTIPLY DIVIDE MOD
%right UMINUS

%%

/* 语法规则 */

/* 主规则 */
program
    : statement_list
    ;

statement_list
    : statement
    | statement_list SEMICOLON statement
    ;

statement
    : select_statement    { $$ = $1; }
    | insert_statement    { $$ = $1; }
    | update_statement    { $$ = $1; }
    | delete_statement    { $$ = $1; }
    | create_table_statement { $$ = $1; }
    | drop_table_statement { $$ = $1; }
    ;

/* SELECT语句 */
select_statement
    : SELECT select_list FROM table_list
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition GROUP BY expression_list
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          stmt->set_group_by(*$9);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition GROUP BY expression_list HAVING condition
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          stmt->set_group_by(*$9);
          stmt->set_having_clause($11);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition ORDER BY expression_list
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          stmt->set_order_by(*$9);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition GROUP BY expression_list ORDER BY expression_list
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          stmt->set_group_by(*$9);
          stmt->set_order_by(*$12);
          $$ = std::move(stmt);
      }
    | SELECT select_list FROM table_list WHERE condition GROUP BY expression_list HAVING condition ORDER BY expression_list
      {
          auto stmt = std::make_unique<SelectStatement>();
          stmt->set_select_list(*$2);
          stmt->set_from_tables(*$4);
          stmt->set_where_clause($6);
          stmt->set_group_by(*$9);
          stmt->set_having_clause($11);
          stmt->set_order_by(*$14);
          $$ = std::move(stmt);
      }
    ;

/* INSERT语句 */
insert_statement
    : INSERT INTO IDENTIFIER LPAREN column_list RPAREN VALUES LPAREN expression_list RPAREN
      {
          auto stmt = std::make_unique<InsertStatement>();
          stmt->set_table_name(*$3);
          stmt->set_columns(*$5);
          // 简化实现，只支持单行插入
          std::vector<std::vector<std::unique_ptr<Expression>>> values;
          values.push_back(*$9);
          stmt->set_values(std::move(values));
          $$ = std::move(stmt);
      }
    ;

/* UPDATE语句 */
update_statement
    : UPDATE IDENTIFIER SET set_clause
      {
          auto stmt = std::make_unique<UpdateStatement>();
          stmt->set_table_name(*$2);
          stmt->set_set_clause(*$4);
          $$ = std::move(stmt);
      }
    | UPDATE IDENTIFIER SET set_clause WHERE condition
      {
          auto stmt = std::make_unique<UpdateStatement>();
          stmt->set_table_name(*$2);
          stmt->set_set_clause(*$4);
          stmt->set_where_clause($6);
          $$ = std::move(stmt);
      }
    ;

/* DELETE语句 */
delete_statement
    : DELETE FROM IDENTIFIER
      {
          auto stmt = std::make_unique<DeleteStatement>();
          stmt->set_table_name(*$3);
          $$ = std::move(stmt);
      }
    | DELETE FROM IDENTIFIER WHERE condition
      {
          auto stmt = std::make_unique<DeleteStatement>();
          stmt->set_table_name(*$3);
          stmt->set_where_clause($5);
          $$ = std::move(stmt);
      }
    ;

/* CREATE TABLE语句 */
create_table_statement
    : CREATE TABLE IDENTIFIER LPAREN column_definitions RPAREN
      {
          auto stmt = std::make_unique<CreateTableStatement>();
          stmt->set_table_name(*$3);
          stmt->set_columns(*$5);
          $$ = std::move(stmt);
      }
    ;

/* DROP TABLE语句 */
drop_table_statement
    : DROP TABLE IDENTIFIER
      {
          auto stmt = std::make_unique<DropTableStatement>();
          stmt->set_table_name(*$3);
          $$ = std::move(stmt);
      }
    ;

/* 表达式 */
expression
    : condition
    | arithmetic_expression
    ;

condition
    : primary
    | condition AND condition
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::AND,
              std::move($1),
              std::move($3)
          );
      }
    | condition OR condition
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::OR,
              std::move($1),
              std::move($3)
          );
      }
    | NOT condition
      {
          // 简化实现，NOT作为一元操作符
          $$ = std::move($2);
      }
    | primary EQUAL primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::EQUAL,
              std::move($1),
              std::move($3)
          );
      }
    | primary NOT_EQUAL primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::NOT_EQUAL,
              std::move($1),
              std::move($3)
          );
      }
    | primary LESS primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::LESS,
              std::move($1),
              std::move($3)
          );
      }
    | primary LESS_EQUAL primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::LESS_EQUAL,
              std::move($1),
              std::move($3)
          );
      }
    | primary GREATER primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::GREATER,
              std::move($1),
              std::move($3)
          );
      }
    | primary GREATER_EQUAL primary
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::GREATER_EQUAL,
              std::move($1),
              std::move($3)
          );
      }
    ;

arithmetic_expression
    : term
    | arithmetic_expression PLUS term
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::ADD,
              std::move($1),
              std::move($3)
          );
      }
    | arithmetic_expression MINUS term
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::SUBTRACT,
              std::move($1),
              std::move($3)
          );
      }
    ;

term
    : factor
    | term MULTIPLY factor
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::MULTIPLY,
              std::move($1),
              std::move($3)
          );
      }
    | term DIVIDE factor
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::DIVIDE,
              std::move($1),
              std::move($3)
          );
      }
    | term MOD factor
      {
          $$ = std::make_unique<BinaryExpression>(
              BinaryExpression::Operator::MOD,
              std::move($1),
              std::move($3)
          );
      }
    ;

factor
    : primary
    | MINUS factor %prec UMINUS
      {
          // 简化实现，一元负号
          $$ = std::move($2);
      }
    ;

primary
    : IDENTIFIER
      {
          $$ = std::make_unique<IdentifierExpression>(*$1);
      }
    | NUMBER_LITERAL
      {
          $$ = std::make_unique<LiteralExpression>(
              LiteralExpression::Type::INTEGER,
              *$1
          );
      }
    | STRING_LITERAL
      {
          $$ = std::make_unique<LiteralExpression>(
              LiteralExpression::Type::STRING,
              *$1
          );
      }
    | LPAREN expression RPAREN
      {
          $$ = std::move($2);
      }
    ;

/* 列表 */
select_list
    : expression
      {
          $$ = new std::vector<std::unique_ptr<Expression>>();
          $$->push_back(std::move($1));
      }
    | select_list COMMA expression
      {
          $1->push_back(std::move($3));
          $$ = $1;
      }
    ;

expression_list
    : expression
      {
          $$ = new std::vector<std::unique_ptr<Expression>>();
          $$->push_back(std::move($1));
      }
    | expression_list COMMA expression
      {
          $1->push_back(std::move($3));
          $$ = $1;
      }
    ;

table_list
    : IDENTIFIER
      {
          $$ = new std::vector<std::string>();
          $$->push_back(*$1);
      }
    | table_list COMMA IDENTIFIER
      {
          $1->push_back(*$3);
          $$ = $1;
      }
    ;

column_list
    : IDENTIFIER
      {
          $$ = new std::vector<std::string>();
          $$->push_back(*$1);
      }
    | column_list COMMA IDENTIFIER
      {
          $1->push_back(*$3);
          $$ = $1;
      }
    ;

set_clause
    : IDENTIFIER ASSIGN expression
      {
          $$ = new std::vector<std::pair<std::string, std::unique_ptr<Expression>>>();
          $$->emplace_back(*$1, std::move($3));
      }
    | set_clause COMMA IDENTIFIER ASSIGN expression
      {
          $1->emplace_back(*$3, std::move($5));
          $$ = $1;
      }
    ;

column_definitions
    : column_definition
      {
          $$ = new std::vector<CreateTableStatement::ColumnDefinition>();
          $$->push_back(*$1);
      }
    | column_definitions COMMA column_definition
      {
          $1->push_back(*$3);
          $$ = $1;
      }
    ;

column_definition
    : IDENTIFIER data_type
      {
          $$ = new CreateTableStatement::ColumnDefinition(*$1, *$2);
      }
    ;

data_type
    : INT     { $$ = new std::string("INT"); }
    | BIGINT  { $$ = new std::string("BIGINT"); }
    | SMALLINT { $$ = new std::string("SMALLINT"); }
    | TINYINT { $$ = new std::string("TINYINT"); }
    | FLOAT   { $$ = new std::string("FLOAT"); }
    | DOUBLE  { $$ = new std::string("DOUBLE"); }
    | DECIMAL { $$ = new std::string("DECIMAL"); }
    | NUMERIC { $$ = new std::string("NUMERIC"); }
    | CHAR    { $$ = new std::string("CHAR"); }
    | VARCHAR { $$ = new std::string("VARCHAR"); }
    | TEXT    { $$ = new std::string("TEXT"); }
    | BLOB    { $$ = new std::string("BLOB"); }
    | DATE    { $$ = new std::string("DATE"); }
    | TIME    { $$ = new std::string("TIME"); }
    | DATETIME { $$ = new std::string("DATETIME"); }
    | TIMESTAMP { $$ = new std::string("TIMESTAMP"); }
    | BOOLEAN { $$ = new std::string("BOOLEAN"); }
    | BOOL    { $$ = new std::string("BOOL"); }
    ;

%%

/* 用户代码段 */

void yyerror(const char* s) {
    parser_state.set_error(s, yylineno, yycolumn);
}

std::unique_ptr<Statement> sealdb::Parser::parse() {
    parser_state.clear_error();
    current_sql = sql_;

    // 设置输入
    YY_BUFFER_STATE buffer = yy_scan_string(current_sql.c_str());

    // 解析
    int result = yyparse();

    // 清理
    yy_delete_buffer(buffer);

    if (result == 0 && !parser_state.statements.empty()) {
        return std::move(parser_state.statements[0]);
    }

    return nullptr;
}

const std::string& sealdb::Parser::get_error_message() const {
    return parser_state.error_message;
}

int sealdb::Parser::get_error_line() const {
    return parser_state.error_line;
}

int sealdb::Parser::get_error_column() const {
    return parser_state.error_column;
}
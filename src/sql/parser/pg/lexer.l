%{
#include "sealdb/parser.h"
#include "sealdb/lexer.h"
#include <string>
#include <cstring>

using namespace sealdb;

// 当前行号和列号
int yylineno = 1;
int yycolumn = 1;

// 当前token的位置信息
int token_start_line = 1;
int token_start_column = 1;

// 设置token位置信息
#define SET_TOKEN_POS() do { \
    token_start_line = yylineno; \
    token_start_column = yycolumn; \
} while(0)

// 更新列号
#define UPDATE_COLUMN() do { \
    yycolumn += yyleng; \
} while(0)

// 处理换行符
#define HANDLE_NEWLINE() do { \
    yylineno++; \
    yycolumn = 1; \
} while(0)

%}

%option noyywrap
%option yylineno
%option case-insensitive

/* 字符类 */
DIGIT       [0-9]
LETTER      [a-zA-Z_]
WHITESPACE  [ \t\r]
NEWLINE     \n
COMMENT     --.*
BLOCK_COMMENT \/\*([^*]|\*+[^*/])*\*+\/

/* 标识符和关键字 */
IDENTIFIER  {LETTER}({LETTER}|{DIGIT})*
NUMBER      {DIGIT}+(\.{DIGIT}+)?([eE][+-]?{DIGIT}+)?
STRING      '([^']|'')*'
HEX_NUMBER  0[xX][0-9a-fA-F]+
BINARY_NUMBER 0[bB][01]+

/* 操作符 */
PLUS        \+
MINUS       \-
MULTIPLY    \*
DIVIDE      \/
MOD         %
ASSIGN      =
EQUAL       =
NOT_EQUAL   !=|<>|!=
LESS        <
LESS_EQUAL  <=
GREATER     >
GREATER_EQUAL >=
AND         &&|AND
OR          \|\||OR
NOT         !|NOT
DOT         \.
COMMA       ,
SEMICOLON   \;
LPAREN      \(
RPAREN      \)
LBRACKET    \[
RBRACKET    \]
LBRACE      \{
RBRACE      \}

/* SQL关键字 */
SELECT      SELECT
INSERT      INSERT
UPDATE      UPDATE
DELETE      DELETE
CREATE      CREATE
DROP        DROP
ALTER       ALTER
TABLE       TABLE
INDEX       INDEX
VIEW        VIEW
FROM        FROM
WHERE       WHERE
GROUP       GROUP
BY          BY
ORDER       ORDER
HAVING      HAVING
LIMIT       LIMIT
OFFSET      OFFSET
JOIN        JOIN
LEFT        LEFT
RIGHT       RIGHT
INNER       INNER
OUTER       OUTER
ON          ON
AS          AS
DISTINCT    DISTINCT
UNIQUE      UNIQUE
PRIMARY     PRIMARY
KEY         KEY
FOREIGN     FOREIGN
REFERENCES  REFERENCES
CONSTRAINT  CONSTRAINT
CASCADE     CASCADE
RESTRICT    RESTRICT
SET         SET
NULL_VALUE  NULL
DEFAULT     DEFAULT
CHECK       CHECK
IN          IN
EXISTS      EXISTS
BETWEEN     BETWEEN
LIKE        LIKE
IS          IS
COUNT       COUNT
SUM         SUM
AVG         AVG
MAX         MAX
MIN         MIN

/* 数据类型 */
INT         INT(EGER)?
BIGINT      BIGINT
SMALLINT    SMALLINT
TINYINT     TINYINT
FLOAT       FLOAT
DOUBLE      DOUBLE
DECIMAL     DECIMAL
NUMERIC     NUMERIC
CHAR        CHAR
VARCHAR     VARCHAR
TEXT        TEXT
BLOB        BLOB
DATE        DATE
TIME        TIME
DATETIME    DATETIME
TIMESTAMP   TIMESTAMP
BOOLEAN     BOOLEAN
BOOL        BOOL

%%

{WHITESPACE} {
    UPDATE_COLUMN();
    /* 忽略空白字符 */
}

{NEWLINE} {
    HANDLE_NEWLINE();
    /* 忽略换行符 */
}

{COMMENT} {
    UPDATE_COLUMN();
    /* 忽略单行注释 */
}

{BLOCK_COMMENT} {
    UPDATE_COLUMN();
    /* 忽略块注释 */
}

{SELECT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::SELECT;
}

{INSERT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::INSERT;
}

{UPDATE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::UPDATE;
}

{DELETE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DELETE;
}

{CREATE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::CREATE;
}

{DROP} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DROP;
}

{ALTER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::ALTER;
}

{TABLE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::TABLE;
}

{INDEX} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::INDEX;
}

{VIEW} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::VIEW;
}

{FROM} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::FROM;
}

{WHERE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::WHERE;
}

{GROUP} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::GROUP;
}

{BY} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BY;
}

{ORDER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::ORDER;
}

{HAVING} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::HAVING;
}

{LIMIT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LIMIT;
}

{OFFSET} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::OFFSET;
}

{JOIN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::JOIN;
}

{LEFT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LEFT;
}

{RIGHT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::RIGHT;
}

{INNER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::INNER;
}

{OUTER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::OUTER;
}

{ON} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::ON;
}

{AS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::AS;
}

{DISTINCT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DISTINCT;
}

{UNIQUE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::UNIQUE;
}

{PRIMARY} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::PRIMARY;
}

{KEY} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::KEY;
}

{FOREIGN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::FOREIGN;
}

{REFERENCES} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::REFERENCES;
}

{CONSTRAINT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::CONSTRAINT;
}

{CASCADE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::CASCADE;
}

{RESTRICT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::RESTRICT;
}

{SET} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::SET;
}

{NULL_VALUE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NULL_VALUE;
}

{DEFAULT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DEFAULT;
}

{CHECK} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::CHECK;
}

{IN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::IN;
}

{EXISTS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::EXISTS;
}

{BETWEEN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BETWEEN;
}

{LIKE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LIKE;
}

{IS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::IS;
}

{COUNT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::COUNT;
}

{SUM} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::SUM;
}

{AVG} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::AVG;
}

{MAX} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::MAX;
}

{MIN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::MIN;
}

{AND} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::AND;
}

{OR} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::OR;
}

{NOT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NOT;
}

{INT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::INT;
}

{BIGINT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BIGINT;
}

{SMALLINT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::SMALLINT;
}

{TINYINT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::TINYINT;
}

{FLOAT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::FLOAT;
}

{DOUBLE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DOUBLE;
}

{DECIMAL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DECIMAL;
}

{NUMERIC} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NUMERIC;
}

{CHAR} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::CHAR;
}

{VARCHAR} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::VARCHAR;
}

{TEXT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::TEXT;
}

{BLOB} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BLOB;
}

{DATE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DATE;
}

{TIME} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::TIME;
}

{DATETIME} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DATETIME;
}

{TIMESTAMP} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::TIMESTAMP;
}

{BOOLEAN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BOOLEAN;
}

{BOOL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::BOOL;
}

{PLUS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::PLUS;
}

{MINUS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::MINUS;
}

{MULTIPLY} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::MULTIPLY;
}

{DIVIDE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DIVIDE;
}

{MOD} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::MOD;
}

{EQUAL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::EQUAL;
}

{NOT_EQUAL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NOT_EQUAL;
}

{LESS} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LESS;
}

{LESS_EQUAL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LESS_EQUAL;
}

{GREATER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::GREATER;
}

{GREATER_EQUAL} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::GREATER_EQUAL;
}

{ASSIGN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::ASSIGN;
}

{DOT} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::DOT;
}

{COMMA} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::COMMA;
}

{SEMICOLON} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::SEMICOLON;
}

{LPAREN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LPAREN;
}

{RPAREN} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::RPAREN;
}

{LBRACKET} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LBRACKET;
}

{RBRACKET} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::RBRACKET;
}

{LBRACE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::LBRACE;
}

{RBRACE} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::RBRACE;
}

{IDENTIFIER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::IDENTIFIER;
}

{NUMBER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NUMBER_LITERAL;
}

{STRING} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::STRING_LITERAL;
}

{HEX_NUMBER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NUMBER_LITERAL;
}

{BINARY_NUMBER} {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::NUMBER_LITERAL;
}

. {
    SET_TOKEN_POS();
    UPDATE_COLUMN();
    return sealdb::TokenType::ERROR;
}

%%

/* 用户代码段 */
void sealdb::Lexer::reset() {
    yylineno = 1;
    yycolumn = 1;
    token_start_line = 1;
    token_start_column = 1;
}

sealdb::Token sealdb::Lexer::next_token() {
    int token_type = yylex();
    return sealdb::Token(
        static_cast<sealdb::TokenType>(token_type),
        yytext,
        token_start_line,
        token_start_column
    );
}
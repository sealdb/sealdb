grammar SQL;

// 解析器规则
parse
    : sqlStatement EOF
    ;

sqlStatement
    : selectStatement
    | insertStatement
    | updateStatement
    | deleteStatement
    | createTableStatement
    | dropTableStatement
    | createIndexStatement
    | dropIndexStatement
    | beginTransactionStatement
    | commitStatement
    | rollbackStatement
    ;

// SELECT 语句
selectStatement
    : SELECT selectList
      FROM tableReferenceList
      (WHERE whereClause)?
      (GROUP BY groupByClause)?
      (HAVING havingClause)?
      (ORDER BY orderByClause)?
      (LIMIT limitClause)?
    ;

selectList
    : selectItem (',' selectItem)*
    ;

selectItem
    : '*'                                    # selectAll
    | columnReference                        # selectColumn
    | expression AS? identifier?            # selectExpression
    ;

tableReferenceList
    : tableReference (',' tableReference)*
    ;

tableReference
    : tableName (AS? identifier)?           # tableReferenceSimple
    | '(' selectStatement ')' AS? identifier # tableReferenceSubquery
    ;

whereClause
    : expression
    ;

groupByClause
    : expression (',' expression)*
    ;

havingClause
    : expression
    ;

orderByClause
    : orderByItem (',' orderByItem)*
    ;

orderByItem
    : expression (ASC | DESC)?
    ;

limitClause
    : INTEGER_LITERAL (',' INTEGER_LITERAL)?
    ;

// INSERT 语句
insertStatement
    : INSERT INTO tableName
      '(' columnList ')'?
      VALUES '(' valueList ')' (',' '(' valueList ')')*
    ;

columnList
    : identifier (',' identifier)*
    ;

valueList
    : expression (',' expression)*
    ;

// UPDATE 语句
updateStatement
    : UPDATE tableName
      SET setClause (',' setClause)*
      (WHERE whereClause)?
    ;

setClause
    : columnReference '=' expression
    ;

// DELETE 语句
deleteStatement
    : DELETE FROM tableName
      (WHERE whereClause)?
    ;

// CREATE TABLE 语句
createTableStatement
    : CREATE TABLE tableName
      '(' columnDefinition (',' columnDefinition)* ')'
    ;

columnDefinition
    : identifier dataType (columnConstraint)*
    ;

dataType
    : INT
    | INTEGER
    | BIGINT
    | SMALLINT
    | TINYINT
    | VARCHAR '(' INTEGER_LITERAL ')'
    | CHAR '(' INTEGER_LITERAL ')'
    | TEXT
    | BOOLEAN
    | BOOL
    | FLOAT
    | DOUBLE
    | DECIMAL '(' INTEGER_LITERAL ',' INTEGER_LITERAL ')'
    | DATE
    | DATETIME
    | TIMESTAMP
    ;

columnConstraint
    : NOT NULL
    | NULL
    | PRIMARY KEY
    | UNIQUE
    | DEFAULT expression
    | AUTO_INCREMENT
    ;

// DROP TABLE 语句
dropTableStatement
    : DROP TABLE tableName
    ;

// CREATE INDEX 语句
createIndexStatement
    : CREATE INDEX? indexName
      ON tableName '(' columnList ')'
    ;

// DROP INDEX 语句
dropIndexStatement
    : DROP INDEX indexName ON tableName
    ;

// 事务语句
beginTransactionStatement
    : BEGIN (TRANSACTION)?
    ;

commitStatement
    : COMMIT
    ;

rollbackStatement
    : ROLLBACK
    ;

// 表达式
expression
    : literal                                                           # literalExpression
    | columnReference                                                    # columnExpression
    | '(' expression ')'                                                # parenthesizedExpression
    | expression operator expression                                     # binaryExpression
    | functionCall                                                      # functionExpression
    | CASE expression? (WHEN expression THEN expression)+ (ELSE expression)? END # caseExpression
    | EXISTS '(' selectStatement ')'                                    # existsExpression
    | expression IN '(' (selectStatement | valueList) ')'               # inExpression
    | expression BETWEEN expression AND expression                       # betweenExpression
    | expression IS (NOT)? NULL                                         # isNullExpression
    | NOT expression                                                    # notExpression
    | (PLUS | MINUS) expression                                        # unaryExpression
    ;

operator
    : PLUS
    | MINUS
    | ASTERISK
    | SLASH
    | PERCENT
    | EQUAL
    | NOT_EQUAL
    | LESS
    | LESS_EQUAL
    | GREATER
    | GREATER_EQUAL
    | AND
    | OR
    | LIKE
    ;

functionCall
    : functionName '(' (DISTINCT? expression (',' expression)*)? ')'
    ;

functionName
    : COUNT
    | SUM
    | AVG
    | MIN
    | MAX
    | identifier
    ;

columnReference
    : (tableName '.')? identifier
    ;

tableName
    : identifier
    ;

indexName
    : identifier
    ;

identifier
    : IDENTIFIER
    | QUOTED_IDENTIFIER
    ;

literal
    : STRING_LITERAL
    | INTEGER_LITERAL
    | FLOAT_LITERAL
    | BOOLEAN_LITERAL
    | NULL_LITERAL
    ;

// 词法规则
SELECT: S E L E C T;
INSERT: I N S E R T;
UPDATE: U P D A T E;
DELETE: D E L E T E;
CREATE: C R E A T E;
DROP: D R O P;
TABLE: T A B L E;
INDEX: I N D E X;
FROM: F R O M;
WHERE: W H E R E;
GROUP: G R O U P;
BY: B Y;
HAVING: H A V I N G;
ORDER: O R D E R;
LIMIT: L I M I T;
AS: A S;
INTO: I N T O;
VALUES: V A L U E S;
SET: S E T;
ON: O N;
BEGIN: B E G I N;
COMMIT: C O M M I T;
ROLLBACK: R O L L B A C K;
TRANSACTION: T R A N S A C T I O N;

// 数据类型
INT: I N T;
INTEGER: I N T E G E R;
BIGINT: B I G I N T;
SMALLINT: S M A L L I N T;
TINYINT: T I N Y I N T;
VARCHAR: V A R C H A R;
CHAR: C H A R;
TEXT: T E X T;
BOOLEAN: B O O L E A N;
BOOL: B O O L;
FLOAT: F L O A T;
DOUBLE: D O U B L E;
DECIMAL: D E C I M A L;
DATE: D A T E;
DATETIME: D A T E T I M E;
TIMESTAMP: T I M E S T A M P;

// 约束
NOT: N O T;
NULL: N U L L;
PRIMARY: P R I M A R Y;
KEY: K E Y;
UNIQUE: U N I Q U E;
DEFAULT: D E F A U L T;
AUTO_INCREMENT: A U T O '_' I N C R E M E N T;

// 函数
COUNT: C O U N T;
SUM: S U M;
AVG: A V G;
MIN: M I N;
MAX: M A X;
DISTINCT: D I S T I N C T;

// 操作符
PLUS: '+';
MINUS: '-';
ASTERISK: '*';
SLASH: '/';
PERCENT: '%';
EQUAL: '=';
NOT_EQUAL: '!=' | '<>';
LESS: '<';
LESS_EQUAL: '<=';
GREATER: '>';
GREATER_EQUAL: '>=';
AND: A N D;
OR: O R;
LIKE: L I K E;

// 关键字
CASE: C A S E;
WHEN: W H E N;
THEN: T H E N;
ELSE: E L S E;
END: E N D;
EXISTS: E X I S T S;
IN: I N;
BETWEEN: B E T W E E N;
IS: I S;

// 排序
ASC: A S C;
DESC: D E S C;

// 字面量
STRING_LITERAL: '\'' (~['\\] | '\\' .)* '\'';
INTEGER_LITERAL: [0-9]+;
FLOAT_LITERAL: [0-9]+ '.' [0-9]* | '.' [0-9]+;
BOOLEAN_LITERAL: T R U E | F A L S E;
NULL_LITERAL: N U L L;

// 标识符
IDENTIFIER: [a-zA-Z_][a-zA-Z0-9_]*;
QUOTED_IDENTIFIER: '`' (~[`])* '`';

// 空白字符
WHITESPACE: [ \t\r\n]+ -> skip;

// 注释
COMMENT: '--' ~[\r\n]* -> skip;
BLOCK_COMMENT: '/*' .*? '*/' -> skip;

// 辅助规则
fragment A: [aA];
fragment B: [bB];
fragment C: [cC];
fragment D: [dD];
fragment E: [eE];
fragment F: [fF];
fragment G: [gG];
fragment H: [hH];
fragment I: [iI];
fragment J: [jJ];
fragment K: [kK];
fragment L: [lL];
fragment M: [mM];
fragment N: [nN];
fragment O: [oO];
fragment P: [pP];
fragment Q: [qQ];
fragment R: [rR];
fragment S: [sS];
fragment T: [tT];
fragment U: [uU];
fragment V: [vV];
fragment W: [wW];
fragment X: [xX];
fragment Y: [yY];
fragment Z: [zZ];
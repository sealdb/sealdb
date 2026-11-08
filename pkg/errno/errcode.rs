pub const ERR_DB_CREATE_EXISTS: i32 = 1007;
pub const ERR_DB_DROP_EXISTS: i32 = 1008;
pub const ERR_NO_DB: i32 = 1046;
pub const ERR_BAD_DB: i32 = 1049;
pub const ERR_TABLE_EXISTS: i32 = 1050;
pub const ERR_PARSE_ERROR: i32 = 1064;
pub const ERR_UNKNOWN_TABLE: i32 = 1109;
pub const ERR_BAD_FIELD: i32 = 1054;
/// Column name collides across join inputs (`Column 'a' in from clause is ambiguous`).
pub const ERR_AMBIGUOUS_COLUMN: i32 = 1052;
pub const ERR_WRONG_FIELD_WITH_GROUP: i32 = 1055;
pub const ERR_NO_SUCH_TABLE: i32 = 1146;
pub const ERR_UNKNOWN_ERROR: i32 = 1105;
pub const ERR_WRONG_VALUE_COUNT: i32 = 1058;
/// MySQL `ER_WRONG_VALUE_COUNT_ON_ROW`: VALUES arity differs from the columns.
pub const ERR_WRONG_VALUE_COUNT_ON_ROW: i32 = 1136;
pub const ERR_BAD_NULL: i32 = 1048;
pub const ERR_DUP_ENTRY: i32 = 1062;
pub const ERR_DATA_OUT_OF_RANGE: i32 = 1690;
pub const ERR_TRUNCATED_WRONG_VALUE: i32 = 1292;
/// MySQL `ER_WARN_DATA_TRUNCATED`: invalid ENUM/SET member coerced to `''`.
pub const ERR_WARN_DATA_TRUNCATED: i32 = 1265;
/// TiDB: timestamp falls in a DST transition gap.
pub const ERR_TIMESTAMP_IN_DST_TRANSITION: i32 = 8179;
/// TiDB `ErrMemoryExceedForQuery`: statement exceeded `tidb_mem_quota_query`.
pub const ERR_MEMORY_EXCEEDED: i32 = 8175;
pub const ERR_UNKNOWN_FUNCTION: i32 = 1305;
pub const ERR_WRONG_ARGUMENTS: i32 = 1210;
pub const ERR_UNSUPPORTED: i32 = 1235;
pub const ERR_DIVISION_BY_ZERO: i32 = 1365;
pub const ERR_INVALID_KEY: i32 = 6100;
pub const ERR_INVALID_RECORD_KEY: i32 = 6101;
pub const ERR_INVALID_INDEX_KEY: i32 = 6102;
pub const ERR_WRITE_CONFLICT: i32 = 9007;
pub const ERR_LOCK_WAIT_TIMEOUT: i32 = 1205;
/// MySQL `ER_TABLE_NOT_LOCKED_FOR_WRITE`: `UPDATE` on a READ-locked table.
pub const ERR_TABLE_NOT_LOCKED_FOR_WRITE: i32 = 1099;
pub const ERR_LOCK_NOWAIT: i32 = 3572;
pub const ERR_LOCK_DEADLOCK: i32 = 1213;
pub const ERR_SUBQUERY_NO_1_ROW: i32 = 1242;
/// Row constructor used where a single column is required (MySQL 1241).
pub const ERR_OPERAND_COLUMNS: i32 = 1241;
pub const ERR_WRONG_NUMBER_OF_COLUMNS_IN_SELECT: i32 = 1222;
/// MySQL `ER_WRONG_USAGE`: e.g. per-arm LIMIT without parentheses in UNION.
pub const ERR_INCORRECT_USAGE: i32 = 1221;
/// CTE / derived / VIEW: SELECT list vs column names list (MySQL ER_VIEW_WRONG_LIST).
pub const ERR_VIEW_WRONG_LIST: i32 = 1353;
pub const ERR_DUP_FIELDNAME: i32 = 1060;
/// MySQL `ER_FIELD_SPECIFIED_TWICE`: `INSERT … (c1, c1)` / duplicate SET targets.
pub const ERR_FIELD_SPECIFIED_TWICE: i32 = 1110;
pub const ERR_DUP_KEYNAME: i32 = 1061;
pub const ERR_CANT_DROP_FIELD_OR_KEY: i32 = 1091;
pub const ERR_NO_PARTITION_FOR_GIVEN_VALUE: i32 = 1526;
/// MySQL `ER_UNKNOWN_PARTITION`: named partition does not exist (1735).
pub const ERR_UNKNOWN_PARTITION: i32 = 1735;
/// TiDB `ErrRowDoesNotMatchGivenPartitionSet` (`INSERT ... PARTITION` skip).
pub const ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET: i32 = 1748;
pub const ERR_ACCESS_DENIED_ERROR: i32 = 1045;
pub const ERR_DBACCESS_DENIED_ERROR: i32 = 1044;
pub const ERR_TABLEACCESS_DENIED_ERROR: i32 = 1142;
pub const ERR_INFO_SCHEMA_EXPIRED: i32 = 8027;
pub const ERR_INFO_SCHEMA_CHANGED: i32 = 8028;
pub const ERR_UNKNOWN_STMT_HANDLER: i32 = 1243;
pub const ERR_UNKNOWN_CHARACTER_SET: i32 = 1115;
pub const ERR_UNKNOWN_COLLATION: i32 = 1273;
pub const ERR_WRONG_OBJECT: i32 = 1347;
pub const ERR_VIEW_CHECK_FAILED: i32 = 1369;
/// VIEW WITH CHECK OPTION is invalid for ALGORITHM=TEMPTABLE (MySQL 1471).
pub const ERR_VIEW_INVALID: i32 = 1471;
pub const ERR_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION: i32 = 1792;
/// TiDB `ErrCTEMaxRecursionDepth`: recursive CTE exceeded @@cte_max_recursion_depth.
pub const ERR_CTE_MAX_RECURSION_DEPTH: i32 = 3636;
/// TiDB `ErrFunctionalIndexOnJSONOrGeometryFunction`.
pub const ERR_FUNCTIONAL_INDEX_ON_JSON_OR_GEOMETRY_FUNCTION: i32 = 3753;
/// TiDB `ErrPrimaryKeyOnExprColumn`: `PRIMARY KEY ((a + b))`.
pub const ERR_PRIMARY_KEY_ON_EXPR_COLUMN: i32 = 3756;
/// TiDB `ErrFunctionalIndexOnField`: expression index over a disallowed function.
pub const ERR_FUNCTIONAL_INDEX_ON_FIELD: i32 = 3758;
/// TiDB `ErrFunctionalIndexOnBlob`: the storage engine cannot index the expr.
pub const ERR_FUNCTIONAL_INDEX_ON_BLOB: i32 = 3761;
/// TiDB `ErrFunctionalIndexRowValueIsNotAllowed`: `KEY k (((j, j)))`.
pub const ERR_FUNCTIONAL_INDEX_ROW_VALUE_IS_NOT_ALLOWED: i32 = 3800;

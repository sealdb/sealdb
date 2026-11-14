pub const ERR_DB_CREATE_EXISTS: i32 = 1007;
pub const ERR_DB_DROP_EXISTS: i32 = 1008;
pub const ERR_NO_DB: i32 = 1046;
pub const ERR_BAD_DB: i32 = 1049;
pub const ERR_TABLE_EXISTS: i32 = 1050;
/// TiDB `ErrTTLUnsupportedPKType`: a clustered primary key containing FLOAT /
/// DOUBLE columns cannot use TTL (8153).
pub const ERR_TTL_UNSUPPORTED_PK_TYPE: i32 = 8153;
/// MySQL `ER_BAD_TABLE_ERROR`: `DROP TABLE` naming a VIEW (1051).
pub const ERR_BAD_TABLE: i32 = 1051;
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
/// MySQL `ER_WARN_DATA_OUT_OF_RANGE`: value outside the target type's range (1264).
pub const ERR_WARN_DATA_OUT_OF_RANGE: i32 = 1264;
/// MySQL `ER_WARN_DATA_TRUNCATED`: invalid ENUM/SET member coerced to `''`.
pub const ERR_WARN_DATA_TRUNCATED: i32 = 1265;
/// TiDB: timestamp falls in a DST transition gap.
pub const ERR_TIMESTAMP_IN_DST_TRANSITION: i32 = 8179;
/// TiDB `ErrMemoryExceedForQuery`: statement exceeded `tidb_mem_quota_query`.
pub const ERR_MEMORY_EXCEEDED: i32 = 8175;
pub const ERR_UNKNOWN_FUNCTION: i32 = 1305;
/// TiDB `ErrInvalidJSONText`: malformed JSON document (3140).
pub const ERR_INVALID_JSON_TEXT: i32 = 3140;
/// TiDB `ErrInvalidJSONTextInParam`: malformed JSON passed to a function (3141).
pub const ERR_INVALID_JSON_TEXT_IN_PARAM: i32 = 3141;
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
/// TiDB `ErrViewNoExplain`: EXPLAIN/SHOW on a VIEW without the needed
/// privileges on the view / underlying tables.
pub const ERR_VIEW_NO_EXPLAIN: i32 = 1345;
pub const ERR_INFO_SCHEMA_EXPIRED: i32 = 8027;
pub const ERR_INFO_SCHEMA_CHANGED: i32 = 8028;
pub const ERR_UNKNOWN_STMT_HANDLER: i32 = 1243;
pub const ERR_UNKNOWN_CHARACTER_SET: i32 = 1115;
pub const ERR_UNKNOWN_COLLATION: i32 = 1273;
pub const ERR_WRONG_OBJECT: i32 = 1347;
pub const ERR_VIEW_CHECK_FAILED: i32 = 1369;
/// TiDB `ErrViewSelectVariable`: a VIEW SELECT references a variable or
/// parameter (1351).
pub const ERR_VIEW_SELECT_VARIABLE: i32 = 1351;
/// MySQL `ER_TOO_LONG_IDENT`: identifier longer than 64 characters (1059).
pub const ERR_IDENTIFIER_TOO_LONG: i32 = 1059;
/// MySQL `ER_VIEW_INVALID`: a VIEW definition references tables/columns that
/// no longer exist (1356).
pub const ERR_VIEW_INVALID_TABLE: i32 = 1356;
/// TiDB `ErrDefinerTooLong`: VIEW definer user > 32 or host > 255 chars (1470).
pub const ERR_DEFINER_TOO_LONG: i32 = 1470;
/// VIEW WITH CHECK OPTION is invalid for ALGORITHM=TEMPTABLE (MySQL 1471).
pub const ERR_VIEW_INVALID: i32 = 1471;
/// TiDB `ErrViewRecursive`: a VIEW definition references itself (1462).
pub const ERR_VIEW_RECURSIVE: i32 = 1462;
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
/// TiDB `ErrUnsupportedOnGeneratedColumn`: an ALTER on a column referenced by
/// a generated column (`MODIFY a bigint` → 3106).
pub const ERR_UNSUPPORTED_ON_GENERATED_COLUMN: i32 = 3106;
/// TiDB `ErrDependentByGeneratedColumn`: DROP / CHANGE / RENAME of a column a
/// generated column depends on.
pub const ERR_DEPENDENT_BY_GENERATED_COLUMN: i32 = 3108;
/// TiDB `ErrDependentByPartitionFunctional`: the column is used by the
/// partitioning expression, so it cannot be dropped or renamed.
pub const ERR_DEPENDENT_BY_PARTITION_FUNCTIONAL: i32 = 3855;
/// TiDB `ErrTooBigDisplayWidth`: `BIT(65)` / `INT(0)` — the declared display
/// width is outside `[1, type max]`.
pub const ERR_TOO_BIG_DISPLAYWIDTH: i32 = 1439;
/// TiDB `ErrInvalidDefault`: a column default that is invalid for the column
/// type/precision (e.g. `TIMESTAMP` default `NOW(1)` for fsp 0).
pub const ERR_INVALID_DEFAULT: i32 = 1067;
/// TiDB `ErrTTLInvalidColumn`: the `TTL` expression column is missing or not
/// `DATETIME`/`DATE`/`TIMESTAMP`.
pub const ERR_TTL_INVALID_COLUMN: i32 = 8148;
/// TiDB `ErrTTLDropColumn`: a column used by the TTL config cannot be dropped.
pub const ERR_TTL_DROP_COLUMN: i32 = 8149;
/// TiDB `ErrTTLNoTTLConfig`: `TTL_ENABLE`/`TTL_JOB_INTERVAL` without `TTL`.
pub const ERR_TTL_NO_CONFIG: i32 = 8150;
/// TiDB `ErrTTLTemporaryTable`: `TTL` is not allowed on temporary tables.
pub const ERR_TTL_TEMPORARY_TABLE: i32 = 8151;
/// MySQL `ER_PRIMARY_CANT_HAVE_NULL`: a PRIMARY KEY column cannot be NULL (1171).
pub const ERR_PRIMARY_CANT_HAVE_NULL: i32 = 1171;
/// MySQL `ER_BLOB_USED_AS_KEY`: a BLOB/TEXT column cannot be a full index key
/// (1170).
pub const ERR_BLOB_USED_AS_KEY: i32 = 1170;
/// MySQL `ER_NO_DEFAULT_FOR_FIELD`: a strict insert supplies no value and the
/// column has no default (1364).
pub const ERR_FIELD_NOT_DEFAULT: i32 = 1364;
/// MySQL `ER_WRONG_KEY_COLUMN`: a key part has an invalid length, e.g. the
/// prefix length 0 in `KEY (b(0))` (1391).
pub const ERR_WRONG_KEY_COLUMN: i32 = 1391;
/// TiDB `ErrFKIncompatibleColumns`: an `ALTER` would make a foreign key's
/// referencing and referenced columns disagree on type (3780).
pub const ERR_FK_INCOMPATIBLE_COLUMNS: i32 = 3780;
/// TiDB `ErrTTLFKReferenced`: `TTL` is not allowed on a table that is
/// referenced by a foreign key.
pub const ERR_TTL_FK_REFERENCED: i32 = 8152;
/// TiDB `ErrTTLMultiSchemaChange`: TTL change cannot be combined with other
/// specs in one `ALTER TABLE`.
pub const ERR_TTL_MULTI_SCHEMA_CHANGE: i32 = 8200;
/// TiDB `ErrInvalidAutoRandom`: `AUTO_RANDOM` misuse (explicit insert with
/// `@@allow_auto_random_explicit_insert` off, invalid conversion, …).
pub const ERR_INVALID_AUTO_RANDOM: i32 = 8216;
/// TiDB `ErrGeneratedColumnFunctionIsNotAllowed` (3102).
pub const ERR_GENERATED_COLUMN_FUNCTION_NOT_ALLOWED: i32 = 3102;
/// TiDB `ErrGeneratedColumnRefAutoInc`: generated column references an
/// AUTO_INCREMENT column while `tidb_enable_auto_increment_in_generated` is off.
pub const ERR_GENERATED_COLUMN_REF_AUTO_INC: i32 = 3109;
/// TiDB `ErrColumnWithGeneratedDependencies`: a generated column's position
/// would break another generated column's dependency order (3107).
pub const ERR_COLUMN_WITH_GENERATED_DEPENDENCIES: i32 = 3107;
/// TiDB `ErrFunctionalIndexOnAutoIncrementColumn` (3754).
pub const ERR_FUNCTIONAL_INDEX_ON_AUTO_INCREMENT: i32 = 3754;
/// TiDB `ErrUnsupportedModifyCharset` (8200): an existing column's character
/// set cannot be changed, e.g. `MODIFY c VARCHAR(10) CHARACTER SET gbk`.
pub const ERR_UNSUPPORTED_MODIFY_CHARSET: i32 = 8200;
/// TiDB `ErrWrongParamcountToNativeFct` (1582): a builtin function is called
/// with an argument count outside its signature, e.g. `LOWER(a, 2)`.
pub const ERR_WRONG_PARAMCOUNT_TO_NATIVE_FCT: i32 = 1582;
/// MySQL `ER_BLOB_CANT_HAVE_DEFAULT` (1101): a literal default on a
/// `BLOB`/`TEXT`/`JSON` column.
pub const ERR_BLOB_CANT_HAVE_DEFAULT: i32 = 1101;

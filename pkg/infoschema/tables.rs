/// Virtual INFORMATION_SCHEMA tables (TiDB `pkg/infoschema/tables.go`).
/// Completeness wave: cluster/TiKV/processlist I_S, SESSION_VARIABLES from live session.
use crate::InfoSchema;
use sealdb_errno::{ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_meta::{IndexInfo, PartitionType, TableInfo};
use sealdb_types::Datum;

pub fn virtual_rows(table: &str, is: &InfoSchema) -> Result<(Vec<String>, Vec<Vec<Datum>>)> {
    match table.to_ascii_lowercase().as_str() {
        "schemata" => Ok(schemata(is)),
        "tables" => Ok(tables(is)),
        "columns" => Ok(columns(is)),
        "statistics" => Ok(statistics(is)),
        "tidb_indexes" => Ok(tidb_indexes(is)),
        "key_column_usage" => Ok(key_column_usage(is)),
        "table_constraints" => Ok(table_constraints(is)),
        "partitions" => Ok(partitions(is)),
        "processlist" => Ok(processlist()),
        "ddl_jobs" => Ok(ddl_jobs()),
        "attributes" => Ok(attributes(is)),
        "inspection_rules" => Ok(inspection_rules()),
        "character_sets" => Ok(character_sets()),
        "collations" => Ok(collations()),
        "collation_character_set_applicability" => Ok(collation_applicability()),
        "engines" => Ok(engines()),
        "views" => Ok(views(is)),
        "referential_constraints" => Ok(empty(&[
            "CONSTRAINT_SCHEMA",
            "CONSTRAINT_NAME",
            "TABLE_NAME",
            "REFERENCED_TABLE_NAME",
        ])),
        "sequences" => Ok(sequences(is)),
        "check_constraints" => Ok(empty(&[
            "CONSTRAINT_CATALOG",
            "CONSTRAINT_SCHEMA",
            "CONSTRAINT_NAME",
            "CHECK_CLAUSE",
        ])),
        "triggers" => Ok(empty(&[
            "TRIGGER_SCHEMA",
            "TRIGGER_NAME",
            "EVENT_OBJECT_TABLE",
        ])),
        "routines" => Ok(empty(&["ROUTINE_SCHEMA", "ROUTINE_NAME", "ROUTINE_TYPE"])),
        // `mysql.stats_*` (TiDB's statistics system tables), read from the
        // statistics handle so `select … from mysql.stats_meta` works.
        "stats_meta" => Ok(stats_meta(is)),
        "stats_histograms" => Ok(stats_histograms(is)),
        "stats_buckets" => Ok(stats_buckets(is)),
        "stats_top_n" => Ok(stats_top_n(is)),
        // `mysql.bind_info` rows carry session bindings from the domain; the
        // executor fills them, this exposes the column layout.
        "bind_info" => Ok((
            vec![
                "original_sql".to_string(),
                "normalized_sql".to_string(),
                "sql_digest".to_string(),
                "plan_digest".to_string(),
                "bind_sql".to_string(),
                "status".to_string(),
                "create_time".to_string(),
                "update_time".to_string(),
                "charset".to_string(),
                "collation".to_string(),
                "source".to_string(),
                "sql_mode".to_string(),
                "hint".to_string(),
            ],
            Vec::new(),
        )),
        other => Err(Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table 'information_schema.{other}' doesn't exist"),
        )),
    }
}

/// Column names a virtual table exposes, without building its rows.
pub fn virtual_columns(table: &str) -> Vec<String> {
    let empty = InfoSchema::empty();
    virtual_rows(table, &empty)
        .map(|(columns, _)| columns)
        .unwrap_or_default()
}

fn processlist() -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "ID".to_string(),
        "USER".to_string(),
        "HOST".to_string(),
        "DB".to_string(),
        "COMMAND".to_string(),
        "TIME".to_string(),
        "STATE".to_string(),
        "INFO".to_string(),
        "RESOURCE_GROUP".to_string(),
    ];
    let mut rows = Vec::new();
    if let Some(p) = crate::infoschema::thread_process() {
        rows.push(vec![
            Datum::U64(p.id),
            bytes(&p.user),
            bytes(&p.host),
            bytes(&p.db),
            bytes(&p.command),
            Datum::I64(p.time),
            bytes(&p.state),
            bytes(&p.info),
            bytes(&p.resource_group),
        ]);
    }
    (schema, rows)
}

/// `information_schema.ddl_jobs`: TiDB's `ADMIN SHOW DDL JOBS` catalog. The
/// executing session publishes the domain's jobs for the statement.
fn ddl_jobs() -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "JOB_ID".to_string(),
        "DB_NAME".to_string(),
        "TABLE_NAME".to_string(),
        "JOB_TYPE".to_string(),
        "SCHEMA_STATE".to_string(),
        "SCHEMA_ID".to_string(),
        "TABLE_ID".to_string(),
        "ROW_COUNT".to_string(),
        "CREATE_TIME".to_string(),
        "START_TIME".to_string(),
        "END_TIME".to_string(),
        "STATE".to_string(),
        "QUERY".to_string(),
    ];
    let rows = crate::infoschema::thread_ddl_jobs()
        .into_iter()
        .map(|job| {
            vec![
                Datum::I64(job.id),
                bytes(&job.db_name),
                bytes(&job.table_name),
                bytes(&job.job_type),
                bytes(&job.schema_state),
                Datum::I64(0),
                Datum::I64(0),
                Datum::I64(0),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                bytes(&job.state),
                bytes(&job.job_type),
            ]
        })
        .collect();
    (schema, rows)
}

fn schemata(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "CATALOG_NAME".to_string(),
        "SCHEMA_NAME".to_string(),
        "DEFAULT_CHARACTER_SET_NAME".to_string(),
        "DEFAULT_COLLATION_NAME".to_string(),
    ];
    let mut rows = vec![schema_row("information_schema")];
    for db in is.all_schemas() {
        rows.push(schema_row(db.name.o()));
    }
    (schema, rows)
}

fn schema_row(name: &str) -> Vec<Datum> {
    vec![
        bytes("def"),
        bytes(name),
        bytes("utf8mb4"),
        bytes("utf8mb4_bin"),
    ]
}

/// TiDB `autoid.InformationSchemaDBID` (`SystemSchemaIDFlag | 1`).
const INFO_SCHEMA_DB_ID: i64 = 1 << 62 | 1;

/// TiDB `tableIDMap` (`pkg/infoschema/tables.go`): the offset of every
/// `information_schema` virtual table from `INFO_SCHEMA_DB_ID`. The gaps mirror
/// TiDB's historical numbering (slot 66 is a removed table; `def` owns 9), so
/// the ids stay byte-for-byte compatible with TiDB.
const INFO_SCHEMA_TABLE_IDS: &[(&str, i64)] = &[
    ("SCHEMATA", 1),
    ("TABLES", 2),
    ("COLUMNS", 3),
    ("COLUMN_STATISTICS", 4),
    ("STATISTICS", 5),
    ("CHARACTER_SETS", 6),
    ("COLLATIONS", 7),
    ("FILES", 8),
    ("PROFILING", 10),
    ("PARTITIONS", 11),
    ("KEY_COLUMN_USAGE", 12),
    ("REFERENTIAL_CONSTRAINTS", 13),
    ("SESSION_VARIABLES", 14),
    ("PLUGINS", 15),
    ("TABLE_CONSTRAINTS", 16),
    ("TRIGGERS", 17),
    ("USER_PRIVILEGES", 18),
    ("SCHEMA_PRIVILEGES", 19),
    ("TABLE_PRIVILEGES", 20),
    ("COLUMN_PRIVILEGES", 21),
    ("ENGINES", 22),
    ("VIEWS", 23),
    ("ROUTINES", 24),
    ("PARAMETERS", 25),
    ("EVENTS", 26),
    ("GLOBAL_STATUS", 27),
    ("GLOBAL_VARIABLES", 28),
    ("SESSION_STATUS", 29),
    ("OPTIMIZER_TRACE", 30),
    ("TABLESPACES", 31),
    ("COLLATION_CHARACTER_SET_APPLICABILITY", 32),
    ("PROCESSLIST", 33),
    ("TIDB_INDEXES", 34),
    ("SLOW_QUERY", 35),
    ("TIDB_HOT_REGIONS", 36),
    ("TIKV_STORE_STATUS", 37),
    ("ANALYZE_STATUS", 38),
    ("TIKV_REGION_STATUS", 39),
    ("TIKV_REGION_PEERS", 40),
    ("TIDB_SERVERS_INFO", 41),
    ("CLUSTER_INFO", 42),
    ("CLUSTER_CONFIG", 43),
    ("CLUSTER_LOAD", 44),
    ("TIFLASH_REPLICA", 45),
    ("CLUSTER_SLOW_QUERY", 46),
    ("CLUSTER_PROCESSLIST", 47),
    ("CLUSTER_LOG", 48),
    ("CLUSTER_HARDWARE", 49),
    ("CLUSTER_SYSTEMINFO", 50),
    ("INSPECTION_RESULT", 51),
    ("METRICS_SUMMARY", 52),
    ("METRICS_SUMMARY_BY_LABEL", 53),
    ("METRICS_TABLES", 54),
    ("INSPECTION_SUMMARY", 55),
    ("INSPECTION_RULES", 56),
    ("DDL_JOBS", 57),
    ("SEQUENCES", 58),
    ("STATEMENTS_SUMMARY", 59),
    ("STATEMENTS_SUMMARY_HISTORY", 60),
    ("CLUSTER_STATEMENTS_SUMMARY", 61),
    ("CLUSTER_STATEMENTS_SUMMARY_HISTORY", 62),
    ("TABLE_STORAGE_STATS", 63),
    ("TIFLASH_TABLES", 64),
    ("TIFLASH_SEGMENTS", 65),
    ("CLIENT_ERRORS_SUMMARY_GLOBAL", 67),
    ("CLIENT_ERRORS_SUMMARY_BY_USER", 68),
    ("CLIENT_ERRORS_SUMMARY_BY_HOST", 69),
    ("TIDB_TRX", 70),
    ("CLUSTER_TIDB_TRX", 71),
    ("DEADLOCKS", 72),
    ("CLUSTER_DEADLOCKS", 73),
    ("DATA_LOCK_WAITS", 74),
    ("STATEMENTS_SUMMARY_EVICTED", 75),
    ("CLUSTER_STATEMENTS_SUMMARY_EVICTED", 76),
    ("ATTRIBUTES", 77),
    ("TIDB_HOT_REGIONS_HISTORY", 78),
    ("PLACEMENT_POLICIES", 79),
    ("TRX_SUMMARY", 80),
    ("CLUSTER_TRX_SUMMARY", 81),
    ("VARIABLES_INFO", 82),
    ("USER_ATTRIBUTES", 83),
    ("MEMORY_USAGE", 84),
    ("MEMORY_USAGE_OPS_HISTORY", 85),
    ("CLUSTER_MEMORY_USAGE", 86),
    ("CLUSTER_MEMORY_USAGE_OPS_HISTORY", 87),
    ("RESOURCE_GROUPS", 88),
    ("RUNAWAY_WATCHES", 89),
    ("CHECK_CONSTRAINTS", 90),
    ("TIDB_CHECK_CONSTRAINTS", 91),
    ("KEYWORDS", 92),
    ("TIDB_INDEX_USAGE", 93),
    ("CLUSTER_TIDB_INDEX_USAGE", 94),
    ("TIFLASH_INDEXES", 95),
];

/// TiDB `information_schema.tables` sharding info column
/// (`infoschema.GetShardingInfo`).
fn sharding_info(table: &TableInfo, db: &str) -> Datum {
    if table.is_view() || db.eq_ignore_ascii_case("information_schema") {
        return Datum::Null;
    }
    if table.pk_is_handle {
        bytes("NOT_SHARDED(PK_IS_HANDLE)")
    } else {
        bytes("NOT_SHARDED")
    }
}

fn tables(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "TABLE_TYPE".to_string(),
        "ENGINE".to_string(),
        "VERSION".to_string(),
        "ROW_FORMAT".to_string(),
        "TABLE_ROWS".to_string(),
        "AVG_ROW_LENGTH".to_string(),
        "DATA_LENGTH".to_string(),
        "MAX_DATA_LENGTH".to_string(),
        "INDEX_LENGTH".to_string(),
        "DATA_FREE".to_string(),
        "AUTO_INCREMENT".to_string(),
        "CREATE_TIME".to_string(),
        "UPDATE_TIME".to_string(),
        "CHECK_TIME".to_string(),
        "TABLE_COLLATION".to_string(),
        "CHECKSUM".to_string(),
        "CREATE_OPTIONS".to_string(),
        "TABLE_COMMENT".to_string(),
        "TIDB_TABLE_ID".to_string(),
        "TIDB_ROW_ID_SHARDING_INFO".to_string(),
        "TIDB_PK_TYPE".to_string(),
        "TIDB_PLACEMENT_POLICY_NAME".to_string(),
        "TIDB_TABLE_MODE".to_string(),
        "TIDB_AFFINITY".to_string(),
    ];
    let mut rows = Vec::new();
    // TiDB sorts schemas by name, so `INFORMATION_SCHEMA` comes first.
    for (name, offset) in INFO_SCHEMA_TABLE_IDS {
        rows.push(vec![
            bytes("def"),
            bytes("INFORMATION_SCHEMA"),
            bytes(name),
            bytes("SYSTEM VIEW"),
            bytes("InnoDB"),
            Datum::I64(10),
            bytes("Compact"),
            Datum::I64(0),
            Datum::I64(0),
            Datum::I64(0),
            Datum::I64(0),
            Datum::I64(0),
            Datum::I64(0),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            bytes("utf8mb4_bin"),
            Datum::Null,
            bytes(""),
            bytes(""),
            Datum::I64(INFO_SCHEMA_DB_ID + offset),
            Datum::Null,
            bytes("NONCLUSTERED"),
            Datum::Null,
            bytes("Normal"),
            Datum::Null,
        ]);
    }
    for_each_table(is, |db, table| {
        let tidb_table_id = Datum::I64(table.id);
        let pk_type = if table.pk_is_clustered() {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        if table.is_view() {
            rows.push(vec![
                bytes("def"),
                bytes(db),
                bytes(table.name.o()),
                bytes("VIEW"),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                bytes("VIEW"),
                tidb_table_id,
                Datum::Null,
                bytes(pk_type),
                Datum::Null,
                Datum::Null,
                Datum::Null,
            ]);
            return;
        }
        let table_type = if table.is_sequence() {
            "SEQUENCE"
        } else {
            "BASE TABLE"
        };
        let create_options = if table.partition.is_some() {
            "partitioned"
        } else {
            ""
        };
        let table_stats = is.stats().and_then(|h| h.table(db, table.name.o()));
        let (row_count, avg_row_length, data_length, index_length) =
            estimate_row_lengths(table, table_stats.as_ref());
        rows.push(vec![
            bytes("def"),
            bytes(db),
            bytes(table.name.o()),
            bytes(table_type),
            bytes("InnoDB"),
            Datum::I64(10),
            bytes("Compact"),
            Datum::I64(row_count as i64),
            Datum::I64(avg_row_length as i64),
            Datum::I64(data_length as i64),
            Datum::I64(0),
            Datum::I64(index_length as i64),
            Datum::I64(0),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            bytes(if table.collate.is_empty() {
                "utf8mb4_bin"
            } else {
                &table.collate
            }),
            Datum::Null,
            bytes(create_options),
            bytes(""),
            tidb_table_id,
            sharding_info(table, db),
            bytes(pk_type),
            Datum::Null,
            bytes("Normal"),
            Datum::Null,
        ]);
    });
    (schema, rows)
}

fn columns(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "COLUMN_NAME".to_string(),
        "ORDINAL_POSITION".to_string(),
        "DATA_TYPE".to_string(),
        "COLUMN_TYPE".to_string(),
        "IS_NULLABLE".to_string(),
        "COLUMN_KEY".to_string(),
        "COLUMN_COMMENT".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        for (i, col) in table.columns.iter().enumerate() {
            // Hidden expression-index columns are internal.
            if col.hidden {
                continue;
            }
            let type_name = col.field_type.type_name();
            rows.push(vec![
                bytes("def"),
                bytes(db),
                bytes(table.name.o()),
                bytes(col.name.o()),
                Datum::I64((i + 1) as i64),
                bytes(&type_name),
                bytes(&type_name),
                bytes("YES"),
                bytes(column_key(table, col.name.o())),
                bytes(""),
            ]);
        }
    });
    (schema, rows)
}

fn statistics(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "NON_UNIQUE".to_string(),
        "INDEX_SCHEMA".to_string(),
        "INDEX_NAME".to_string(),
        "SEQ_IN_INDEX".to_string(),
        "COLUMN_NAME".to_string(),
        "COLLATION".to_string(),
        "CARDINALITY".to_string(),
        "SUB_PART".to_string(),
        "PACKED".to_string(),
        "NULLABLE".to_string(),
        "INDEX_TYPE".to_string(),
        "COMMENT".to_string(),
        "INDEX_COMMENT".to_string(),
        "IS_VISIBLE".to_string(),
        "Expression".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        for idx in &table.indices {
            for (seq, col) in idx.columns.iter().enumerate() {
                // An expression key part reports COLUMN_NAME NULL and its
                // generating expression instead (MySQL 8 / TiDB `Expression`).
                let (column_name, expression) = match table
                    .columns
                    .iter()
                    .find(|c| c.hidden && c.name.o().eq_ignore_ascii_case(col.name.o()))
                {
                    Some(hidden) => (
                        Datum::Null,
                        bytes(hidden.generated_expr.as_deref().unwrap_or_default()),
                    ),
                    None => (bytes(col.name.o()), Datum::Null),
                };
                rows.push(vec![
                    bytes("def"),
                    bytes(db),
                    bytes(table.name.o()),
                    bytes(if idx.unique || idx.primary { "0" } else { "1" }),
                    bytes(db),
                    bytes(idx.name.o()),
                    Datum::I64((seq + 1) as i64),
                    column_name,
                    bytes("A"),
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    bytes("YES"),
                    bytes("BTREE"),
                    bytes(""),
                    bytes(""),
                    bytes("YES"),
                    expression,
                ]);
            }
        }
    });
    (schema, rows)
}

/// TiDB `information_schema.tidb_indexes`: `statistics` plus the index id,
/// visibility, clustered flag and (for expression key parts) the generating
/// expression with `COLUMN_NAME` NULL.
fn tidb_indexes(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "NON_UNIQUE".to_string(),
        "KEY_NAME".to_string(),
        "SEQ_IN_INDEX".to_string(),
        "COLUMN_NAME".to_string(),
        "SUB_PART".to_string(),
        "INDEX_COMMENT".to_string(),
        "Expression".to_string(),
        "INDEX_ID".to_string(),
        "IS_VISIBLE".to_string(),
        "CLUSTERED".to_string(),
        "IS_GLOBAL".to_string(),
        "PREDICATE".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        // TiDB emits a synthetic PRIMARY row for an integer row handle (it is
        // not an index, so it carries id 0 and comes before the real indexes).
        if table.pk_is_handle
            && let Some(pk) = table.indices.iter().find(|idx| idx.primary)
        {
            for (seq, col) in pk.columns.iter().enumerate() {
                rows.push(vec![
                    bytes(db),
                    bytes(table.name.o()),
                    Datum::I64(0),
                    bytes("PRIMARY"),
                    Datum::I64((seq + 1) as i64),
                    bytes(col.name.o()),
                    Datum::Null,
                    bytes(""),
                    Datum::Null,
                    Datum::I64(0),
                    bytes("YES"),
                    bytes("YES"),
                    Datum::I64(0),
                    Datum::Null,
                ]);
            }
        }
        for idx in &table.indices {
            if idx.primary && table.pk_is_handle {
                continue;
            }
            let clustered = idx.primary && table.pk_is_clustered();
            for (seq, col) in idx.columns.iter().enumerate() {
                let (column_name, expression) = match table
                    .columns
                    .iter()
                    .find(|c| c.hidden && c.name.o().eq_ignore_ascii_case(col.name.o()))
                {
                    Some(hidden) => (
                        Datum::Null,
                        bytes(hidden.generated_expr.as_deref().unwrap_or_default()),
                    ),
                    None => (bytes(col.name.o()), Datum::Null),
                };
                rows.push(vec![
                    bytes(db),
                    bytes(table.name.o()),
                    Datum::I64(if idx.unique || idx.primary { 0 } else { 1 }),
                    bytes(idx.name.o()),
                    Datum::I64((seq + 1) as i64),
                    column_name,
                    Datum::Null,
                    bytes(""),
                    expression,
                    // TiDB `IndexInfo.ID`: an integer row handle has none (0).
                    Datum::I64(idx.id),
                    bytes("YES"),
                    bytes(if clustered { "YES" } else { "NO" }),
                    // TiDB `IndexInfo.Global`: only a `GLOBAL` index reports 1.
                    Datum::I64(if idx.global { 1 } else { 0 }),
                    Datum::Null,
                ]);
            }
        }
    });
    (schema, rows)
}

fn key_column_usage(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "CONSTRAINT_CATALOG".to_string(),
        "CONSTRAINT_SCHEMA".to_string(),
        "CONSTRAINT_NAME".to_string(),
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "COLUMN_NAME".to_string(),
        "ORDINAL_POSITION".to_string(),
        "POSITION_IN_UNIQUE_CONSTRAINT".to_string(),
        "REFERENCED_TABLE_SCHEMA".to_string(),
        "REFERENCED_TABLE_NAME".to_string(),
        "REFERENCED_COLUMN_NAME".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        for idx in table.indices.iter().filter(|idx| idx.unique || idx.primary) {
            for (seq, col) in idx.columns.iter().enumerate() {
                rows.push(vec![
                    bytes("def"),
                    bytes(db),
                    bytes(constraint_name(idx)),
                    bytes("def"),
                    bytes(db),
                    bytes(table.name.o()),
                    bytes(col.name.o()),
                    Datum::I64((seq + 1) as i64),
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                ]);
            }
        }
    });
    (schema, rows)
}

fn table_constraints(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "CONSTRAINT_CATALOG".to_string(),
        "CONSTRAINT_SCHEMA".to_string(),
        "CONSTRAINT_NAME".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "CONSTRAINT_TYPE".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        for idx in table.indices.iter().filter(|idx| idx.unique || idx.primary) {
            rows.push(vec![
                bytes("def"),
                bytes(db),
                bytes(constraint_name(idx)),
                bytes(db),
                bytes(table.name.o()),
                bytes(if idx.primary { "PRIMARY KEY" } else { "UNIQUE" }),
            ]);
        }
    });
    (schema, rows)
}

fn partitions(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    // TiDB `partitionsCols` (28 columns, SUBPARTITION_* included as NULL —
    // SealDB has no subpartitioning yet).
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "PARTITION_NAME".to_string(),
        "SUBPARTITION_NAME".to_string(),
        "PARTITION_ORDINAL_POSITION".to_string(),
        "SUBPARTITION_ORDINAL_POSITION".to_string(),
        "PARTITION_METHOD".to_string(),
        "SUBPARTITION_METHOD".to_string(),
        "PARTITION_EXPRESSION".to_string(),
        "SUBPARTITION_EXPRESSION".to_string(),
        "PARTITION_DESCRIPTION".to_string(),
        "TABLE_ROWS".to_string(),
        "AVG_ROW_LENGTH".to_string(),
        "DATA_LENGTH".to_string(),
        "MAX_DATA_LENGTH".to_string(),
        "INDEX_LENGTH".to_string(),
        "DATA_FREE".to_string(),
        "CREATE_TIME".to_string(),
        "UPDATE_TIME".to_string(),
        "CHECK_TIME".to_string(),
        "CHECKSUM".to_string(),
        "PARTITION_COMMENT".to_string(),
        "NODEGROUP".to_string(),
        "TABLESPACE_NAME".to_string(),
        "TIDB_PARTITION_ID".to_string(),
        "TIDB_PLACEMENT_POLICY_NAME".to_string(),
        "TIDB_AFFINITY".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        let Some(part) = &table.partition else {
            return;
        };
        let method = match part.tp {
            PartitionType::Range => "RANGE",
            PartitionType::Hash => "HASH",
            PartitionType::List => "LIST",
            PartitionType::Key => "KEY",
        };
        let table_stats = is.stats().and_then(|h| h.table(db, table.name.o()));
        for (i, def) in part.definitions.iter().enumerate() {
            let desc = if def.in_values.is_empty() {
                def.less_than.join(",")
            } else {
                def.in_values
                    .iter()
                    .map(|v| match v {
                        Some(v) => v.to_string(),
                        None => "NULL".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let (row_count, avg_row_length, data_length, index_length) =
                estimate_partition_row_lengths(table, table_stats.as_ref(), def.id);
            rows.push(vec![
                bytes("def"),
                bytes(db),
                bytes(table.name.o()),
                bytes(def.name.o()),
                Datum::Null,
                Datum::I64((i + 1) as i64),
                Datum::Null,
                bytes(method),
                Datum::Null,
                bytes(&part.expr),
                Datum::Null,
                if desc.is_empty() {
                    Datum::Null
                } else {
                    bytes(&desc)
                },
                Datum::I64(row_count as i64),
                Datum::I64(avg_row_length as i64),
                Datum::I64(data_length as i64),
                Datum::Null,
                Datum::I64(index_length as i64),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                bytes(""),
                Datum::Null,
                Datum::Null,
                Datum::I64(def.id),
                Datum::Null,
                Datum::Null,
            ]);
        }
    });
    (schema, rows)
}

/// TiDB `StatsTableRowCache.GetDataAndIndexLength`: fixed-length columns charge
/// `row_count * StorageLength`, variable-length columns charge the analyzed
/// `tot_col_size`. Index length sums the key parts' column lengths; a GLOBAL
/// index is charged at the table level and normal indexes at partition level.
fn data_and_index_length(
    info: &TableInfo,
    stats: Option<&sealdb_statistics::table::Table>,
    physical_id: i64,
    row_count: u64,
) -> (u64, u64) {
    let partitioned = info.partition.as_ref().is_some_and(|part| part.enable);
    let mut column_length = Vec::with_capacity(info.columns.len());
    let mut data_length = 0u64;
    for col in &info.columns {
        let length = match col.field_type.storage_length() {
            Some(len) => row_count.saturating_mul(len),
            None => match stats.and_then(|s| s.column(col.name.o())) {
                Some(col_stats) if partitioned && physical_id != info.id => col_stats
                    .part_tot_col_size
                    .iter()
                    .find(|(id, _)| *id == physical_id)
                    .map(|(_, size)| *size)
                    .unwrap_or(0),
                Some(col_stats) => col_stats.tot_col_size,
                None => 0,
            },
        };
        data_length += length;
        column_length.push(length);
    }
    let mut index_length = 0u64;
    for index in &info.indices {
        if partitioned {
            // TiDB: GLOBAL indexes are charged once at table level, the rest
            // per partition.
            if index.global && info.id != physical_id {
                continue;
            }
            if !index.global && info.id == physical_id {
                continue;
            }
        }
        for part in &index.columns {
            if let Some(pos) = info
                .columns
                .iter()
                .position(|c| c.name.o().eq_ignore_ascii_case(part.name.o()))
            {
                index_length += column_length[pos];
            }
        }
    }
    (data_length, index_length)
}

/// TiDB `StatsTableRowCache.EstimateDataLength` for `information_schema.tables`:
/// a partitioned table's rows and data live under its partitions, while GLOBAL
/// indexes are charged at the table level.
fn estimate_row_lengths(
    info: &TableInfo,
    stats: Option<&sealdb_statistics::table::Table>,
) -> (u64, u64, u64, u64) {
    let mut row_count = stats.map(|s| s.count).unwrap_or(0);
    let (mut data_length, mut index_length) =
        data_and_index_length(info, stats, info.id, row_count);
    if let Some(part) = info.partition.as_ref().filter(|part| part.enable) {
        row_count = 0;
        data_length = 0;
        for def in &part.definitions {
            let part_rows = stats
                .and_then(|s| {
                    s.part_counts
                        .iter()
                        .find(|(id, _)| *id == def.id)
                        .map(|(_, count)| *count)
                })
                .unwrap_or(0);
            let (part_data, part_index) = data_and_index_length(info, stats, def.id, part_rows);
            row_count += part_rows;
            data_length += part_data;
            index_length += part_index;
        }
    }
    if info.is_sequence() {
        row_count = 1;
    }
    let avg_row_length = data_length.checked_div(row_count).unwrap_or(0);
    (row_count, avg_row_length, data_length, index_length)
}

/// One partition's `TABLE_ROWS` / `AVG_ROW_LENGTH` / `DATA_LENGTH` /
/// `INDEX_LENGTH` (TiDB `infoschema/tables.go` partition branch).
fn estimate_partition_row_lengths(
    info: &TableInfo,
    stats: Option<&sealdb_statistics::table::Table>,
    physical_id: i64,
) -> (u64, u64, u64, u64) {
    let row_count = stats
        .and_then(|s| {
            s.part_counts
                .iter()
                .find(|(id, _)| *id == physical_id)
                .map(|(_, count)| *count)
        })
        .unwrap_or(0);
    let (data_length, index_length) = data_and_index_length(info, stats, physical_id, row_count);
    let avg_row_length = data_length.checked_div(row_count).unwrap_or(0);
    (row_count, avg_row_length, data_length, index_length)
}

fn character_sets() -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "CHARACTER_SET_NAME".to_string(),
        "DEFAULT_COLLATE_NAME".to_string(),
        "DESCRIPTION".to_string(),
        "MAXLEN".to_string(),
    ];
    let rows = CHARSETS
        .iter()
        .map(|cs| {
            vec![
                bytes(cs.name),
                bytes(cs.default_collation),
                bytes(cs.description),
                Datum::I64(cs.maxlen),
            ]
        })
        .collect();
    (schema, rows)
}

fn collations() -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "COLLATION_NAME".to_string(),
        "CHARACTER_SET_NAME".to_string(),
        "ID".to_string(),
        "IS_DEFAULT".to_string(),
        "IS_COMPILED".to_string(),
        "SORTLEN".to_string(),
        "PAD_ATTRIBUTE".to_string(),
    ];
    let rows = COLLATIONS
        .iter()
        .map(|col| {
            vec![
                bytes(col.name),
                bytes(col.charset),
                Datum::I64(col.id),
                bytes(if col.is_default { "Yes" } else { "" }),
                bytes("Yes"),
                Datum::I64(1),
                bytes("PAD SPACE"),
            ]
        })
        .collect();
    (schema, rows)
}

fn collation_applicability() -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "COLLATION_NAME".to_string(),
        "CHARACTER_SET_NAME".to_string(),
    ];
    let rows = COLLATIONS
        .iter()
        .map(|col| vec![bytes(col.name), bytes(col.charset)])
        .collect();
    (schema, rows)
}

fn engines() -> (Vec<String>, Vec<Vec<Datum>>) {
    (
        vec![
            "ENGINE".to_string(),
            "SUPPORT".to_string(),
            "COMMENT".to_string(),
            "TRANSACTIONS".to_string(),
            "XA".to_string(),
            "SAVEPOINTS".to_string(),
        ],
        vec![vec![
            bytes("InnoDB"),
            bytes("DEFAULT"),
            bytes("Supports transactions, row-level locking, and foreign keys"),
            bytes("YES"),
            bytes("YES"),
            bytes("YES"),
        ]],
    )
}

fn sequences(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "SEQUENCE_SCHEMA".to_string(),
        "SEQUENCE_NAME".to_string(),
        "START".to_string(),
        "MIN_VALUE".to_string(),
        "MAX_VALUE".to_string(),
        "INCREMENT".to_string(),
        "CYCLE".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        let Some(seq) = &table.sequence else {
            return;
        };
        rows.push(vec![
            bytes("def"),
            bytes(db),
            bytes(table.name.o()),
            Datum::I64(seq.start),
            Datum::I64(seq.min_value),
            Datum::I64(seq.max_value),
            Datum::I64(seq.increment),
            bytes(if seq.cycle { "YES" } else { "NO" }),
        ]);
    });
    (schema, rows)
}

fn views(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "VIEW_DEFINITION".to_string(),
        "CHECK_OPTION".to_string(),
        "IS_UPDATABLE".to_string(),
        "DEFINER".to_string(),
        "SECURITY_TYPE".to_string(),
        "CHARACTER_SET_CLIENT".to_string(),
        "COLLATION_CONNECTION".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        let Some(view) = &table.view else {
            return;
        };
        // TiDB stores the default `WITH CASCADED CHECK OPTION` for views created
        // without an explicit clause and prints it verbatim in I_S.VIEWS.
        let check_option =
            if view.check_option.is_empty() || view.check_option.eq_ignore_ascii_case("NONE") {
                "CASCADED"
            } else {
                view.check_option.as_str()
            };
        // `root@%` (TiDB `UserIdentity.String()`), not the SHOW CREATE rendering.
        let definer = view.definer.replace('`', "");
        rows.push(vec![
            bytes("def"),
            bytes(db),
            bytes(table.name.o()),
            bytes(&view.select_stmt),
            bytes(check_option),
            bytes("NO"),
            bytes(&definer),
            bytes(&view.security),
            bytes("utf8mb4"),
            bytes("utf8mb4_general_ci"),
        ]);
    });
    (schema, rows)
}

fn empty(cols: &[&str]) -> (Vec<String>, Vec<Vec<Datum>>) {
    (cols.iter().map(|c| (*c).to_string()).collect(), Vec::new())
}

/// `information_schema.ATTRIBUTES`: TiDB placement attributes of tables and
/// partitions (`ALTER TABLE … ATTRIBUTES`, `ALTER TABLE … PARTITION p
/// ATTRIBUTES`). IDs mirror TiDB's `attrs.GetAttrID`.
fn attributes(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec!["ID".to_string(), "ATTRIBUTES".to_string()];
    let mut rows = Vec::new();
    for db in is.all_schemas() {
        for table in is.schema_tables(db.name.o()) {
            let prefix = format!("schema/{}/{}", db.name.o(), table.name.o());
            if !table.attributes.is_empty() {
                rows.push(vec![
                    bytes(&prefix),
                    bytes(&attributes_json(&table.attributes)),
                ]);
            }
            if let Some(partition) = &table.partition {
                for def in &partition.definitions {
                    if def.attributes.is_empty() {
                        continue;
                    }
                    rows.push(vec![
                        bytes(&format!("{prefix}/partition/{}", def.name.o())),
                        bytes(&attributes_json(&def.attributes)),
                    ]);
                }
            }
        }
    }
    (schema, rows)
}

/// `information_schema.INSPECTION_RULES`: the built-in rule names usable with
/// `INSPECTION_RESULT` / `INSPECTION_SUMMARY`, tagged `inspection` (single
/// diagnostic checks) or `summary` (metric families).
fn inspection_rules() -> (Vec<String>, Vec<Vec<Datum>>) {
    const RULES: &[(&str, &str)] = &[
        ("config", "inspection"),
        ("version", "inspection"),
        ("node-load", "inspection"),
        ("critical-error", "inspection"),
        ("threshold-check", "inspection"),
        ("all", "summary"),
        ("all-load", "summary"),
        ("cpu", "summary"),
        ("memory", "summary"),
        ("io", "summary"),
        ("network", "summary"),
        ("region", "summary"),
        ("time", "summary"),
        ("tikv", "summary"),
        ("pd", "summary"),
    ];
    let schema = vec!["RULE".to_string(), "TYPE".to_string()];
    let rows = RULES
        .iter()
        .map(|(rule, tp)| vec![bytes(rule), bytes(tp)])
        .collect();
    (schema, rows)
}

/// Render an attribute map as the JSON object TiDB stores in `ATTRIBUTES`.
fn attributes_json(attributes: &[(String, String)]) -> String {
    let body = attributes
        .iter()
        .map(|(key, value)| {
            format!(
                "\"{}\":\"{}\"",
                key.replace('"', "\\\""),
                value.replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

fn for_each_table(is: &InfoSchema, mut f: impl FnMut(&str, &TableInfo)) {
    for db in is.all_schemas() {
        for table in is.schema_tables(db.name.o()) {
            f(db.name.o(), table);
        }
    }
}

fn constraint_name(idx: &IndexInfo) -> &str {
    if idx.primary { "PRIMARY" } else { idx.name.o() }
}

fn column_key(table: &TableInfo, column: &str) -> &'static str {
    let mut mul = false;
    for idx in &table.indices {
        if idx
            .columns
            .iter()
            .any(|col| col.name.o().eq_ignore_ascii_case(column))
        {
            if idx.primary {
                return "PRI";
            }
            if idx.unique && idx.columns.len() == 1 {
                return "UNI";
            }
            mul = true;
        }
    }
    if mul { "MUL" } else { "" }
}

struct Charset {
    name: &'static str,
    default_collation: &'static str,
    description: &'static str,
    maxlen: i64,
}

struct Collation {
    name: &'static str,
    charset: &'static str,
    id: i64,
    is_default: bool,
}

const CHARSETS: &[Charset] = &[
    Charset {
        name: "utf8mb4",
        default_collation: "utf8mb4_bin",
        description: "UTF-8 Unicode",
        maxlen: 4,
    },
    Charset {
        name: "utf8",
        default_collation: "utf8_bin",
        description: "UTF-8 Unicode",
        maxlen: 3,
    },
    Charset {
        name: "latin1",
        default_collation: "latin1_bin",
        description: "cp1252 West European",
        maxlen: 1,
    },
    Charset {
        name: "binary",
        default_collation: "binary",
        description: "Binary pseudo charset",
        maxlen: 1,
    },
    Charset {
        name: "ascii",
        default_collation: "ascii_bin",
        description: "US ASCII",
        maxlen: 1,
    },
    Charset {
        name: "gbk",
        default_collation: "gbk_bin",
        description: "GBK charset",
        maxlen: 2,
    },
];

const COLLATIONS: &[Collation] = &[
    Collation {
        name: "utf8mb4_bin",
        charset: "utf8mb4",
        id: 46,
        is_default: true,
    },
    Collation {
        name: "utf8mb4_general_ci",
        charset: "utf8mb4",
        id: 45,
        is_default: false,
    },
    Collation {
        name: "utf8mb4_unicode_ci",
        charset: "utf8mb4",
        id: 224,
        is_default: false,
    },
    Collation {
        name: "utf8_bin",
        charset: "utf8",
        id: 83,
        is_default: true,
    },
    Collation {
        name: "latin1_bin",
        charset: "latin1",
        id: 47,
        is_default: true,
    },
    Collation {
        name: "binary",
        charset: "binary",
        id: 63,
        is_default: true,
    },
    Collation {
        name: "ascii_bin",
        charset: "ascii",
        id: 65,
        is_default: true,
    },
    Collation {
        name: "gbk_bin",
        charset: "gbk",
        id: 87,
        is_default: true,
    },
    Collation {
        name: "gbk_chinese_ci",
        charset: "gbk",
        id: 28,
        is_default: false,
    },
];

/// One `mysql.stats_*` row source: the physical table id plus the table's
/// statistics (live, or the soft-deleted snapshot from `DROP STATS`).
fn stats_rows(is: &InfoSchema) -> Vec<(i64, sealdb_statistics::table::Table)> {
    let Some(stats) = is.stats() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for db in is.all_schemas() {
        let db_name = db.name.l().to_string();
        if db_name.eq_ignore_ascii_case("information_schema")
            || db_name.eq_ignore_ascii_case("mysql")
        {
            continue;
        }
        for table in is.schema_tables(&db_name) {
            let name = table.name.l().to_string();
            if let Some(raw) = stats.raw_table(&db_name, &name) {
                out.push((table.id, raw));
            }
        }
    }
    out.sort_by_key(|(id, _)| *id);
    out
}

fn stats_meta(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let columns = vec![
        "table_id".to_string(),
        "version".to_string(),
        "modify_count".to_string(),
        "count".to_string(),
        "last_stats_histograms_version".to_string(),
    ];
    let rows = stats_rows(is)
        .into_iter()
        .map(|(id, stats)| {
            vec![
                Datum::I64(id),
                Datum::I64(stats.version as i64),
                Datum::I64(stats.modify_count as i64),
                Datum::I64(stats.count as i64),
                Datum::I64(stats.version as i64),
            ]
        })
        .collect();
    (columns, rows)
}

fn stats_histograms(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let columns = vec![
        "table_id".to_string(),
        "is_index".to_string(),
        "hist_id".to_string(),
        "distinct_count".to_string(),
        "null_count".to_string(),
        "modify_count".to_string(),
        "version".to_string(),
        "cm_sketch".to_string(),
        "stats_ver".to_string(),
        "last_analyze_pos".to_string(),
        "tot_col_size".to_string(),
        "correlation".to_string(),
        "last_update_version".to_string(),
        "flag".to_string(),
    ];
    let mut rows = Vec::new();
    for (id, stats) in stats_rows(is) {
        for (pos, (_, column)) in stats.columns.iter().enumerate() {
            rows.push(vec![
                Datum::I64(id),
                Datum::I64(0),
                Datum::I64(pos as i64 + 1),
                Datum::I64(column.ndv as i64),
                Datum::I64(column.null_count as i64),
                Datum::I64(0),
                Datum::I64(stats.version as i64),
                Datum::Null,
                Datum::I64(2),
                bytes(""),
                Datum::I64(column.tot_col_size as i64),
                Datum::I64(0),
                Datum::I64(stats.version as i64),
                Datum::I64(0),
            ]);
        }
        for (pos, (_, index)) in stats.indices.iter().enumerate() {
            rows.push(vec![
                Datum::I64(id),
                Datum::I64(1),
                Datum::I64(pos as i64 + 1),
                Datum::I64(index.ndv as i64),
                Datum::I64(index.null_count as i64),
                Datum::I64(0),
                Datum::I64(stats.version as i64),
                Datum::Null,
                Datum::I64(index.stats_ver as i64),
                bytes(""),
                Datum::I64(0),
                Datum::I64(0),
                Datum::I64(stats.version as i64),
                Datum::I64(0),
            ]);
        }
    }
    (columns, rows)
}

fn stats_buckets(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let columns = vec![
        "table_id".to_string(),
        "is_index".to_string(),
        "hist_id".to_string(),
        "bucket_id".to_string(),
        "count".to_string(),
        "repeats".to_string(),
        "upper_bound".to_string(),
        "lower_bound".to_string(),
        "ndv".to_string(),
    ];
    let mut rows = Vec::new();
    for (id, stats) in stats_rows(is) {
        for (pos, (_, column)) in stats.columns.iter().enumerate() {
            let Some(histogram) = column.histogram.as_ref() else {
                continue;
            };
            for (bucket_id, bucket) in histogram.buckets.iter().enumerate() {
                rows.push(vec![
                    Datum::I64(id),
                    Datum::I64(0),
                    Datum::I64(pos as i64 + 1),
                    Datum::I64(bucket_id as i64),
                    Datum::I64(bucket.count as i64),
                    Datum::I64(bucket.repeats as i64),
                    bytes(&bucket.upper),
                    bytes(&bucket.lower),
                    Datum::I64(bucket.ndv as i64),
                ]);
            }
        }
        for (pos, (_, index)) in stats.indices.iter().enumerate() {
            let Some(histogram) = index.histogram.as_ref() else {
                continue;
            };
            for (bucket_id, bucket) in histogram.buckets.iter().enumerate() {
                rows.push(vec![
                    Datum::I64(id),
                    Datum::I64(1),
                    Datum::I64(pos as i64 + 1),
                    Datum::I64(bucket_id as i64),
                    Datum::I64(bucket.count as i64),
                    Datum::I64(bucket.repeats as i64),
                    bytes(&String::from_utf8_lossy(&bucket.upper)),
                    bytes(&String::from_utf8_lossy(&bucket.lower)),
                    Datum::I64(bucket.ndv as i64),
                ]);
            }
        }
    }
    (columns, rows)
}

fn stats_top_n(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let columns = vec![
        "table_id".to_string(),
        "is_index".to_string(),
        "hist_id".to_string(),
        "value".to_string(),
        "count".to_string(),
    ];
    let mut rows = Vec::new();
    for (id, stats) in stats_rows(is) {
        for (pos, (_, column)) in stats.columns.iter().enumerate() {
            let Some(topn) = column.topn.as_ref() else {
                continue;
            };
            for (value, count) in &topn.items {
                rows.push(vec![
                    Datum::I64(id),
                    Datum::I64(0),
                    Datum::I64(pos as i64 + 1),
                    bytes(value),
                    Datum::I64(*count as i64),
                ]);
            }
        }
        for (pos, (_, index)) in stats.indices.iter().enumerate() {
            for (value, count) in &index.topn {
                rows.push(vec![
                    Datum::I64(id),
                    Datum::I64(1),
                    Datum::I64(pos as i64 + 1),
                    bytes(&String::from_utf8_lossy(value)),
                    Datum::I64(*count as i64),
                ]);
            }
        }
    }
    (columns, rows)
}

fn bytes(value: &str) -> Datum {
    Datum::Bytes(value.as_bytes().to_vec())
}

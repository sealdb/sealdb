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
        "key_column_usage" => Ok(key_column_usage(is)),
        "table_constraints" => Ok(table_constraints(is)),
        "partitions" => Ok(partitions(is)),
        "character_sets" => Ok(character_sets()),
        "collations" => Ok(collations()),
        "collation_character_set_applicability" => Ok(collation_applicability()),
        "engines" => Ok(engines()),
        "views" => Ok(empty(&[
            "TABLE_CATALOG",
            "TABLE_SCHEMA",
            "TABLE_NAME",
            "VIEW_DEFINITION",
            "CHECK_OPTION",
            "IS_UPDATABLE",
        ])),
        "referential_constraints" => Ok(empty(&[
            "CONSTRAINT_SCHEMA",
            "CONSTRAINT_NAME",
            "TABLE_NAME",
            "REFERENCED_TABLE_NAME",
        ])),
        "sequences" => Ok(empty(&[
            "TABLE_CATALOG",
            "SEQUENCE_SCHEMA",
            "SEQUENCE_NAME",
        ])),
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
        other => Err(Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table 'information_schema.{other}' doesn't exist"),
        )),
    }
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

fn tables(is: &InfoSchema) -> (Vec<String>, Vec<Vec<Datum>>) {
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "TABLE_TYPE".to_string(),
        "ENGINE".to_string(),
    ];
    let mut rows = Vec::new();
    for_each_table(is, |db, table| {
        rows.push(vec![
            bytes("def"),
            bytes(db),
            bytes(table.name.o()),
            bytes("BASE TABLE"),
            bytes("InnoDB"),
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
                rows.push(vec![
                    bytes("def"),
                    bytes(db),
                    bytes(table.name.o()),
                    bytes(if idx.unique || idx.primary { "0" } else { "1" }),
                    bytes(db),
                    bytes(idx.name.o()),
                    Datum::I64((seq + 1) as i64),
                    bytes(col.name.o()),
                    bytes("A"),
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    bytes("YES"),
                    bytes("BTREE"),
                    bytes(""),
                    bytes(""),
                    bytes("YES"),
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
    let schema = vec![
        "TABLE_CATALOG".to_string(),
        "TABLE_SCHEMA".to_string(),
        "TABLE_NAME".to_string(),
        "PARTITION_NAME".to_string(),
        "PARTITION_ORDINAL_POSITION".to_string(),
        "PARTITION_METHOD".to_string(),
        "PARTITION_EXPRESSION".to_string(),
        "PARTITION_DESCRIPTION".to_string(),
        "TABLE_ROWS".to_string(),
        "TIDB_PARTITION_ID".to_string(),
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
        for (i, def) in part.definitions.iter().enumerate() {
            let desc = if def.in_values.is_empty() {
                def.less_than.join(",")
            } else {
                def.in_values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            rows.push(vec![
                bytes("def"),
                bytes(db),
                bytes(table.name.o()),
                bytes(def.name.o()),
                Datum::I64((i + 1) as i64),
                bytes(method),
                bytes(&part.expr),
                if desc.is_empty() {
                    Datum::Null
                } else {
                    bytes(&desc)
                },
                Datum::I64(0),
                Datum::I64(def.id),
            ]);
        }
    });
    (schema, rows)
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

fn empty(cols: &[&str]) -> (Vec<String>, Vec<Vec<Datum>>) {
    (cols.iter().map(|c| (*c).to_string()).collect(), Vec::new())
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
];

fn bytes(value: &str) -> Datum {
    Datum::Bytes(value.as_bytes().to_vec())
}

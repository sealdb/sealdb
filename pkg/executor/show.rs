use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_BAD_DB, ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_meta::{ColumnInfo, TableInfo};
use sealdb_parser::ast::{ShowStmt, ShowStmtKind, TableName};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct ShowExec {
    domain: Arc<Domain>,
    stmt: ShowStmt,
    current_db: String,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl ShowExec {
    pub fn new(domain: Arc<Domain>, stmt: ShowStmt, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for ShowExec {
    fn open(&mut self) -> Result<()> {
        let is = self.domain.infoschema();
        match self.stmt.kind {
            ShowStmtKind::Databases => {
                self.schema = vec![show_column_header("Database", self.stmt.pattern.as_deref())];
                self.rows = is
                    .all_schemas()
                    .into_iter()
                    .map(|db| db.name.o().to_string())
                    .filter(|name| show_like_ok(name, self.stmt.pattern.as_deref()))
                    .map(|name| vec![Datum::Bytes(name.into_bytes())])
                    .collect();
            }
            ShowStmtKind::Tables => {
                let db = self
                    .stmt
                    .db_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.current_db.clone());
                if db.is_empty() {
                    return Err(Error::new(ERR_NO_DB, "No database selected"));
                }
                if is.schema_by_name(&db).is_none() {
                    return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{db}'")));
                }
                self.schema = vec![show_column_header(
                    &format!("Tables_in_{db}"),
                    self.stmt.pattern.as_deref(),
                )];
                self.rows = is
                    .schema_tables(&db)
                    .into_iter()
                    .map(|t| t.name.o().to_string())
                    .filter(|name| show_like_ok(name, self.stmt.pattern.as_deref()))
                    .map(|name| vec![Datum::Bytes(name.into_bytes())])
                    .collect();
            }
            ShowStmtKind::Grants => {
                let user = self
                    .stmt
                    .user
                    .clone()
                    .unwrap_or_else(sealdb_parser::ast::UserIdentity::root);
                self.schema = vec![format!("Grants for {}", user.display())];
                self.rows = self
                    .domain
                    .privilege()
                    .show_grants(&user)
                    .into_iter()
                    .map(|g| vec![Datum::Bytes(g.into_bytes())])
                    .collect();
            }
            ShowStmtKind::CreateDatabase => {
                let db = self
                    .stmt
                    .db_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.current_db.clone());
                if db.is_empty() {
                    return Err(Error::new(ERR_NO_DB, "No database selected"));
                }
                if is.schema_by_name(&db).is_none() {
                    return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{db}'")));
                }
                let info = is.schema_by_name(&db).expect("checked above");
                self.schema = vec!["Database".to_string(), "Create Database".to_string()];
                let create = format!(
                    "CREATE DATABASE `{db}` DEFAULT CHARACTER SET {} COLLATE {}",
                    info.charset, info.collate
                );
                self.rows = vec![vec![
                    Datum::Bytes(db.as_bytes().to_vec()),
                    Datum::Bytes(create.into_bytes()),
                ]];
            }
            ShowStmtKind::CreateTable => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                self.schema = vec!["Table".to_string(), "Create Table".to_string()];
                self.rows = vec![vec![
                    Datum::Bytes(name.as_bytes().to_vec()),
                    Datum::Bytes(render_create_table(info).into_bytes()),
                ]];
            }
            ShowStmtKind::CreateView => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing view name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                let view = info.view.as_ref().ok_or_else(|| {
                    Error::new(
                        sealdb_errno::ERR_WRONG_OBJECT,
                        format!("'{schema}.{name}' is not VIEW"),
                    )
                })?;
                self.schema = vec!["View".to_string(), "Create View".to_string()];
                self.rows = vec![vec![
                    Datum::Bytes(name.as_bytes().to_vec()),
                    Datum::Bytes(render_create_view(info, view).into_bytes()),
                ]];
            }
            ShowStmtKind::Columns => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                let full = self.stmt.full;
                self.schema = if full {
                    vec![
                        "Field".to_string(),
                        "Type".to_string(),
                        "Collation".to_string(),
                        "Null".to_string(),
                        "Key".to_string(),
                        "Default".to_string(),
                        "Extra".to_string(),
                        "Privileges".to_string(),
                        "Comment".to_string(),
                    ]
                } else {
                    vec![
                        "Field".to_string(),
                        "Type".to_string(),
                        "Null".to_string(),
                        "Key".to_string(),
                        "Default".to_string(),
                        "Extra".to_string(),
                    ]
                };
                self.rows = info
                    .columns
                    .iter()
                    .map(|col| column_row(col, full))
                    .collect();
            }
            ShowStmtKind::Index => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                self.schema = vec![
                    "Table".to_string(),
                    "Non_unique".to_string(),
                    "Key_name".to_string(),
                    "Seq_in_index".to_string(),
                    "Column_name".to_string(),
                    "Collation".to_string(),
                    "Cardinality".to_string(),
                    "Sub_part".to_string(),
                    "Packed".to_string(),
                    "Null".to_string(),
                    "Index_type".to_string(),
                    "Comment".to_string(),
                    "Index_comment".to_string(),
                ];
                self.rows = info
                    .indices
                    .iter()
                    .flat_map(|idx| {
                        idx.columns.iter().enumerate().map(|(seq, col)| {
                            vec![
                                Datum::Bytes(info.name.o().as_bytes().to_vec()),
                                Datum::Bytes(if idx.unique || idx.primary {
                                    b"0".to_vec()
                                } else {
                                    b"1".to_vec()
                                }),
                                Datum::Bytes(idx.name.o().as_bytes().to_vec()),
                                Datum::I64((seq + 1) as i64),
                                Datum::Bytes(col.name.o().as_bytes().to_vec()),
                                Datum::Bytes(b"A".to_vec()),
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Bytes(b"YES".to_vec()),
                                Datum::Bytes(b"BTREE".to_vec()),
                                Datum::Bytes(Vec::new()),
                                Datum::Bytes(Vec::new()),
                            ]
                        })
                    })
                    .collect();
            }
            ShowStmtKind::Charset => {
                let (_, rows) = sealdb_infoschema::tables::virtual_rows("character_sets", &is)?;
                self.schema = vec![
                    "Charset".to_string(),
                    "Description".to_string(),
                    "Default collation".to_string(),
                    "Maxlen".to_string(),
                ];
                self.rows = rows
                    .into_iter()
                    .map(|row| {
                        vec![
                            row[0].clone(),
                            row[2].clone(),
                            row[1].clone(),
                            row[3].clone(),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::Collation => {
                let (_, rows) = sealdb_infoschema::tables::virtual_rows("collations", &is)?;
                self.schema = vec![
                    "Collation".to_string(),
                    "Charset".to_string(),
                    "Id".to_string(),
                    "Default".to_string(),
                    "Compiled".to_string(),
                    "Sortlen".to_string(),
                ];
                self.rows = rows
                    .into_iter()
                    .map(|row| row.into_iter().take(6).collect())
                    .collect();
            }
            ShowStmtKind::DdlJobs => {
                self.schema = vec![
                    "JOB_ID".to_string(),
                    "DB_NAME".to_string(),
                    "TABLE_NAME".to_string(),
                    "JOB_TYPE".to_string(),
                    "STATE".to_string(),
                    "SCHEMA_STATE".to_string(),
                ];
                self.rows = self
                    .domain
                    .ddl_jobs()
                    .into_iter()
                    .map(|job| {
                        vec![
                            Datum::I64(job.id),
                            Datum::Bytes(job.schema_name.into_bytes()),
                            Datum::Bytes(job.table_name.into_bytes()),
                            Datum::Bytes(job.tp.into_bytes()),
                            Datum::Bytes(job.state.as_str().as_bytes().to_vec()),
                            Datum::Bytes(job.schema_state.as_str().as_bytes().to_vec()),
                        ]
                    })
                    .collect();
            }
            ShowStmtKind::Bindings => {
                self.schema = vec![
                    "Original_sql".to_string(),
                    "Bind_sql".to_string(),
                    "Default_db".to_string(),
                    "Status".to_string(),
                    "Source".to_string(),
                ];
                self.rows = self
                    .domain
                    .bind_handle()
                    .all_bindings()
                    .into_iter()
                    .map(|b| {
                        vec![
                            Datum::Bytes(b.original_sql.into_bytes()),
                            Datum::Bytes(b.bind_sql.into_bytes()),
                            Datum::Bytes(b.db.into_bytes()),
                            Datum::Bytes(b.status.into_bytes()),
                            Datum::Bytes(b.source.into_bytes()),
                        ]
                    })
                    .collect();
            }
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn column_row(col: &ColumnInfo, full: bool) -> Vec<Datum> {
    let type_name = col.field_type.type_name();
    let mut row = vec![
        Datum::Bytes(col.name.o().as_bytes().to_vec()),
        Datum::Bytes(type_name.clone().into_bytes()),
    ];
    if full {
        let collation = if type_name.starts_with("varchar") || type_name.starts_with("char") {
            Datum::Bytes(b"utf8mb4_bin".to_vec())
        } else {
            Datum::Null
        };
        row.push(collation);
    }
    row.extend([
        Datum::Bytes(b"YES".to_vec()),
        Datum::Bytes(Vec::new()),
        Datum::Null,
        Datum::Bytes(Vec::new()),
    ]);
    if full {
        row.push(Datum::Bytes(b"select,insert,update,references".to_vec()));
        row.push(Datum::Bytes(Vec::new()));
    }
    row
}

fn render_create_table(info: &TableInfo) -> String {
    // MySQL NotNullFlag — omit DEFAULT NULL when set.
    const NOT_NULL_FLAG: u32 = 1;
    let mut cols: Vec<String> = info
        .columns
        .iter()
        .map(|col| {
            let mut line = format!("  `{}` {}", col.name.o(), col.field_type.type_name());
            if col.field_type.flag & NOT_NULL_FLAG == 0 {
                line.push_str(" DEFAULT NULL");
            }
            line
        })
        .collect();
    for idx in &info.indices {
        let key = if idx.primary {
            "PRIMARY KEY"
        } else if idx.unique {
            "UNIQUE KEY"
        } else {
            "KEY"
        };
        let parts = idx
            .columns
            .iter()
            .map(|c| format!("`{}`", c.name.o()))
            .collect::<Vec<_>>()
            .join(", ");
        cols.push(format!("  {key} `{}` ({parts})", idx.name.o()));
    }
    let mut out = format!(
        "CREATE TABLE `{}` (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET={} COLLATE={}",
        info.name.o(),
        cols.join(",\n"),
        info.charset,
        info.collate
    );
    if let Some(ttl) = &info.ttl {
        let enable = if ttl.enable { "ON" } else { "OFF" };
        out.push_str(&format!(
            " /*T![ttl] TTL={} */ /*T![ttl] TTL_ENABLE='{enable}' */ /*T![ttl] TTL_JOB_INTERVAL='{}' */",
            ttl.expr, ttl.job_interval
        ));
    }
    if let Some(part) = &info.partition {
        out.push('\n');
        out.push_str(&render_partition_clause(part));
    }
    out
}

fn render_partition_clause(part: &sealdb_meta::PartitionInfo) -> String {
    use sealdb_meta::PartitionType;
    let key = if part.columns.len() > 1
        || (part.tp == PartitionType::Range && !part.columns.is_empty() && part.expr.contains(','))
    {
        let cols = part
            .columns
            .iter()
            .map(|c| format!("`{}`", c.o()))
            .collect::<Vec<_>>()
            .join(",");
        format!("RANGE COLUMNS({cols})")
    } else if part.tp == PartitionType::Range && part.columns.len() == 1 {
        // TiDB RANGE COLUMNS(single) still prints COLUMNS when created as such.
        // Heuristic: expr equals column name → RANGE COLUMNS(`col`) for COLUMNS form
        // stored via with_columns.
        format!("RANGE COLUMNS(`{}`)", part.columns[0].o())
    } else {
        match part.tp {
            PartitionType::Range => format!("RANGE (`{}`)", part.expr),
            PartitionType::Hash => format!("HASH ({})", part.expr),
            PartitionType::List => format!("LIST (`{}`)", part.expr),
            PartitionType::Key => format!("KEY (`{}`)", part.expr),
        }
    };
    let mut defs = Vec::new();
    for def in &part.definitions {
        let bound = if !def.in_values.is_empty() {
            let vals = def
                .in_values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("VALUES IN ({vals})")
        } else {
            let lt = def
                .less_than
                .iter()
                .map(|v| v.clone())
                .collect::<Vec<_>>()
                .join(",");
            format!("VALUES LESS THAN ({lt})")
        };
        defs.push(format!("PARTITION `{}` {bound}", def.name.o()));
    }
    format!("PARTITION BY {key}\n({})", defs.join(","))
}

fn render_create_view(info: &TableInfo, view: &sealdb_meta::ViewInfo) -> String {
    let cols = if view.cols.is_empty() {
        String::new()
    } else {
        let list = view
            .cols
            .iter()
            .map(|c| format!("`{}`", c.o()))
            .collect::<Vec<_>>()
            .join(",");
        format!(" ({list})")
    };
    format!(
        "CREATE ALGORITHM={} DEFINER={} SQL SECURITY {} VIEW `{}`{} AS {}",
        view.algorithm,
        view.definer,
        view.security,
        info.name.o(),
        cols,
        view.select_stmt
    )
}

fn resolve_table(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}

/// TiDB SHOW LIKE header: `Tables_in_db (pat)` / `Database (pat)`.
fn show_column_header(base: &str, pattern: Option<&str>) -> String {
    match pattern {
        Some(pat) => format!("{base} ({pat})"),
        None => base.to_string(),
    }
}

fn show_like_ok(name: &str, pattern: Option<&str>) -> bool {
    match pattern {
        None => true,
        Some(pat) => show_like_match(name.as_bytes(), pat.as_bytes()),
    }
}

/// MySQL LIKE (`%` / `_`), case-insensitive for identifier SHOW filters.
fn show_like_match(text: &[u8], pattern: &[u8]) -> bool {
    show_like_at(text, 0, pattern, 0)
}

fn show_like_at(text: &[u8], ti: usize, pattern: &[u8], pi: usize) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }
    match pattern[pi] {
        b'%' => {
            show_like_at(text, ti, pattern, pi + 1)
                || (ti < text.len() && show_like_at(text, ti + 1, pattern, pi))
        }
        b'_' => ti < text.len() && show_like_at(text, ti + 1, pattern, pi + 1),
        ch => {
            ti < text.len()
                && text[ti].eq_ignore_ascii_case(&ch)
                && show_like_at(text, ti + 1, pattern, pi + 1)
        }
    }
}

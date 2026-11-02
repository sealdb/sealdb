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
                self.schema = vec!["Database".to_string()];
                self.rows = is
                    .all_schemas()
                    .into_iter()
                    .map(|db| vec![Datum::Bytes(db.name.o().as_bytes().to_vec())])
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
                self.schema = vec![format!("Tables_in_{db}")];
                self.rows = is
                    .schema_tables(&db)
                    .into_iter()
                    .map(|t| vec![Datum::Bytes(t.name.o().as_bytes().to_vec())])
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
                self.schema = vec!["Database".to_string(), "Create Database".to_string()];
                self.rows = vec![vec![
                    Datum::Bytes(db.as_bytes().to_vec()),
                    Datum::Bytes(format!("CREATE DATABASE `{db}`").into_bytes()),
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
    let mut cols: Vec<String> = info
        .columns
        .iter()
        .map(|col| format!("  `{}` {}", col.name.o(), col.field_type.type_name()))
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
    format!(
        "CREATE TABLE `{}` (\n{}\n)",
        info.name.o(),
        cols.join(",\n")
    )
}

fn resolve_table(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}

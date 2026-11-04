//! BRIE MVB: `BACKUP` / `RESTORE DATABASE … TO/FROM 'path'`.
//!
//! Local filesystem (`file://` optional) for backup + restore. `RESTORE` also
//! accepts `http://` / `https://` JSON dumps. S3/GCS/HDFS and TiKV BR agent
//! remain deferred.

use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_UNSUPPORTED, Error, Result};
use sealdb_executor::QueryResult;
use sealdb_types::Datum;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use super::Session;

const BACKUP_VERSION: u32 = 1;

#[derive(Clone, Debug)]
enum BrieLocation {
    Local(String),
    Http(String),
}

#[derive(Clone, Debug)]
enum BrieOp {
    Backup {
        database: String,
        location: BrieLocation,
    },
    Restore {
        location: BrieLocation,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupFile {
    version: u32,
    databases: Vec<BackupDatabase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupDatabase {
    name: String,
    tables: Vec<BackupTable>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupTable {
    name: String,
    create_sql: String,
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
}

/// Execute `BACKUP` / `RESTORE` against a local path or HTTP restore URL.
pub fn execute_brie(session: &Session, raw_sql: &str) -> Result<QueryResult> {
    let plan = parse_brie(raw_sql)?;
    match plan {
        BrieOp::Backup {
            database,
            location,
        } => match location {
            BrieLocation::Local(path) => backup_database(session, &database, &path),
            BrieLocation::Http(url) => Err(Error::new(
                ERR_UNSUPPORTED,
                format!("BACKUP to remote URL '{url}' is not available; use a local path"),
            )),
        },
        BrieOp::Restore { location } => restore_database(session, &location),
    }
}

fn parse_brie(raw_sql: &str) -> Result<BrieOp> {
    let sql = raw_sql.trim().trim_end_matches(';');
    let upper = sql.to_ascii_uppercase();
    if upper.starts_with("BACKUP") {
        let rest = sql[6..].trim_start();
        let rest_upper = rest.to_ascii_uppercase();
        if !rest_upper.starts_with("DATABASE") {
            return Err(Error::new(
                ERR_UNSUPPORTED,
                "only BACKUP DATABASE is supported in this phase",
            ));
        }
        let after_db = rest[8..].trim_start();
        let (db, after_name) = take_ident_or_star(after_db)?;
        let after_name = after_name.trim_start();
        let after_upper = after_name.to_ascii_uppercase();
        if !after_upper.starts_with("TO") {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "BACKUP DATABASE requires TO"));
        }
        let path = take_quoted_path(after_name[2..].trim_start())?;
        let location = parse_brie_location(&path)?;
        Ok(BrieOp::Backup {
            database: db,
            location,
        })
    } else if upper.starts_with("RESTORE") {
        let rest = sql[7..].trim_start();
        let rest_upper = rest.to_ascii_uppercase();
        let after = if rest_upper.starts_with("DATABASE") {
            let after_db = rest[8..].trim_start();
            if after_db.to_ascii_uppercase().starts_with("FROM") {
                after_db
            } else {
                let (_, after_name) = take_ident_or_star(after_db)?;
                after_name.trim_start()
            }
        } else {
            rest
        };
        let after_upper = after.to_ascii_uppercase();
        if !after_upper.starts_with("FROM") {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "RESTORE requires FROM"));
        }
        let path = take_quoted_path(after[4..].trim_start())?;
        let location = parse_brie_location(&path)?;
        Ok(BrieOp::Restore { location })
    } else {
        Err(Error::new(
            ERR_UNSUPPORTED,
            "expected BACKUP or RESTORE statement",
        ))
    }
}

fn take_ident_or_star(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    if s.starts_with('*') {
        return Ok(("*".into(), &s[1..]));
    }
    let mut end = 0;
    let bytes = s.as_bytes();
    if bytes.first() == Some(&b'`') {
        end = 1;
        while end < bytes.len() && bytes[end] != b'`' {
            end += 1;
        }
        if end >= bytes.len() {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "unterminated identifier"));
        }
        let name = s[1..end].to_string();
        return Ok((name, &s[end + 1..]));
    }
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end == 0 {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "expected database name"));
    }
    Ok((s[..end].to_string(), &s[end..]))
}

fn take_quoted_path(s: &str) -> Result<String> {
    let s = s.trim_start();
    let quote = s.as_bytes().first().copied();
    if quote != Some(b'\'') && quote != Some(b'"') {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            "BACKUP/RESTORE path must be a quoted string",
        ));
    }
    let q = quote.unwrap() as char;
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
            continue;
        }
        if c == q {
            return Ok(out);
        }
        out.push(c);
    }
    Err(Error::new(ERR_UNKNOWN_ERROR, "unterminated path string"))
}

fn parse_brie_location(path: &str) -> Result<BrieLocation> {
    let p = path.trim();
    if p.starts_with("http://") || p.starts_with("https://") {
        return Ok(BrieLocation::Http(p.to_string()));
    }
    if p.starts_with("s3://") || p.starts_with("gs://") || p.starts_with("hdfs://") {
        return Err(Error::new(
            ERR_UNSUPPORTED,
            format!("remote BRIE destination '{p}' is not available in this phase"),
        ));
    }
    let p = p.strip_prefix("file://").unwrap_or(p);
    if p.is_empty() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "empty backup path"));
    }
    Ok(BrieLocation::Local(p.to_string()))
}

fn location_display(loc: &BrieLocation) -> &str {
    match loc {
        BrieLocation::Local(p) | BrieLocation::Http(p) => p,
    }
}

fn read_backup_json(location: &BrieLocation) -> Result<String> {
    match location {
        BrieLocation::Local(path) => fs::read_to_string(path)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("read backup: {e}"))),
        BrieLocation::Http(url) => {
            let resp = reqwest::blocking::get(url).map_err(|e| {
                Error::new(ERR_UNKNOWN_ERROR, format!("http backup fetch {url}: {e}"))
            })?;
            if !resp.status().is_success() {
                return Err(Error::new(
                    ERR_UNKNOWN_ERROR,
                    format!("http backup fetch {url}: status {}", resp.status()),
                ));
            }
            resp.text().map_err(|e| {
                Error::new(ERR_UNKNOWN_ERROR, format!("http backup body {url}: {e}"))
            })
        }
    }
}

fn backup_database(session: &Session, database: &str, path: &str) -> Result<QueryResult> {
    let db_names: Vec<String> = if database == "*" {
        let dbs = session.execute_stmt("SHOW DATABASES")?;
        dbs.display_rows()
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .filter(|n| {
                let u = n.to_ascii_lowercase();
                u != "information_schema"
                    && u != "mysql"
                    && u != "performance_schema"
                    && u != "sys"
            })
            .collect()
    } else {
        session
            .execute_stmt(&format!("USE `{database}`"))
            .map_err(|_| {
                Error::new(
                    sealdb_errno::ERR_BAD_DB,
                    format!("Unknown database '{database}'"),
                )
            })?;
        vec![database.to_string()]
    };

    let mut databases = Vec::new();
    for db in db_names {
        let tables_rs = session.execute_stmt(&format!("SHOW TABLES FROM `{db}`"))?;
        let table_names: Vec<String> = tables_rs
            .display_rows()
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect();
        let mut tables = Vec::new();
        for table in table_names {
            let create = session.execute_stmt(&format!("SHOW CREATE TABLE `{db}`.`{table}`"))?;
            let create_sql = create
                .display_rows()
                .first()
                .and_then(|r| r.get(1))
                .cloned()
                .unwrap_or_default();
            if create_sql.to_ascii_uppercase().contains(" VIEW ") {
                continue;
            }
            let data = session.execute_stmt(&format!("SELECT * FROM `{db}`.`{table}`"))?;
            let rows: Vec<Vec<serde_json::Value>> = data
                .rows
                .iter()
                .map(|row| row.iter().map(datum_to_json).collect())
                .collect();
            tables.push(BackupTable {
                name: table,
                create_sql,
                columns: data.columns.clone(),
                rows,
            });
        }
        databases.push(BackupDatabase { name: db, tables });
    }

    let file = BackupFile {
        version: BACKUP_VERSION,
        databases,
    };
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::new(ERR_UNKNOWN_ERROR, format!("create backup dir: {e}"))
            })?;
        }
    }
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("serialize backup: {e}")))?;
    fs::write(path, json)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("write backup: {e}")))?;
    Ok(brie_result("Backup", path, "OK"))
}

fn restore_database(session: &Session, location: &BrieLocation) -> Result<QueryResult> {
    let raw = read_backup_json(location)?;
    let file: BackupFile = serde_json::from_str(&raw)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("parse backup: {e}")))?;
    if file.version != BACKUP_VERSION {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("unsupported backup version {}", file.version),
        ));
    }
    for db in &file.databases {
        session.execute_stmt(&format!("CREATE DATABASE IF NOT EXISTS `{}`", db.name))?;
        for table in &db.tables {
            let _ = session.execute_stmt(&format!(
                "DROP TABLE IF EXISTS `{}`.`{}`",
                db.name, table.name
            ));
            let create = qualify_create_table(&table.create_sql, &db.name, &table.name);
            session.execute_stmt(&create)?;
            for row in &table.rows {
                let values = row
                    .iter()
                    .map(json_to_sql_literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                session.execute_stmt(&format!(
                    "INSERT INTO `{}`.`{}` VALUES ({values})",
                    db.name, table.name
                ))?;
            }
        }
    }
    Ok(brie_result("Restore", location_display(location), "OK"))
}

fn qualify_create_table(create_sql: &str, db: &str, table: &str) -> String {
    let trimmed = create_sql.trim();
    if let Some(idx) = trimmed.find('(') {
        return format!("CREATE TABLE `{db}`.`{table}` {}", &trimmed[idx..]);
    }
    trimmed.to_string()
}

fn datum_to_json(d: &Datum) -> serde_json::Value {
    match d {
        Datum::Null => serde_json::Value::Null,
        Datum::I64(v) => serde_json::json!(*v),
        Datum::U64(v) => serde_json::json!(*v),
        Datum::F64(v) => serde_json::json!(*v),
        Datum::Bytes(v) => serde_json::Value::String(String::from_utf8_lossy(v).into_owned()),
        Datum::Decimal(v) => serde_json::Value::String(v.to_string()),
        Datum::Time(v) => serde_json::Value::String(format!("{v:?}")),
    }
}

fn json_to_sql_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "NULL".into(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

fn brie_result(op: &str, path: &str, status: &str) -> QueryResult {
    QueryResult {
        columns: vec!["Operation".into(), "Destination".into(), "Status".into()],
        rows: vec![vec![
            Datum::Bytes(op.as_bytes().to_vec()),
            Datum::Bytes(path.as_bytes().to_vec()),
            Datum::Bytes(status.as_bytes().to_vec()),
        ]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_backup_restore() {
        let b = parse_brie("BACKUP DATABASE db1 TO '/tmp/a.json'").unwrap();
        match b {
            BrieOp::Backup {
                database,
                location: BrieLocation::Local(path),
            } => {
                assert_eq!(database, "db1");
                assert_eq!(path, "/tmp/a.json");
            }
            other => panic!("{other:?}"),
        }
        let b = parse_brie("BACKUP DATABASE * TO 'file:///tmp/b.json'").unwrap();
        match b {
            BrieOp::Backup {
                database,
                location: BrieLocation::Local(path),
            } => {
                assert_eq!(database, "*");
                assert_eq!(path, "/tmp/b.json");
            }
            other => panic!("{other:?}"),
        }
        let r = parse_brie("RESTORE DATABASE FROM '/tmp/a.json'").unwrap();
        match r {
            BrieOp::Restore {
                location: BrieLocation::Local(path),
            } => assert_eq!(path, "/tmp/a.json"),
            other => panic!("{other:?}"),
        }
        let r = parse_brie("RESTORE DATABASE FROM 'https://example.com/b.json'").unwrap();
        match r {
            BrieOp::Restore {
                location: BrieLocation::Http(url),
            } => assert_eq!(url, "https://example.com/b.json"),
            other => panic!("{other:?}"),
        }
        let err = parse_brie("BACKUP DATABASE db1 TO 's3://bucket/x'").unwrap_err();
        assert_eq!(err.code, ERR_UNSUPPORTED);
        let err = parse_brie("BACKUP DATABASE db1 TO 'http://example.com/x.json'").unwrap();
        match err {
            BrieOp::Backup {
                location: BrieLocation::Http(_),
                ..
            } => {}
            other => panic!("expected http backup location: {other:?}"),
        }
    }

    #[test]
    fn test_read_backup_json_local_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "sealdb-brie-http-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.json");
        let body = r#"{"version":1,"databases":[]}"#;
        fs::write(&path, body).unwrap();
        let got = read_backup_json(&BrieLocation::Local(path.to_string_lossy().into())).unwrap();
        assert_eq!(got, body);
        let _ = fs::remove_dir_all(&dir);
    }
}

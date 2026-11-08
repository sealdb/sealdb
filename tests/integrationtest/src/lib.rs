//! TiDB-style SQL integration tester (`tests/integrationtest`).
//!
//! Reads `t/**/*.test`, executes each statement through `Session`, and
//! compares against `r/**/*.result` in mysql-tester form.
//!
//! Suite selection is gated by `project/regression/manifest.json`:
//! - default / CI: `green` + `local`
//! - `SEALDB_ITEST_RUN_IN_PROGRESS=1`: also `in_progress`
//! - `SEALDB_ITEST_RUN_PENDING=1`: also `pending` (expect failures; TDD triage)
//! - `deferred` never runs unless `--status deferred` explicitly
//!
//! Authoritative goldens come from TiDB (`project/scripts/record_tidb_itest.sh`).
//! SealDB `--record` requires `SEALDB_ITEST_ALLOW_SEALDB_RECORD=1` and refuses
//! non-`local/` suites.

use sealdb_config::Config;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_session::Session;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreBackend {
    Memory,
    Tikv,
}

impl StoreBackend {
    pub fn parse_list(raw: &str) -> Result<Vec<Self>> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "both" | "all" => Ok(vec![Self::Memory, Self::Tikv]),
            "memory" | "mem" => Ok(vec![Self::Memory]),
            "tikv" => Ok(vec![Self::Tikv]),
            other => Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("unknown store {other:?}; expected memory, tikv, or both"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Tikv => "tikv",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub record: bool,
    pub filter: Option<String>,
    pub stores: Vec<StoreBackend>,
    pub require_tikv: bool,
    pub pd: Option<String>,
    /// If set, only suites whose manifest status is in this list.
    pub statuses: Option<Vec<String>>,
    /// If set, only suites whose manifest wave is in this list (e.g. R1).
    pub waves: Option<Vec<String>>,
    /// When true, missing golden is a hard error (default). When false, skip.
    pub require_golden: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            record: false,
            filter: None,
            stores: vec![StoreBackend::Memory, StoreBackend::Tikv],
            require_tikv: require_tikv_from_env(),
            pd: None,
            statuses: None,
            waves: None,
            require_golden: true,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub memory: usize,
    pub tikv: usize,
    pub tikv_skipped: bool,
    pub skipped_suites: usize,
}

impl RunReport {
    pub fn ran(&self) -> usize {
        self.memory + self.tikv
    }
}

#[derive(Clone, Debug)]
pub struct Step {
    pub sql: String,
    pub expected_error: Option<i32>,
    pub sorted: bool,
    /// When true, append session warnings after the result (TiDB `--enable_warnings`).
    pub show_warnings: bool,
    /// TiDB `--echo …` — emit this line into the result (no SQL execution).
    pub echo: Option<String>,
    /// TiDB `--disable_result_log`: echo the statement but suppress its rows.
    pub log_result: bool,
    /// TiDB `--enable_info`: print `affected rows:` and the OK-packet info string.
    pub show_info: bool,
    /// TiDB `--replace_regex /pat/repl/` applied to this statement's result.
    pub replace_regex: Vec<(regex::Regex, String)>,
    /// TiDB `--replace_column N text …` (1-based): overwrite those result
    /// columns, e.g. the memory/disk numbers of `EXPLAIN ANALYZE`.
    pub replace_column: Vec<(usize, String)>,
    /// mysql-tester session directive (`connect` / `connection` / `disconnect`).
    pub directive: Option<SessionDirective>,
}

/// mysql-tester connection directives. They emit no result rows, so the golden
/// file only shows the statements that run on the named connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionDirective {
    /// `connect (name, host, user, pass, db)`
    Connect(String),
    /// `connection name`
    Connection(String),
    /// `disconnect name`
    Disconnect(String),
}

/// Parse a bare `connect` / `connection` / `disconnect` line. Returns `None`
/// for ordinary SQL so the statement buffer path keeps working.
fn parse_session_directive(line: &str) -> Option<SessionDirective> {
    fn name_of(raw: &str) -> Option<String> {
        let raw = raw.trim();
        let raw = raw.strip_prefix('(').unwrap_or(raw);
        let name = raw
            .split([',', ')', ';', ' ', '\t'])
            .find(|t| !t.is_empty())?
            .trim_matches(['\'', '"']);
        (!name.is_empty()).then(|| name.to_string())
    }
    let line = line.trim();
    for (keyword, make) in [
        (
            "connection",
            SessionDirective::Connection as fn(String) -> SessionDirective,
        ),
        ("disconnect", SessionDirective::Disconnect),
        ("connect", SessionDirective::Connect),
    ] {
        let Some(rest) = strip_prefix_ci(line, keyword) else {
            continue;
        };
        // `connect` must be a whole word (`connection` is matched above).
        if rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        return name_of(rest).map(make);
    }
    None
}

#[derive(Clone, Debug)]
pub struct Suite {
    pub name: String,
    pub test_path: PathBuf,
    pub result_path: PathBuf,
    pub steps: Vec<Step>,
    pub status: String,
    pub wave: String,
    pub owner_hint: String,
    pub skip_tikv: bool,
    /// Set when `t/*.test` uses directives SealDB cannot parse yet.
    pub load_error: Option<String>,
}
#[derive(Debug, Deserialize)]
struct ManifestFile {
    suites: Vec<ManifestSuite>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestSuite {
    pub name: String,
    pub wave: String,
    pub status: String,
    #[serde(default)]
    pub owner_hint: String,
    /// When true, suite is skipped on TiKV (e.g. SAVEPOINT until driver parity).
    #[serde(default)]
    pub skip_tikv: bool,
}
pub fn testdata_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn workspace_root() -> PathBuf {
    testdata_root()
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn manifest_path() -> PathBuf {
    workspace_root().join("project/regression/manifest.json")
}

pub fn load_manifest(path: &Path) -> Result<HashMap<String, ManifestSuite>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
    let doc: ManifestFile = serde_json::from_str(&raw)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, format!("manifest: {e}")))?;
    Ok(doc
        .suites
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect())
}

fn default_status_allowlist() -> HashSet<String> {
    let mut set = HashSet::from(["green".to_string(), "local".to_string()]);
    if env_flag("SEALDB_ITEST_RUN_IN_PROGRESS") {
        set.insert("in_progress".to_string());
    }
    if env_flag("SEALDB_ITEST_RUN_PENDING") {
        set.insert("pending".to_string());
    }
    if env_flag("SEALDB_ITEST_RUN_DEFERRED") {
        set.insert("deferred".to_string());
    }
    set
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

pub fn parse_test(src: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut buf = String::new();
    let mut expected_error = None;
    let mut sorted = false;
    let mut warnings_enabled = false;
    let mut result_log = true;
    let mut info_enabled = false;
    let mut replace_regex: Vec<(regex::Regex, String)> = Vec::new();
    let mut replace_column: Vec<(usize, String)> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        // TiDB mysql-tester drops blank lines inside multi-line statements, so
        // the echoed statement (and golden) has no interior empty lines.
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = strip_prefix_ci(trimmed, "--") {
            let rest = rest.trim();
            if let Some(codes) = strip_prefix_ci(rest, "error") {
                expected_error = parse_error_code(codes.trim())?;
                continue;
            }
            if rest.eq_ignore_ascii_case("sorted_result") {
                sorted = true;
                continue;
            }
            if rest.eq_ignore_ascii_case("disable_result_log") {
                result_log = false;
                continue;
            }
            if rest.eq_ignore_ascii_case("enable_result_log") {
                result_log = true;
                continue;
            }
            if let Some(spec) = strip_prefix_ci(rest, "replace_regex") {
                replace_regex = parse_replace_regex(spec.trim())?;
                continue;
            }
            if let Some(spec) = strip_prefix_ci(rest, "replace_column") {
                replace_column = parse_replace_column(spec.trim())?;
                continue;
            }
            if rest.eq_ignore_ascii_case("enable_info") {
                info_enabled = true;
                continue;
            }
            if rest.eq_ignore_ascii_case("disable_info") {
                info_enabled = false;
                continue;
            }
            if rest.eq_ignore_ascii_case("enable_warnings") {
                warnings_enabled = true;
                continue;
            }
            if rest.eq_ignore_ascii_case("disable_warnings") {
                warnings_enabled = false;
                continue;
            }
            if let Some(msg) = strip_prefix_ci(rest, "echo") {
                steps.push(Step {
                    sql: String::new(),
                    expected_error: None,
                    sorted: false,
                    show_warnings: false,
                    echo: Some(msg.trim_start().to_string()),
                    log_result: true,
                    show_info: false,
                    replace_regex: Vec::new(),
                    replace_column: Vec::new(),
                    directive: None,
                });
                continue;
            }
            // TiDB directives we do not yet honor: skip with clear error on use.
            if rest.eq_ignore_ascii_case("enable_query_log")
                || rest.eq_ignore_ascii_case("disable_query_log")
                || strip_prefix_ci(rest, "sleep").is_some()
            {
                return Err(Error::unsupported(format!("test directive -- {rest}")));
            }
            return Err(Error::unsupported(format!("test directive -- {rest}")));
        }
        // Session directives are standalone lines that emit no rows.
        if buf.trim().is_empty() {
            if let Some(directive) = parse_session_directive(trimmed) {
                steps.push(Step {
                    sql: String::new(),
                    expected_error: None,
                    sorted: false,
                    show_warnings: false,
                    echo: None,
                    log_result: true,
                    show_info: false,
                    replace_regex: Vec::new(),
                    replace_column: Vec::new(),
                    directive: Some(directive),
                });
                continue;
            }
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        // TiDB mysql-tester strips leading whitespace on each SQL line in
        // result files (CREATE TABLE columns appear flush-left).
        buf.push_str(trimmed);
        if trimmed.ends_with(';') {
            steps.push(Step {
                sql: buf.trim().to_string(),
                expected_error,
                sorted,
                show_warnings: warnings_enabled,
                echo: None,
                log_result: result_log,
                show_info: info_enabled,
                replace_regex: std::mem::take(&mut replace_regex),
                replace_column: std::mem::take(&mut replace_column),
                directive: None,
            });
            buf.clear();
            expected_error = None;
            sorted = false;
        }
    }
    // mysql-tester executes the trailing buffer at EOF, so a `.test` file may
    // end without `;` (`explain_complex_stats` ends on `set @@sql_mode=default`).
    if !buf.trim().is_empty() {
        steps.push(Step {
            sql: buf.trim().to_string(),
            expected_error,
            sorted,
            show_warnings: warnings_enabled,
            echo: None,
            log_result: result_log,
            show_info: info_enabled,
            replace_regex,
            replace_column,
            directive: None,
        });
    }
    Ok(steps)
}

/// Parse `--replace_regex /pat/repl/ [/pat/repl/ …]` into (regex, replacement) pairs.
fn parse_replace_regex(spec: &str) -> Result<Vec<(regex::Regex, String)>> {
    let chars: Vec<char> = spec.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] != '/' {
            return Err(Error::unsupported(format!(
                "test directive -- replace_regex {spec}"
            )));
        }
        i += 1;
        let mut pattern = String::new();
        while i < chars.len() && chars[i] != '/' {
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '/' {
                pattern.push('/');
                i += 2;
                continue;
            }
            pattern.push(chars[i]);
            i += 1;
        }
        if i >= chars.len() {
            return Err(Error::unsupported(format!(
                "test directive -- replace_regex {spec}"
            )));
        }
        i += 1;
        let mut replacement = String::new();
        while i < chars.len() && chars[i] != '/' {
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '/' {
                replacement.push('/');
                i += 2;
                continue;
            }
            replacement.push(chars[i]);
            i += 1;
        }
        if i < chars.len() {
            i += 1;
        }
        let re = regex::Regex::new(&pattern).map_err(|e| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("replace_regex pattern {pattern:?}: {e}"),
            )
        })?;
        out.push((re, replacement));
    }
    Ok(out)
}

/// Parse `--replace_column 6 <execution_info> 8 <memory>` into 1-based column
/// indexes paired with the placeholder that replaces their value.
fn parse_replace_column(spec: &str) -> Result<Vec<(usize, String)>> {
    let tokens: Vec<&str> = spec.split_whitespace().collect();
    if tokens.is_empty() || !tokens.len().is_multiple_of(2) {
        return Err(Error::unsupported(format!(
            "test directive -- replace_column {spec}"
        )));
    }
    let mut out = Vec::new();
    for pair in tokens.chunks(2) {
        let index: usize = pair[0]
            .parse()
            .map_err(|_| Error::unsupported(format!("test directive -- replace_column {spec}")))?;
        if index == 0 {
            return Err(Error::unsupported(format!(
                "test directive -- replace_column {spec}"
            )));
        }
        out.push((index - 1, pair[1].to_string()));
    }
    Ok(out)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // Slice by byte length, but only on a char boundary so non-ASCII SQL
    // (e.g. identifiers starting with an emoji) cannot panic.
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        s.get(prefix.len()..)
    } else {
        None
    }
}

fn parse_error_code(raw: &str) -> Result<Option<i32>> {
    let first = raw.split([',', ' ']).find(|s| !s.is_empty()).unwrap_or(raw);
    first
        .parse::<i32>()
        .map(Some)
        .map_err(|_| Error::new(sealdb_errno::ERR_PARSE_ERROR, format!("bad --error {raw}")))
}

pub fn load_suites(root: &Path) -> Result<Vec<Suite>> {
    let t_dir = root.join("t");
    if !t_dir.is_dir() {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("missing {}", t_dir.display()),
        ));
    }
    let manifest = load_manifest(&manifest_path())?;
    let mut suites = Vec::new();
    collect_tests(&t_dir, &t_dir, root, &manifest, &mut suites)?;
    suites.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(suites)
}

fn collect_tests(
    dir: &Path,
    t_root: &Path,
    root: &Path,
    manifest: &HashMap<String, ManifestSuite>,
    out: &mut Vec<Suite>,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_tests(&path, t_root, root, manifest, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("test") {
            continue;
        }
        let rel = path.strip_prefix(t_root).unwrap().with_extension("result");
        let name = rel.with_extension("").to_string_lossy().replace('\\', "/");
        let (status, wave, owner_hint, skip_tikv) = if let Some(m) = manifest.get(&name) {
            (
                m.status.clone(),
                m.wave.clone(),
                m.owner_hint.clone(),
                m.skip_tikv,
            )
        } else if name.starts_with("local/") {
            (
                "local".to_string(),
                "local".to_string(),
                "sealdb smoke".to_string(),
                name == "local/txn",
            )
        } else {
            (
                "pending".to_string(),
                "R7".to_string(),
                String::new(),
                false,
            )
        };
        let src = fs::read_to_string(&path)
            .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
        let (steps, load_error) = match parse_test(&src) {
            Ok(s) => (s, None),
            Err(err) => (Vec::new(), Some(format!("suite {name}: {}", err.message))),
        };
        out.push(Suite {
            name,
            test_path: path,
            result_path: root.join("r").join(rel),
            steps,
            status,
            wave,
            owner_hint,
            skip_tikv,
            load_error,
        });
    }
    Ok(())
}

fn suite_allowed(suite: &Suite, opts: &RunOptions) -> bool {
    if let Some(filter) = opts.filter.as_deref() {
        // Exact name, or path suffix when filter contains `/`.
        // Bare `-t index_join` must not also match `globalindex/index_join`.
        let name = suite.name.as_str();
        let ok = if filter.contains('/') {
            name == filter || name.ends_with(filter) || name.ends_with(&format!("/{filter}"))
        } else {
            name == filter
        };
        if !ok {
            return false;
        }
    }
    if let Some(waves) = opts.waves.as_ref()
        && !waves.iter().any(|w| w.eq_ignore_ascii_case(&suite.wave))
    {
        return false;
    }
    let allow = opts
        .statuses
        .clone()
        .map(|v| v.into_iter().collect::<HashSet<_>>())
        .unwrap_or_else(default_status_allowlist);
    allow.contains(&suite.status)
}

fn require_tikv_from_env() -> bool {
    env_flag("SEALDB_ITEST_REQUIRE_TIKV")
}

fn unique_id_base() -> i64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    ((nanos.wrapping_add(seq.wrapping_mul(1_000_003))) % 1_000_000_000_000) as i64
}

fn open_storage(backend: StoreBackend, pd: Option<&str>) -> Result<Arc<dyn sealdb_kv::Storage>> {
    let cfg = Config::default()
        .with_store_name(Some(backend.as_str()))?
        .with_pd(pd);
    sealdb_store::open(&cfg)
}

fn new_session(storage: Arc<dyn sealdb_kv::Storage>) -> Session {
    Session::with_domain(Arc::new(
        Domain::with_storage_id_base(storage, unique_id_base()).expect("domain meta load"),
    ))
}

pub fn run_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<String> {
    if let Some(err) = suite.load_error.as_ref() {
        return Err(Error::unsupported(err.clone()));
    }
    let session = new_session(storage);
    // TiDB mysql-tester: database name = suite path with `/` → `__`
    // (`show` → `show`, `types/const` → `types__const`).
    let db = suite.name.replace('/', "__");
    // TiDB runs each test file against a fresh instance, so suites assume an
    // empty database and skip `DROP TABLE`. TiKV storage is persistent, so
    // recreate the suite database rather than inheriting an aborted run's tables.
    let _ = session.execute_stmt(&format!("DROP DATABASE IF EXISTS `{db}`"));
    let _ = session.execute_stmt(&format!("CREATE DATABASE IF NOT EXISTS `{db}`"));
    let _ = session.execute_stmt(&format!("USE `{db}`"));
    // `connect (conn1, …)` opens an extra session; all sessions share the same
    // Domain, so `SET GLOBAL` values (and storage) stay visible across them.
    let domain = session.domain();
    let mut sessions: std::collections::HashMap<String, Session> = std::collections::HashMap::new();
    sessions.insert("default".to_string(), session);
    let mut current = "default".to_string();
    let mut chunks = Vec::new();
    for step in &suite.steps {
        if let Some(directive) = &step.directive {
            match directive {
                SessionDirective::Connect(name) => {
                    sessions
                        .entry(name.clone())
                        .or_insert_with(|| Session::with_domain(Arc::clone(&domain)));
                    current = name.clone();
                }
                SessionDirective::Connection(name) => {
                    if !sessions.contains_key(name) {
                        return Err(Error::new(
                            sealdb_errno::ERR_UNKNOWN_ERROR,
                            format!("connection '{name}' is not open"),
                        ));
                    }
                    current = name.clone();
                }
                SessionDirective::Disconnect(name) => {
                    sessions.remove(name);
                    if current == *name {
                        current = "default".to_string();
                    }
                }
            }
            continue;
        }
        let session = sessions.get(&current).ok_or_else(|| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("connection '{current}' is not open"),
            )
        })?;
        chunks.push(run_step(session, step)?);
    }
    let mut out = chunks.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// `--sorted_result` ordering: mysql-tester compares raw cell bytes, and a SQL
/// NULL is a nil slice, so it sorts before every non-NULL value (and before the
/// empty string). Rendered `"NULL"` cells are therefore ordered first, with the
/// remaining cells compared bytewise.
fn sort_rows(rows: &mut [Vec<String>]) {
    fn cell_key(cell: &str) -> Option<&str> {
        (cell != "NULL").then_some(cell)
    }
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b.iter()) {
            match (cell_key(x), cell_key(y)) {
                (None, None) => continue,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => match x.as_bytes().cmp(y.as_bytes()) {
                    std::cmp::Ordering::Equal => continue,
                    other => return other,
                },
            }
        }
        a.len().cmp(&b.len())
    });
}

fn run_step(session: &Session, step: &Step) -> Result<String> {
    if let Some(msg) = &step.echo {
        return Ok(msg.clone());
    }
    // `SEALDB_ITEST_TRACE=1` prints each statement before it runs, so a panic
    // or abort still names the failing SQL.
    if std::env::var_os("SEALDB_ITEST_TRACE").is_some() {
        eprintln!("[trace] {}", step.sql);
    }
    let result = session.execute_stmt(&step.sql);
    if std::env::var_os("SEALDB_ITEST_TRACE").is_some() {
        match &result {
            Ok(qr) => eprintln!("[trace]   ok rows={}", qr.rows.len()),
            Err(e) => eprintln!("[trace]   err {} {}", e.code, e.message),
        }
    }
    match (result, step.expected_error) {
        (Ok(_), Some(code)) => Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("expected error {code} for {}", step.sql),
        )),
        (Err(err), Some(code)) if err.code != code => Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "expected error {code} for {}, got {} ({})",
                step.sql, err.code, err.message
            ),
        )),
        (Ok(qr), None) => {
            if !step.log_result {
                return Ok(step.sql.clone());
            }
            let mut lines = vec![step.sql.clone()];
            if !qr.columns.is_empty() {
                lines.push(qr.columns.join("\t"));
                let mut rows = qr.display_rows();
                if step.sorted {
                    sort_rows(&mut rows);
                }
                for row in &mut rows {
                    for (index, placeholder) in &step.replace_column {
                        if let Some(cell) = row.get_mut(*index) {
                            *cell = placeholder.clone();
                        }
                    }
                }
                for row in rows {
                    lines.push(row.join("\t"));
                }
            }
            if step.show_warnings && !qr.warnings.is_empty() {
                lines.push("Level\tCode\tMessage".into());
                for (code, msg) in &qr.warnings {
                    lines.push(format!("Warning\t{code}\t{msg}"));
                }
            }
            if step.show_info {
                lines.push(format!("affected rows: {}", qr.affected_rows));
                lines.push(format!("info: {}", qr.info));
            }
            let mut out = lines.join("\n");
            for (re, replacement) in &step.replace_regex {
                out = re.replace_all(&out, replacement.as_str()).into_owned();
            }
            Ok(out)
        }
        (Err(err), _) => {
            let mut out = format!(
                "{}\nError {} ({}): {}",
                step.sql, err.code, err.sqlstate, err.message
            );
            for (re, replacement) in &step.replace_regex {
                out = re.replace_all(&out, replacement.as_str()).into_owned();
            }
            Ok(out)
        }
    }
}

fn unified_diff(expected: &str, actual: &str, limit: usize) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    let total = exp.len().max(act.len());
    // Prefer showing the first differing window even when it starts past `limit`.
    let first_diff = (0..total).find(|&i| {
        exp.get(i).copied().unwrap_or("<missing>") != act.get(i).copied().unwrap_or("<missing>")
    });
    let start = first_diff.unwrap_or(0);
    let end = (start + limit).min(total);
    for i in start..end {
        let e = exp.get(i).copied().unwrap_or("<missing>");
        let a = act.get(i).copied().unwrap_or("<missing>");
        if e != a {
            out.push_str(&format!("@@ line {} @@\n- {e}\n+ {a}\n", i + 1));
        }
    }
    if total > end {
        out.push_str(&format!(
            "... diff truncated (exp={}, act={}, showed {start}..{end})\n",
            exp.len(),
            act.len()
        ));
    }
    if out.is_empty() {
        out.push_str("(no line diff; check trailing newline/whitespace)\n");
    }
    out
}

pub fn compare_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<()> {
    let actual = run_suite(suite, storage)?;
    if let Ok(path) = std::env::var("SEALDB_ITEST_DUMP_ACTUAL") {
        let _ = fs::write(&path, &actual);
    }
    if !suite.result_path.is_file() {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "suite {} missing golden {} (record via project/scripts/record_tidb_itest.sh)",
                suite.name,
                suite.result_path.display()
            ),
        ));
    }
    let expected = fs::read_to_string(&suite.result_path).map_err(|e| {
        Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("{}: {e}", suite.result_path.display()),
        )
    })?;
    let expected_n = normalize(&expected);
    let actual_n = normalize(&actual);
    if actual_n != expected_n {
        // Triage aid: keep the full actual output next to the diff so a large
        // mismatch does not have to be reproduced statement by statement.
        if let Ok(path) = std::env::var("SEALDB_ITEST_DUMP_DIR") {
            let name = suite.name.replace('/', "__");
            let _ = fs::write(
                std::path::Path::new(&path).join(format!("{name}.actual")),
                &actual,
            );
        }
        let hint = if suite.owner_hint.is_empty() {
            String::new()
        } else {
            format!("\nowner_hint: {}\n", suite.owner_hint)
        };
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "suite {} [{}|{}] result mismatch{hint}\n--- diff ---\n{}--- expected (head) ---\n{}--- actual (head) ---\n{}",
                suite.name,
                suite.wave,
                suite.status,
                unified_diff(&expected_n, &actual_n, 40),
                head_chars(&expected, 1200),
                head_chars(&actual, 1200),
            ),
        ));
    }
    Ok(())
}

fn head_chars(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{head}\n...\n")
    } else {
        head
    }
}

pub fn record_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<()> {
    if !env_flag("SEALDB_ITEST_ALLOW_SEALDB_RECORD") {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            "SealDB --record is forbidden for authoritative goldens; \
             use project/scripts/record_tidb_itest.sh, or set \
             SEALDB_ITEST_ALLOW_SEALDB_RECORD=1 for local/ experimental only",
        ));
    }
    if !suite.name.starts_with("local/") {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "refusing to record non-local suite {} from SealDB; use TiDB recorder",
                suite.name
            ),
        ));
    }
    let actual = run_suite(suite, storage)?;
    if let Some(parent) = suite.result_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
    }
    fs::write(&suite.result_path, actual)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))
}

fn normalize(text: &str) -> String {
    let mut out = String::new();
    for line in text.replace("\r\n", "\n").lines() {
        out.push_str(&normalize_line(line));
        out.push('\n');
    }
    if out.is_empty() && !text.is_empty() {
        out.push('\n');
    }
    out
}

/// Gate B: strip unstable EXPLAIN operator id suffixes (`Projection_3` → `Projection`)
/// and normalize partition physical id lists.
fn normalize_line(line: &str) -> String {
    let Some((id, rest)) = line.split_once('\t') else {
        return line.to_string();
    };
    let Some((est, _)) = rest.split_once('\t') else {
        return line.to_string();
    };
    // Only rewrite explain/desc body rows (numeric estRows).
    if est.parse::<f64>().is_err() {
        return line.to_string();
    }
    let id = strip_explain_id_suffix(id);
    let rest = strip_embedded_op_ids(rest);
    let rest = normalize_partition_ids(&rest);
    format!("{id}\t{rest}")
}

fn strip_explain_id_suffix(id: &str) -> String {
    let mut name_start = 0;
    for (i, ch) in id.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '(' || ch == ')' {
            name_start = i;
            break;
        }
        name_start = i + ch.len_utf8();
    }
    let (prefix, name) = id.split_at(name_start);
    // `TableReader_30(Build)` → strip `_30` before the role suffix.
    let (stem, role) = match name.find('(') {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    let stem = stem
        .rsplit_once('_')
        .and_then(|(head, tail)| {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                Some(head)
            } else {
                None
            }
        })
        .unwrap_or(stem);
    format!("{prefix}{stem}{role}")
}

fn strip_embedded_op_ids(rest: &str) -> String {
    // `data:Selection_8` → `data:Selection`
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '_' {
            let mut digits = String::new();
            while let Some(d) = chars.peek().copied() {
                if d.is_ascii_digit() {
                    digits.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if digits.is_empty() {
                continue;
            }
            // Keep `_digits` unless it looks like an operator id suffix
            // (followed by tab/end/comma/space or end of token).
            let next = chars.peek().copied();
            if next.is_none_or(|n| matches!(n, '\t' | ' ' | ',' | ')' | ']')) {
                out.pop(); // drop '_'
            // drop digits (already consumed)
            } else {
                out.push_str(&digits);
            }
        }
    }
    out
}

fn normalize_partition_ids(rest: &str) -> String {
    // `partition:[123, 456]` or `partition:p2` — keep named `pN`, scrub numeric lists.
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if out.ends_with("partition:[") {
            for d in chars.by_ref() {
                if d == ']' {
                    out.push('…');
                    out.push(']');
                    break;
                }
            }
        }
    }
    out
}

/// Drop every non-system schema so a persistent TiKV cluster starts a run with
/// the empty catalog a fresh TiDB instance would have.
fn reset_tikv_schemas(storage: Arc<dyn sealdb_kv::Storage>) -> Result<()> {
    let session = new_session(storage);
    let listed = session.execute_stmt(
        "SELECT SCHEMA_NAME FROM information_schema.schemata ORDER BY SCHEMA_NAME",
    )?;
    for row in listed.rows {
        let Some(name) = row.first().map(|d| d.display()) else {
            continue;
        };
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "information_schema" | "mysql" | "performance_schema" | "metrics_schema" | "sys"
        ) {
            continue;
        }
        let _ = session.execute_stmt(&format!("DROP DATABASE IF EXISTS `{name}`"));
    }
    Ok(())
}

fn run_backend(    suites: &[Suite],
    backend: StoreBackend,
    opts: &RunOptions,
    record: bool,
) -> Result<(usize, usize, bool)> {
    let shared = match open_storage(backend, opts.pd.as_deref()) {
        Ok(storage) => storage,
        Err(err) if backend == StoreBackend::Tikv && !opts.require_tikv => {
            eprintln!("skip tikv integrationtest: {err}");
            return Ok((0, 0, true));
        }
        Err(err) => return Err(err),
    };
    let mut ran = 0;
    let mut skipped = 0;
    for suite in suites {
        if !suite_allowed(suite, opts) {
            skipped += 1;
            continue;
        }
        if backend == StoreBackend::Tikv && suite.skip_tikv {
            skipped += 1;
            continue;
        }
        // MemStorage snapshots clone the whole map, so keeping every earlier
        // suite's rows alive makes later suites quadratic. TiDB runs each test
        // file against a fresh instance, so the memory backend does the same;
        // TiKV keeps one persistent cluster, so it drops every earlier suite's
        // schema instead (an unfiltered `information_schema.views` query must
        // not see another file's leftovers).
        let storage = match backend {
            StoreBackend::Memory => open_storage(backend, opts.pd.as_deref())?,
            StoreBackend::Tikv => {
                reset_tikv_schemas(Arc::clone(&shared))?;
                Arc::clone(&shared)
            }
        };
        if record {
            record_suite(suite, Arc::clone(&storage))?;
        } else {
            if std::env::var("SEALDB_ITEST_PROGRESS").is_ok() {
                eprintln!("[itest] {} {} start", backend.as_str(), suite.name);
            }
            compare_suite(suite, Arc::clone(&storage))?;
            if std::env::var("SEALDB_ITEST_PROGRESS").is_ok() {
                eprintln!("[itest] {} {} ok", backend.as_str(), suite.name);
            }
        }
        ran += 1;
    }
    Ok((ran, skipped, false))
}

pub fn run_all(root: &Path, opts: &RunOptions) -> Result<RunReport> {
    let suites = load_suites(root)?;
    let stores = if opts.stores.is_empty() {
        vec![StoreBackend::Memory, StoreBackend::Tikv]
    } else {
        opts.stores.clone()
    };
    let mut report = RunReport::default();
    let mut recorded = false;
    for backend in stores {
        let (ran, skipped, tikv_skipped) =
            run_backend(&suites, backend, opts, opts.record && !recorded)?;
        if opts.record && ran > 0 {
            recorded = true;
        }
        report.skipped_suites = skipped;
        match backend {
            StoreBackend::Memory => report.memory = ran,
            StoreBackend::Tikv => {
                report.tikv = ran;
                report.tikv_skipped = tikv_skipped;
            }
        }
    }
    if report.ran() == 0 && !report.tikv_skipped {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "no integrationtest suites matched (skipped={}); \
                 check manifest statuses / --status / --wave / -t filter",
                report.skipped_suites
            ),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_error_and_sorted() {
        let steps = parse_test(
            "# comment\n-- error 1146\nSELECT * FROM missing.t;\n-- sorted_result\nSELECT 1 AS n;\n",
        )
        .unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].expected_error, Some(1146));
        assert!(steps[1].sorted);
    }

    #[test]
    fn test_manifest_loaded() {
        let m = load_manifest(&manifest_path()).unwrap();
        assert!(
            m.len() > 100,
            "expected imported TiDB suites in manifest, got {}",
            m.len()
        );
        assert!(m.values().any(|s| s.status == "local"));
        assert!(m.values().any(|s| s.status == "deferred"));
    }

    /// Both CI gate tests run the whole suite; they share the tmp-dir meta, so
    /// they must not interleave, and the planner recurses deeper than libtest's
    /// 2 MiB thread stack allows.
    static GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_gate(backend: StoreBackend) -> crate::RunReport {
        let _guard = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                run_all(
                    &testdata_root(),
                    &RunOptions {
                        stores: vec![backend],
                        require_tikv: false,
                        statuses: Some(vec!["green".into(), "local".into()]),
                        ..RunOptions::default()
                    },
                )
                .unwrap()
            })
            .expect("spawn suite thread")
            .join()
            .expect("suite thread panicked")
    }

    #[test]
    fn test_ci_suites_memory() {
        // CI gate: only green + local (default allowlist).
        let report = run_gate(StoreBackend::Memory);
        assert!(report.memory > 0);
        assert_eq!(report.tikv, 0);
    }

    #[test]
    fn test_ci_suites_tikv() {
        let report = run_gate(StoreBackend::Tikv);
        if report.tikv_skipped {
            eprintln!("skip live tikv integrationtest (PD unreachable)");
            return;
        }
        assert!(report.tikv > 0);
    }
}

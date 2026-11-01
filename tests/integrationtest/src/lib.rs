//! TiDB-style SQL integration tester (`tests/integrationtest`).
//!
//! Reads `t/*.test`, executes each statement through `Session`, and
//! compares against `r/*.result` in mysql-tester form:
//! echoed SQL, tab-separated header/rows, or `Error code (sqlstate): msg`.
//!
//! Each suite runs on MemStorage and, when PD is reachable, on TiKV.
//! Completeness wave: `connect`/`connection` multi-session, stats `s/*.json`,
//! live MySQL-protocol mysql-tester against `--listen`.

use sealdb_config::Config;
use sealdb_domain::Domain;
use sealdb_errno::{Error, Result};
use sealdb_session::Session;
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
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            record: false,
            filter: None,
            stores: vec![StoreBackend::Memory, StoreBackend::Tikv],
            require_tikv: require_tikv_from_env(),
            pd: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub memory: usize,
    pub tikv: usize,
    pub tikv_skipped: bool,
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
}

#[derive(Clone, Debug)]
pub struct Suite {
    pub name: String,
    pub test_path: PathBuf,
    pub result_path: PathBuf,
    pub steps: Vec<Step>,
}

pub fn testdata_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn parse_test(src: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut buf = String::new();
    let mut expected_error = None;
    let mut sorted = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() && buf.is_empty() {
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
            return Err(Error::unsupported(format!("test directive -- {rest}")));
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line.trim_end());
        if trimmed.ends_with(';') {
            steps.push(Step {
                sql: buf.trim().to_string(),
                expected_error,
                sorted,
            });
            buf.clear();
            expected_error = None;
            sorted = false;
        }
    }
    if !buf.trim().is_empty() {
        return Err(Error::new(
            sealdb_errno::ERR_PARSE_ERROR,
            format!("unterminated statement: {}", buf.trim()),
        ));
    }
    Ok(steps)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
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
    let mut suites = Vec::new();
    collect_tests(&t_dir, &t_dir, root, &mut suites)?;
    suites.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(suites)
}

fn collect_tests(dir: &Path, t_root: &Path, root: &Path, out: &mut Vec<Suite>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_tests(&path, t_root, root, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("test") {
            continue;
        }
        let rel = path.strip_prefix(t_root).unwrap().with_extension("result");
        let name = rel.with_extension("").to_string_lossy().replace('\\', "/");
        let src = fs::read_to_string(&path)
            .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
        out.push(Suite {
            name,
            test_path: path,
            result_path: root.join("r").join(rel),
            steps: parse_test(&src)?,
        });
    }
    Ok(())
}

fn require_tikv_from_env() -> bool {
    matches!(
        std::env::var("SEALDB_ITEST_REQUIRE_TIKV")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
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
    Session::with_domain(Arc::new(Domain::with_storage_id_base(
        storage,
        unique_id_base(),
    )))
}

pub fn run_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<String> {
    let session = new_session(storage);
    let mut chunks = Vec::new();
    for step in &suite.steps {
        chunks.push(run_step(&session, step)?);
    }
    let mut out = chunks.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn run_step(session: &Session, step: &Step) -> Result<String> {
    let result = session.execute_stmt(&step.sql);
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
            let mut lines = vec![step.sql.clone()];
            if !qr.columns.is_empty() {
                lines.push(qr.columns.join("\t"));
                let mut rows = qr.display_rows();
                if step.sorted {
                    rows.sort();
                }
                for row in rows {
                    lines.push(row.join("\t"));
                }
            }
            Ok(lines.join("\n"))
        }
        (Err(err), _) => Ok(format!(
            "{}\nError {} ({}): {}",
            step.sql, err.code, err.sqlstate, err.message
        )),
    }
}

pub fn compare_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<()> {
    let actual = run_suite(suite, storage)?;
    let expected = fs::read_to_string(&suite.result_path).map_err(|e| {
        Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("{}: {e}", suite.result_path.display()),
        )
    })?;
    if normalize(&actual) != normalize(&expected) {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!(
                "suite {} result mismatch\n--- expected ---\n{}--- actual ---\n{}",
                suite.name, expected, actual
            ),
        ));
    }
    Ok(())
}

pub fn record_suite(suite: &Suite, storage: Arc<dyn sealdb_kv::Storage>) -> Result<()> {
    let actual = run_suite(suite, storage)?;
    if let Some(parent) = suite.result_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))?;
    }
    fs::write(&suite.result_path, actual)
        .map_err(|e| Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, e.to_string()))
}

fn normalize(text: &str) -> String {
    let mut out = text.replace("\r\n", "\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn run_backend(
    suites: &[Suite],
    backend: StoreBackend,
    opts: &RunOptions,
    record: bool,
) -> Result<(usize, bool)> {
    let storage = match open_storage(backend, opts.pd.as_deref()) {
        Ok(storage) => storage,
        Err(err) if backend == StoreBackend::Tikv && !opts.require_tikv => {
            eprintln!("skip tikv integrationtest: {err}");
            return Ok((0, true));
        }
        Err(err) => return Err(err),
    };
    let mut ran = 0;
    for suite in suites {
        if let Some(filter) = opts.filter.as_deref()
            && !suite.name.contains(filter)
        {
            continue;
        }
        if record {
            record_suite(suite, Arc::clone(&storage))?;
        } else {
            compare_suite(suite, Arc::clone(&storage))?;
        }
        ran += 1;
    }
    if ran == 0 {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            format!("no integrationtest suites matched on {}", backend.as_str()),
        ));
    }
    Ok((ran, false))
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
        let (ran, skipped) = run_backend(&suites, backend, opts, opts.record && !recorded)?;
        if opts.record && ran > 0 {
            recorded = true;
        }
        match backend {
            StoreBackend::Memory => report.memory = ran,
            StoreBackend::Tikv => {
                report.tikv = ran;
                report.tikv_skipped = skipped;
            }
        }
    }
    if report.ran() == 0 && !report.tikv_skipped {
        return Err(Error::new(
            sealdb_errno::ERR_UNKNOWN_ERROR,
            "no integrationtest suites matched",
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
    fn test_all_suites_memory() {
        let report = run_all(
            &testdata_root(),
            &RunOptions {
                stores: vec![StoreBackend::Memory],
                require_tikv: false,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report.memory > 0);
        assert_eq!(report.tikv, 0);
    }

    #[test]
    fn test_all_suites_tikv() {
        let report = run_all(
            &testdata_root(),
            &RunOptions {
                stores: vec![StoreBackend::Tikv],
                require_tikv: false,
                ..RunOptions::default()
            },
        )
        .unwrap();
        if report.tikv_skipped {
            eprintln!("skip live tikv integrationtest (PD unreachable)");
            return;
        }
        assert!(report.tikv > 0);
    }
}

//! SQL binding record (TiDB `pkg/bindinfo/binding.go`).

/// Status string for an enabled binding (TiDB `Enabled` / legacy `Using`).
pub const ENABLED: &str = "enabled";

/// Source for bindings created by `CREATE BINDING FOR … USING …`.
pub const MANUAL: &str = "manual";

/// One SQL plan binding: map an original statement text to a hinted/bind SQL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    /// Original statement text as supplied to `CREATE BINDING FOR`.
    pub original_sql: String,
    /// Replacement statement text from `USING`.
    pub bind_sql: String,
    /// Default database when the binding was created (may be empty).
    pub db: String,
    /// Lifecycle status (`enabled`, `disabled`, …).
    pub status: String,
    /// Origin of the binding (`manual`, `capture`, …).
    pub source: String,
}

impl Binding {
    /// Build a manual enabled binding for MVB in-memory use.
    pub fn manual(
        original_sql: impl Into<String>,
        bind_sql: impl Into<String>,
        db: impl Into<String>,
    ) -> Self {
        Self {
            original_sql: original_sql.into(),
            bind_sql: bind_sql.into(),
            db: db.into(),
            status: ENABLED.to_string(),
            source: MANUAL.to_string(),
        }
    }

    /// Whether this binding may be applied to matching statements.
    pub fn is_enabled(&self) -> bool {
        self.status.eq_ignore_ascii_case(ENABLED) || self.status.eq_ignore_ascii_case("using")
    }
}

/// Collapse whitespace and lowercase for MVB exact matching of restored SQL.
/// Strips `USE/FORCE/IGNORE INDEX (...)` so hints do not block binding match
/// (TiDB digest ignores index hints; cte suite `use index()` still hits binding).
pub fn normalize_sql(sql: &str) -> String {
    let stripped = strip_index_hints(sql);
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn strip_index_hints(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, b"USE")
            || keyword_at(bytes, i, b"FORCE")
            || keyword_at(bytes, i, b"IGNORE")
        {
            let kw_len = if keyword_at(bytes, i, b"FORCE") {
                5
            } else if keyword_at(bytes, i, b"IGNORE") {
                6
            } else {
                3
            };
            let mut j = skip_ws(bytes, i + kw_len);
            if keyword_at(bytes, j, b"INDEX") || keyword_at(bytes, j, b"KEY") {
                j = skip_ws(bytes, j + if keyword_at(bytes, j, b"INDEX") { 5 } else { 3 });
                if j < bytes.len() && bytes[j] == b'(' {
                    j = skip_balanced_paren(bytes, j);
                    i = j;
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().expect("sql index in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn keyword_at(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    if i + kw.len() > bytes.len() {
        return false;
    }
    if !bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
    let after = i + kw.len();
    let after_ok =
        after >= bytes.len() || !bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_';
    before_ok && after_ok
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_balanced_paren(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

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
/// `IN (…)` value lists collapse to `IN (?)`: TiDB's binding matcher treats
/// `a in (1)` and `a in (1,2,3)` as the same statement.
pub fn normalize_sql(sql: &str) -> String {
    let stripped = strip_index_hints(sql);
    let uncommented = strip_sql_comments(&stripped);
    let collapsed = collapse_in_lists(uncommented.trim_end_matches(';').trim());
    tokenize_sql(&collapsed)
}

/// Drop `-- …` / `# …` line comments and non-hint `/* … */` blocks: the
/// integration harness appends trailing comments to statement text, and TiDB's
/// binding match keys on the statement without them.
fn strip_sql_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let quote = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                out.push(ch as char);
                i += 1;
                if ch == b'\\' && quote != b'`' && i < bytes.len() {
                    out.push(bytes[i] as char);
                    i += 1;
                    continue;
                }
                if ch == quote {
                    break;
                }
            }
            continue;
        }
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let hint = i + 2 < bytes.len() && bytes[i + 2] == b'+';
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            if hint {
                out.push_str(&sql[start..i]);
            } else {
                out.push(' ');
            }
            continue;
        }
        let ch = sql[i..].chars().next().expect("sql index in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Canonical token form: lowercased words, quoted strings/number literals kept
/// verbatim, backtick quoting dropped, one space between tokens. This makes
/// `a=1` and `a = 1` compare equal without losing literal values.
fn tokenize_sql(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if ch == b'\'' || ch == b'"' {
            let quote = ch;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            continue;
        }
        if ch == b'`' {
            // Quoted identifier: keep the name only (the restored form quotes
            // every identifier, the supplied text usually does not).
            let start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            out.push(String::from_utf8_lossy(&bytes[start..i]).to_ascii_lowercase());
            i += 1;
            continue;
        }
        if is_word_byte(ch) {
            let start = i;
            while i < bytes.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            out.push(String::from_utf8_lossy(&bytes[start..i]).to_ascii_lowercase());
            continue;
        }
        // Two-character operators keep their shape (`<=`, `<>`, `!=`, …).
        if i + 1 < bytes.len() {
            let pair = &bytes[i..i + 2];
            if matches!(
                pair,
                b"<=" | b">=" | b"<>" | b"!=" | b"&&" | b"||" | b":=" | b"->" | b"=>"
            ) {
                out.push(String::from_utf8_lossy(pair).into_owned());
                i += 2;
                continue;
            }
        }
        out.push((ch as char).to_string());
        i += 1;
    }
    out.retain(|token| token != ";");
    out.join(" ")
}

/// Replace the contents of every literal `IN (…)` list with `?`.
fn collapse_in_lists(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let quote = b;
            out.push(b as char);
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                out.push(ch as char);
                i += 1;
                if ch == b'\\' && quote != b'`' && i < bytes.len() {
                    out.push(bytes[i] as char);
                    i += 1;
                    continue;
                }
                if ch == quote {
                    break;
                }
            }
            continue;
        }
        if b.is_ascii_alphabetic()
            && (i == 0 || !is_word_byte(bytes[i - 1]))
            && keyword_at(bytes, i, b"IN")
            && (i + 2 >= bytes.len() || !is_word_byte(bytes[i + 2]))
        {
            // `IN` / `NOT IN` followed by `(`: collapse the value list.
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                let Some(end) = matching_paren(bytes, j) else {
                    out.push(bytes[i] as char);
                    i += 1;
                    continue;
                };
                let inner = &sql[j + 1..end];
                let inner_trimmed = inner.trim_start();
                if !inner_trimmed.to_ascii_lowercase().starts_with("select") {
                    out.push_str("in (?)");
                    i = end + 1;
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

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
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
                j = skip_ws(
                    bytes,
                    j + if keyword_at(bytes, j, b"INDEX") { 5 } else { 3 },
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_sql_collapses_in_lists() {
        assert_eq!(
            normalize_sql("SELECT a FROM t WHERE a IN (1)"),
            normalize_sql("select a from t where a in (1, 2, 3)")
        );
        assert_eq!(
            normalize_sql("select * from t where a in (1) and b in (1)"),
            normalize_sql("SELECT * FROM t WHERE a IN (1,2) AND b IN (1,2,3)")
        );
        // Subqueries keep their body.
        assert_ne!(
            normalize_sql("select a from t where a in (select x from u)"),
            normalize_sql("select a from t where a in (select y from u)")
        );
        assert_eq!(
            normalize_sql("select a from t where a in (1);  -- trailing comment"),
            normalize_sql("SELECT `a` FROM `t` WHERE `a` IN (1,2,3)")
        );
        assert_eq!(
            normalize_sql("select a from t where a=1"),
            normalize_sql("SELECT `a` FROM `t` WHERE `a` = 1")
        );
        // `in` inside a string literal or identifier is untouched.
        assert_eq!(
            normalize_sql("select 'in (1)' from t"),
            "select 'in (1)' from t"
        );
    }
}

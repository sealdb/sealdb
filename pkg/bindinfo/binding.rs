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
    /// TiDB-normalized original (`select * from `db` . `t` where `a` = ?`).
    pub normalized_sql: String,
    /// SHA-256 of [`Self::normalized_sql`] (TiDB `BindInfo.SQLDigest`).
    pub sql_digest: String,
    /// Digest of the chosen plan; empty for a manually created binding.
    pub plan_digest: String,
    /// Replacement statement text from `USING` (executed verbatim).
    pub bind_sql: String,
    /// `USING` text in TiDB restore form (`SELECT … FROM `db`.`t` …`), shown by
    /// `SHOW BINDINGS` and stored in `mysql.bind_info`.
    pub display_bind_sql: String,
    /// Default database when the binding was created (may be empty).
    pub db: String,
    /// Lifecycle status (`enabled`, `disabled`, …).
    pub status: String,
    /// Creation timestamp text (`YYYY-MM-DD HH:MM:SS`).
    pub create_time: String,
    /// Last status update timestamp text.
    pub update_time: String,
    /// Charset recorded with the binding (TiDB always `utf8mb4` here).
    pub charset: String,
    /// Collation recorded with the binding (`utf8mb4_general_ci`).
    pub collation: String,
    /// Origin of the binding (`manual`, `capture`, …).
    pub source: String,
    /// `CREATE GLOBAL BINDING` scope (domain-wide) vs session-local.
    pub global: bool,
}

impl Binding {
    /// Build a manual enabled binding for MVB in-memory use.
    pub fn manual(
        original_sql: impl Into<String>,
        normalized_sql: impl Into<String>,
        bind_sql: impl Into<String>,
        display_bind_sql: impl Into<String>,
        db: impl Into<String>,
        global: bool,
        create_time: impl Into<String>,
    ) -> Self {
        let normalized_sql = normalized_sql.into();
        let sql_digest = sql_digest(&normalized_sql);
        let create_time = create_time.into();
        Self {
            original_sql: original_sql.into(),
            normalized_sql,
            sql_digest,
            plan_digest: String::new(),
            bind_sql: bind_sql.into(),
            display_bind_sql: display_bind_sql.into(),
            db: db.into(),
            status: ENABLED.to_string(),
            create_time: create_time.clone(),
            update_time: create_time,
            charset: "utf8mb4".to_string(),
            collation: "utf8mb4_general_ci".to_string(),
            source: MANUAL.to_string(),
            global,
        }
    }

    /// Whether this binding may be applied to matching statements.
    pub fn is_enabled(&self) -> bool {
        self.status.eq_ignore_ascii_case(ENABLED) || self.status.eq_ignore_ascii_case("using")
    }
}

/// SHA-256 hex digest of normalized binding SQL (TiDB `SQLDigest`).
pub fn sql_digest(normalized_sql: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(normalized_sql.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// TiDB-normalized statement text for `SHOW BINDINGS` / `mysql.bind_info`
/// (`select * from `db` . `t` where `a` in ( ... )`). `restored` is the
/// schema-qualified SQL produced by the parser's binding restore mode.
pub fn normalize_display(restored: &str) -> String {
    render_normalized(&tokenize(restored), true)
}

/// Binding key that keeps literal values (used by `SET BINDING … FOR`):
/// spacing and keyword case are ignored, but `in (1)` stays distinct from
/// `in (1,2,3)`.
pub fn normalize_exact(sql: &str) -> String {
    let stripped = strip_sql_comments(sql.trim_end_matches(';').trim());
    render_normalized(&tokenize(&stripped), false)
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    /// Back-quoted identifier, kept verbatim (`db`).
    Name(String),
    /// String/number literal, rendered as `?` in normalized SQL.
    Literal(String),
    /// Punctuation / operator token.
    Punct(String),
}

/// Split restored SQL into identifier/literal/punctuation tokens. The input is
/// machine-restored text, so this only has to handle the shapes the restorer
/// emits (no comments, escaped quotes are still honored).
fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'`' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            out.push(Token::Name(sql[start..i].to_string()));
            continue;
        }
        if b == b'\'' || b == b'"' {
            let (_, end) = scan_quoted(bytes, i);
            out.push(Token::Literal(sql[i..end].to_string()));
            i = end;
            continue;
        }
        // `_utf8mb4'…'` charset introducer: literal.
        if b == b'_' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\'' {
                let (_, end) = scan_quoted(bytes, j);
                out.push(Token::Literal(sql[i..end].to_string()));
                i = end;
                continue;
            }
        }
        if b.is_ascii_digit() || (b == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'.'
                    || bytes[i] == b'x'
                    || bytes[i] == b'X')
            {
                i += 1;
            }
            out.push(Token::Literal(sql[start..i].to_string()));
            continue;
        }
        if is_word_byte(b) {
            let start = i;
            while i < bytes.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            out.push(Token::Word(sql[start..i].to_string()));
            continue;
        }
        // Operators: keep `<=`, `>=`, `<>`, `!=` as one token.
        if i + 1 < bytes.len() {
            let pair = &bytes[i..i + 2];
            if matches!(pair, b"<=" | b">=" | b"<>" | b"!=" | b"&&" | b"||") {
                out.push(Token::Punct(pair.iter().map(|b| *b as char).collect()));
                i += 2;
                continue;
            }
        }
        out.push(Token::Punct((b as char).to_string()));
        i += 1;
    }
    out
}

fn scan_quoted(bytes: &[u8], start: usize) -> (usize, usize) {
    let quote = bytes[start];
    let mut i = start + 1;
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
    (start, i)
}

/// Join tokens with single spaces. `normalized` replaces literals with `?` and
/// collapses `in (…)` value lists to `in ( ... )`.
fn render_normalized(tokens: &[Token], normalized: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(word) => {
                let lower = word.to_ascii_lowercase();
                let is_in = lower == "in";
                if normalized
                    && is_in
                    && matches!(tokens.get(i + 1), Some(Token::Punct(p)) if p == "(")
                {
                    // Consume the balanced list and render it as `( ... )`.
                    if let Some(end) = matching_token_paren(tokens, i + 1) {
                        parts.push(lower);
                        parts.push("(".to_string());
                        parts.push("...".to_string());
                        parts.push(")".to_string());
                        i = end + 1;
                        continue;
                    }
                }
                parts.push(lower);
            }
            Token::Name(name) => parts.push(name.clone()),
            Token::Literal(literal) => parts.push(if normalized {
                "?".to_string()
            } else {
                literal.clone()
            }),
            Token::Punct(punct) => parts.push(punct.clone()),
        }
        i += 1;
    }
    parts.join(" ")
}

fn matching_token_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, token) in tokens.iter().enumerate().skip(open) {
        if let Token::Punct(p) = token {
            match p.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(offset);
                    }
                }
                _ => {}
            }
        }
    }
    None
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
            // TiDB statement digest replaces every string literal with `?`.
            out.push("?".to_string());
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
            let word = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            // TiDB statement digest replaces numeric constants with `?`.
            if word.chars().next().is_some_and(|c| c.is_ascii_digit())
                && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
            {
                out.push("?".to_string());
            } else {
                out.push(word.to_ascii_lowercase());
            }
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
        assert_eq!(normalize_sql("select 'in (1)' from t"), "select ? from t");
    }
}

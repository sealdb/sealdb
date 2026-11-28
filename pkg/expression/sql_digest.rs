//! TiDB SQL digest support for `tidb_encode_sql_digest` /
//! `tidb_decode_sql_digests`.
//!
//! TiDB computes a statement digest as the SHA-256 of the parser-normalized
//! SQL text (`select `a` , `b` from `t` where `a` = ?`): keywords are
//! lower-cased, identifiers are back-quoted, and every literal is replaced by
//! a `?` placeholder. `tidb_decode_sql_digests` then resolves digests back to
//! the normalized text recorded by the statement summary.
//!
//! The registry lives in this crate so both the encoder (expression
//! evaluation) and the statement recorder (session) can share it.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

thread_local! {
    /// Session caller holds the global PROCESS privilege (TiDB requires it for
    /// `tidb_decode_sql_digests`). Defaults to true for contexts without a
    /// session (unit tests, internal evaluation).
    static HAS_PROCESS_PRIVILEGE: Cell<bool> = const { Cell::new(true) };
}

pub fn set_has_process_privilege(has: bool) {
    HAS_PROCESS_PRIVILEGE.with(|cell| cell.set(has));
}

pub fn has_process_privilege() -> bool {
    HAS_PROCESS_PRIVILEGE.with(Cell::get)
}

fn registry() -> &'static Mutex<HashMap<String, String>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an executed statement for `tidb_decode_sql_digests`.
pub fn record_sql(sql: &str) {
    let normalized = normalize(sql);
    if normalized.is_empty() {
        return;
    }
    let digest = digest_normalized(&normalized);
    let mut map = registry().lock().expect("sql digest registry lock");
    // Long suites run tens of thousands of statements; keep the map bounded.
    if map.len() >= 50_000 {
        map.clear();
    }
    map.insert(digest, normalized);
}

/// Normalized SQL text for a digest recorded by [`record_sql`].
pub fn lookup_normalized(digest: &str) -> Option<String> {
    registry()
        .lock()
        .expect("sql digest registry lock")
        .get(digest)
        .cloned()
}

/// SHA-256 hex digest of the normalized form of `sql` (TiDB SQLDigest).
pub fn sql_digest(sql: &str) -> String {
    digest_normalized(&normalize(sql))
}

fn digest_normalized(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// TiDB parser normalization: `select * from t where a = 1` becomes
/// ``select * from `t` where `a` = ?``.
pub fn normalize(sql: &str) -> String {
    let tokens = tokenize(&strip_comments(sql));
    let mut parts: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Word(word) => {
                if is_keyword(word) {
                    parts.push(word.to_ascii_lowercase());
                } else if matches!(tokens.get(index + 1), Some(Token::Punct(p)) if p == "(") {
                    // Function name: TiDB restores it unquoted.
                    parts.push(word.to_ascii_lowercase());
                } else {
                    parts.push(format!("`{word}`"));
                }
            }
            Token::QuotedName(name) => parts.push(format!("`{name}`")),
            Token::Variable(var) => parts.push(var.to_ascii_lowercase()),
            Token::Literal => parts.push("?".to_string()),
            Token::Punct(punct) => parts.push(punct.clone()),
        }
    }
    while parts.last().is_some_and(|last| last == ";") {
        parts.pop();
    }
    parts.join(" ")
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    /// Back-quoted identifier contents.
    QuotedName(String),
    /// `@v` / `@@sysvar` usage.
    Variable(String),
    /// String / numeric literal (rendered as `?`).
    Literal,
    Punct(String),
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn strip_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' || b == b'`' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && quote != b'`' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&sql[start..i]);
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
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
            continue;
        }
        let ch = sql[i..].chars().next().expect("sql index in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() || b == b';' {
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            // String literal: parameter marker.
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Token::Literal);
            continue;
        }
        if b == b'`' {
            let start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            out.push(Token::QuotedName(sql[start..i].to_string()));
            i = (i + 1).min(bytes.len());
            continue;
        }
        if b == b'@' {
            let start = i;
            i += 1;
            if i < bytes.len() && bytes[i] == b'@' {
                i += 1;
            }
            while i < bytes.len() && (is_word_byte(bytes[i]) || bytes[i] == b'.') {
                i += 1;
            }
            out.push(Token::Variable(sql[start..i].to_string()));
            continue;
        }
        if b == b'_' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            // Character-set introducer before a string (`_utf8mb4'x'`): the
            // whole thing is one literal.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"') {
                let quote = bytes[j];
                i = j + 1;
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
                out.push(Token::Literal);
                continue;
            }
        }
        if b.is_ascii_digit() || (b == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                i += 1;
            }
            out.push(Token::Literal);
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
        if i + 1 < bytes.len() {
            let pair = &bytes[i..i + 2];
            if matches!(
                pair,
                b"<=" | b">=" | b"<>" | b"!=" | b"&&" | b"||" | b":=" | b"->" | b"=>"
            ) {
                out.push(Token::Punct(String::from_utf8_lossy(pair).into_owned()));
                i += 2;
                continue;
            }
        }
        out.push(Token::Punct((b as char).to_string()));
        i += 1;
    }
    out
}

/// Reserved words TiDB leaves unquoted (and lower-cases) when normalizing.
fn is_keyword(word: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "add",
        "after",
        "all",
        "alter",
        "and",
        "as",
        "asc",
        "begin",
        "between",
        "by",
        "case",
        "change",
        "character",
        "collate",
        "column",
        "commit",
        "constraint",
        "create",
        "cross",
        "current_timestamp",
        "database",
        "default",
        "delete",
        "desc",
        "distinct",
        "drop",
        "else",
        "end",
        "exists",
        "explain",
        "false",
        "for",
        "foreign",
        "from",
        "full",
        "global",
        "grant",
        "group",
        "having",
        "if",
        "ignore",
        "in",
        "index",
        "inner",
        "insert",
        "interval",
        "into",
        "is",
        "join",
        "key",
        "left",
        "like",
        "limit",
        "lock",
        "match",
        "modify",
        "natural",
        "not",
        "null",
        "offset",
        "on",
        "or",
        "order",
        "outer",
        "partition",
        "primary",
        "references",
        "rename",
        "replace",
        "right",
        "rollback",
        "select",
        "set",
        "show",
        "start",
        "table",
        "then",
        "to",
        "transaction",
        "true",
        "union",
        "unique",
        "update",
        "use",
        "using",
        "values",
        "view",
        "when",
        "where",
        "with",
    ];
    KEYWORDS
        .binary_search(&word.to_ascii_lowercase().as_str())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_matches_tidb_shapes() {
        assert_eq!(normalize("begin"), "begin");
        assert_eq!(
            normalize("select @@tidb_current_ts"),
            "select @@tidb_current_ts"
        );
        assert_eq!(
            normalize("select id, v from test_func_decode_sql_digests where id = 1 for update"),
            "select `id` , `v` from `test_func_decode_sql_digests` where `id` = ? for update"
        );
        assert_eq!(
            sql_digest("begin"),
            "e6f07d43b5c21db0fbb9a31feac2dc599787763393dd5acbfad80e247eb02ad5"
        );
        assert_eq!(
            sql_digest("select id, v from test_func_decode_sql_digests where id = 1 for update"),
            "7e9f826ed22a09940940a42d1aca47a75b3adc3c3fde252f4b912ac886194eb9"
        );
    }

    #[test]
    fn literal_variants_share_a_digest() {
        assert_eq!(sql_digest("select 1;"), sql_digest("select 1 ;"));
        assert_eq!(sql_digest("select 1"), sql_digest("select 2 ;"));
    }
}

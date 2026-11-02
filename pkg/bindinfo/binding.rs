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
pub fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

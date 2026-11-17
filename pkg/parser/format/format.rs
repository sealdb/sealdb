/// SQL text builder shared by all `restore` implementations.
///
/// A default context writes the compact SQL shape used across the engine. A
/// *view* context reproduces TiDB's `SHOW CREATE VIEW` rendering: table names
/// carry their schema, select fields always get an `AS` alias, `WHERE`-style
/// comparisons lose their surrounding spaces and string literals carry the
/// `_UTF8MB4` introducer. A *generated* context reproduces TiDB's stored
/// `GENERATED ALWAYS AS (…)` text: lowercased function names, back-quoted
/// names and `_utf8mb4` string introducers.
pub struct RestoreCtx {
    buf: String,
    view_db: Option<String>,
    generated: bool,
    /// CTE names in scope while restoring a view body. References to these are
    /// written unqualified even in view mode (they are not base tables).
    cte_names: Vec<String>,
    /// Default schema used to qualify table names in *binding* restore mode
    /// (TiDB `mysql.bind_info.bind_sql`): keywords keep their default case and
    /// string literals carry no charset introducer, unlike view mode.
    qualify_db: Option<String>,
}

impl Default for RestoreCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl RestoreCtx {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            view_db: None,
            generated: false,
            cte_names: Vec::new(),
            qualify_db: None,
        }
    }

    /// Context for `ColumnInfo.GeneratedExprString` (TiDB restore flags:
    /// keywords lowercased, names back-quoted, string literals with charset).
    pub fn new_generated() -> Self {
        Self {
            buf: String::new(),
            view_db: None,
            generated: true,
            cte_names: Vec::new(),
            qualify_db: None,
        }
    }

    /// Context for persisting `ViewInfo.select_stmt` (TiDB restore format).
    pub fn new_view(db: &str) -> Self {
        Self {
            buf: String::new(),
            view_db: Some(db.to_string()),
            generated: false,
            cte_names: Vec::new(),
            qualify_db: None,
        }
    }

    /// Context for binding text (TiDB `bind_sql`): default keyword/identifier
    /// rendering, with table names qualified by `db`.
    pub fn new_qualify(db: &str) -> Self {
        Self {
            buf: String::new(),
            view_db: None,
            generated: false,
            cte_names: Vec::new(),
            qualify_db: Some(db.to_string()),
        }
    }

    /// Default schema used to qualify table names in binding mode.
    pub fn qualify_db(&self) -> Option<&str> {
        self.qualify_db.as_deref()
    }

    /// Binding restore mode: table names are schema-qualified, keywords keep
    /// their default rendering (comparisons with spaces, no charset
    /// introducer), unlike view mode.
    pub fn in_bind(&self) -> bool {
        self.qualify_db.is_some()
    }

    /// Stored generated-column expression context (see [`Self::new_generated`]).
    pub fn in_generated(&self) -> bool {
        self.generated
    }

    /// Default schema used to qualify table names in view mode.
    pub fn view_db(&self) -> Option<&str> {
        self.view_db.as_deref()
    }

    pub fn in_view(&self) -> bool {
        self.view_db.is_some()
    }

    /// Enter the scope of a `WITH` clause while restoring a view body.
    pub fn push_cte_names(&mut self, names: &[String]) {
        self.cte_names.extend(names.iter().cloned());
    }

    /// Leave a `WITH` scope, restoring the previous CTE name set.
    pub fn truncate_cte_names(&mut self, len: usize) {
        self.cte_names.truncate(len);
    }

    pub fn cte_name_scope_len(&self) -> usize {
        self.cte_names.len()
    }

    pub fn is_cte_name(&self, name: &str) -> bool {
        self.cte_names
            .iter()
            .any(|cte| cte.eq_ignore_ascii_case(name))
    }

    pub fn write_str(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    pub fn write_name(&mut self, name: &str) {
        self.buf.push('`');
        self.buf.push_str(name);
        self.buf.push('`');
    }

    pub fn into_string(self) -> String {
        self.buf
    }
}

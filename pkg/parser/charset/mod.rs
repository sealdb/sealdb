/// Session charset / collation names used by C5 SET NAMES.
/// Completeness wave: full charset tables, pad/weight strings, GBK.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Collation {
    pub name: String,
    pub charset: String,
    pub ci: bool,
    /// True for `utf8mb4_unicode_ci` / `utf8_unicode_ci` (UCA weight path).
    pub unicode: bool,
}

impl Collation {
    pub fn bin() -> Self {
        Self {
            name: "utf8mb4_bin".to_string(),
            charset: "utf8mb4".to_string(),
            ci: false,
            unicode: false,
        }
    }

    pub fn parse(name: &str) -> Self {
        let lower = name
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase();
        let unicode = lower.contains("unicode");
        let ci = unicode
            || lower.ends_with("_ci")
            || lower == "utf8mb4_general_ci"
            || lower == "utf8_general_ci";
        let charset = if lower.starts_with("utf8mb4") {
            "utf8mb4"
        } else if lower.starts_with("utf8") {
            "utf8"
        } else if lower.starts_with("latin1") {
            "latin1"
        } else if lower.starts_with("ascii") {
            "ascii"
        } else if lower.starts_with("binary") {
            "binary"
        } else {
            "utf8mb4"
        };
        Self {
            name: lower,
            charset: charset.to_string(),
            ci,
            unicode,
        }
    }
}

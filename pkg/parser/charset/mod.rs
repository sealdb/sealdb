//! Session charset / collation names used by SET NAMES.
//! Completeness wave: GBK / latin1 encode-decode MVB + collation dispatch.

pub mod encoding_gbk;
pub mod encoding_latin1;

/// Charsets accepted by `SET NAMES` / character_set_* (MVB surface).
pub const SUPPORTED_CHARSETS: &[&str] = &["utf8mb4", "utf8", "latin1", "binary", "ascii", "gbk"];

/// Collations accepted by `SET collation_connection` / SHOW COLLATION (MVB surface).
pub const SUPPORTED_COLLATIONS: &[&str] = &[
    "utf8mb4_bin",
    "utf8mb4_general_ci",
    "utf8mb4_unicode_ci",
    "utf8_bin",
    "latin1_bin",
    "binary",
    "ascii_bin",
    "gbk_bin",
    "gbk_chinese_ci",
];

pub fn is_supported_charset(name: &str) -> bool {
    let lower = name
        .trim_matches(|c| c == '\'' || c == '"')
        .to_ascii_lowercase();
    SUPPORTED_CHARSETS.contains(&lower.as_str())
}

pub fn is_supported_collation(name: &str) -> bool {
    let lower = name
        .trim_matches(|c| c == '\'' || c == '"')
        .to_ascii_lowercase();
    SUPPORTED_COLLATIONS.contains(&lower.as_str())
}

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

    /// Default binary collation for a charset name (`SET NAMES gbk` → `gbk_bin`).
    pub fn default_for_charset(charset: &str) -> Self {
        let charset = charset
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase();
        let name = match charset.as_str() {
            "gbk" => "gbk_bin",
            "latin1" => "latin1_bin",
            "utf8" => "utf8_bin",
            "ascii" => "ascii_bin",
            "binary" => "binary",
            "utf8mb4" => "utf8mb4_bin",
            other => {
                return Self::parse(&format!("{other}_bin"));
            }
        };
        Self::parse(name)
    }

    pub fn parse(name: &str) -> Self {
        let lower = name
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase();
        let unicode = lower.contains("unicode");
        let ci = unicode
            || lower.ends_with("_ci")
            || lower == "utf8mb4_general_ci"
            || lower == "utf8_general_ci"
            || lower == "gbk_chinese_ci";
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
        } else if lower.starts_with("gbk") {
            "gbk"
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

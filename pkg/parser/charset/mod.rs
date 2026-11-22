//! Session charset / collation names used by SET NAMES.
//! Completeness wave: GBK / latin1 encode-decode MVB + collation dispatch.

pub mod collate_0900;
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
    "utf8_general_ci",
    "utf8_unicode_ci",
    "latin1_bin",
    "binary",
    "ascii_bin",
    "gbk_bin",
    "gbk_chinese_ci",
];

/// Charsets accepted as a string-literal introducer (`_utf8mb4'…'`).
///
/// TiDB rejects `_gbk'…'` with error 1115 even though `gbk` is a valid table
/// charset, so this set is narrower than [`SUPPORTED_CHARSETS`].
pub const SUPPORTED_INTRODUCERS: &[&str] =
    &["utf8mb4", "utf8", "utf8mb3", "binary", "latin1", "ascii"];

pub fn is_supported_introducer(name: &str) -> bool {
    let lower = name
        .trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_ascii_lowercase();
    SUPPORTED_INTRODUCERS.contains(&lower.as_str())
}

/// Collations that exist in TiDB's legacy collation table but are not available
/// with the new collation framework enabled (`utf8mb4_0900_ai_ci`-less builds).
/// TiDB reports these with a different message than a fully unknown name.
pub const LEGACY_UNSUPPORTED_COLLATIONS: &[&str] = &[
    "utf8_roman_ci",
    "utf8mb4_roman_ci",
    "utf8_general_mysql500_ci",
    "utf8mb4_general_mysql500_ci",
    "utf8_esperanto_ci",
    "utf8mb4_esperanto_ci",
];

fn normalized_name(name: &str) -> String {
    name.trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_ascii_lowercase()
}

/// The error 1273 TiDB raises for an unusable collation name, or `None` when the
/// name is supported.
///
/// TiDB distinguishes "Unsupported collation when new collation is enabled"
/// (the name is a legacy collation it knows about) from "Unknown collation"
/// (no such collation at all).
pub fn collation_validation_error(name: &str) -> Option<sealdb_errno::Error> {
    let lower = normalized_name(name);
    if is_supported_collation(&lower) {
        return None;
    }
    let message = if LEGACY_UNSUPPORTED_COLLATIONS.contains(&lower.as_str()) {
        format!("Unsupported collation when new collation is enabled: '{lower}'")
    } else {
        format!("Unknown collation: '{lower}'")
    };
    Some(sealdb_errno::Error::new(
        sealdb_errno::ERR_UNKNOWN_COLLATION,
        message,
    ))
}

pub fn is_text_encodable(charset: &str, text: &str) -> bool {
    match normalized_name(charset).as_str() {
        "gbk" => encoding_gbk::decode(&encoding_gbk::encode(text)) == text,
        "ascii" => text.is_ascii(),
        // utf8mb4 / utf8 / latin1 (TiDB keeps latin1 as a UTF-8 nop) / binary.
        _ => true,
    }
}

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
            // TiDB keeps `gbk_chinese_ci` as the charset default (and thus the
            // collation `SET NAMES gbk` installs); table defaults use it too.
            "gbk" => "gbk_chinese_ci",
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

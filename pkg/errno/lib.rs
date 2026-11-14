#![forbid(unsafe_code)]

pub mod errcode;
pub mod errname;

pub use errcode::*;
pub use errname::{name_of, sqlstate_of};

#[derive(Debug, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct Error {
    pub code: i32,
    pub sqlstate: String,
    pub message: String,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            sqlstate: sqlstate_of(code).to_string(),
            message: message.into(),
        }
    }

    pub fn unsupported(feature: impl Into<String>) -> Self {
        Self::new(
            ERR_UNSUPPORTED,
            format!("{} is not available in this phase", feature.into()),
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// MySQL's error templates clamp the offending value (`%-.128s`) and the type
/// kind (`%-.64s`) so a huge row value cannot bloat the message.
pub fn truncate_error_arg(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// `%-.128s`: the offending value in `Incorrect/Truncated … value` messages.
pub fn error_value(text: &str) -> String {
    truncate_error_arg(text, 128)
}

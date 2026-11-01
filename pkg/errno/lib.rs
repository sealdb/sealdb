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

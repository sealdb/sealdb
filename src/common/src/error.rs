use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("网络错误: {0}")]
    Network(String),

    #[error("存储错误: {0}")]
    Storage(String),

    #[error("SQL 解析错误: {0}")]
    SqlParse(String),

    #[error("执行错误: {0}")]
    Execution(String),

    #[error("事务错误: {0}")]
    Transaction(String),

    #[error("序列化错误: {0}")]
    Serialization(String),

    #[error("反序列化错误: {0}")]
    Deserialization(String),

    #[error("内部错误: {0}")]
    Internal(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("其他错误: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Serialization(err.to_string())
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(err: tokio::task::JoinError) -> Self {
        Error::Internal(err.to_string())
    }
}

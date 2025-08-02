//! 存储层通用类型和错误定义

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// 存储层错误类型
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("连接错误: {0}")]
    Connection(String),

    #[error("操作超时: {0}")]
    Timeout(String),

    #[error("键不存在: {0}")]
    KeyNotFound(String),

    #[error("事务冲突: {0}")]
    TransactionConflict(String),

    #[error("存储引擎错误: {0}")]
    Engine(String),

    #[error("序列化错误: {0}")]
    Serialization(String),

    #[error("反序列化错误: {0}")]
    Deserialization(String),

    #[error("配置错误: {0}")]
    Configuration(String),

    #[error("内部错误: {0}")]
    Internal(String),
}

impl From<common::Error> for StorageError {
    fn from(err: common::Error) -> Self {
        match err {
            common::Error::Config(msg) => StorageError::Configuration(msg),
            common::Error::Network(msg) => StorageError::Connection(msg),
            common::Error::Storage(msg) => StorageError::Engine(msg),
            common::Error::SqlParse(msg) => StorageError::Internal(msg),
            common::Error::Execution(msg) => StorageError::Internal(msg),
            common::Error::Transaction(msg) => StorageError::TransactionConflict(msg),
            common::Error::Serialization(msg) => StorageError::Serialization(msg),
            common::Error::Deserialization(msg) => StorageError::Deserialization(msg),
            common::Error::Internal(msg) => StorageError::Internal(msg),
            common::Error::Io(io_err) => StorageError::Connection(io_err.to_string()),
            common::Error::Other(msg) => StorageError::Internal(msg),
        }
    }
}


/// 存储键类型
pub type Key = Vec<u8>;

/// 存储值类型
pub type Value = Vec<u8>;

/// 键值对
pub type KeyValue = (Key, Value);

/// 存储引擎类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EngineType {
    TiKV,
    RocksDB,
    MySQL,
    PostgreSQL,
    Memory,
}

/// 存储操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationType {
    Get,
    Put,
    Delete,
    Scan,
    Begin,
    Commit,
    Rollback,
}

/// 存储操作
#[derive(Debug, Clone)]
pub struct StorageOperation {
    pub operation_type: OperationType,
    pub key: Option<Key>,
    pub value: Option<Value>,
    pub start_key: Option<Key>,
    pub end_key: Option<Key>,
    pub limit: Option<u32>,
}

/// 存储计划
#[derive(Debug, Clone)]
pub struct StoragePlan {
    pub plan_id: String,
    pub operations: Vec<StorageOperation>,
    pub engine_type: EngineType,
}

/// 存储操作结果
#[derive(Debug, Clone)]
pub struct StorageOperationResult {
    pub operation_type: OperationType,
    pub key: Option<Key>,
    pub value: Option<Value>,
    pub success: bool,
    pub error_message: Option<String>,
    pub latency_ms: u64,
}

/// 存储计划结果
#[derive(Debug, Clone)]
pub struct StoragePlanResult {
    pub plan_id: String,
    pub operation_results: Vec<StorageOperationResult>,
    pub total_latency_ms: u64,
    pub success: bool,
    pub error_message: Option<String>,
}



/// 存储配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub engine_type: EngineType,
    pub connection_string: String,
    pub timeout_ms: u64,
    pub max_connections: u32,
    pub pool_size: u32,
    pub retry_count: u32,
    pub retry_delay_ms: u64,
    pub enable_tracing: bool,
    pub engine_specific: HashMap<String, serde_json::Value>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            engine_type: EngineType::TiKV,
            connection_string: "127.0.0.1:2379".to_string(),
            timeout_ms: 5000,
            max_connections: 100,
            pool_size: 10,
            retry_count: 3,
            retry_delay_ms: 100,
            enable_tracing: true,
            engine_specific: HashMap::new(),
        }
    }
}

/// 存储统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStats {
    pub engine_type: EngineType,
    pub total_operations: u64,
    pub successful_operations: u64,
    pub failed_operations: u64,
    pub total_latency_ms: u64,
    pub avg_latency_ms: f64,
    pub connection_count: u32,
    pub active_connections: u32,
}

impl Default for StorageStats {
    fn default() -> Self {
        Self {
            engine_type: EngineType::TiKV,
            total_operations: 0,
            successful_operations: 0,
            failed_operations: 0,
            total_latency_ms: 0,
            avg_latency_ms: 0.0,
            connection_count: 0,
            active_connections: 0,
        }
    }
}

/// 存储结果包装器
#[derive(Debug, Clone)]
pub struct StorageResult<T> {
    pub value: T,
    pub latency_ms: u64,
    pub engine_type: EngineType,
}

impl<T> StorageResult<T> {
    pub fn new(value: T, latency_ms: u64, engine_type: EngineType) -> Self {
        Self {
            value,
            latency_ms,
            engine_type,
        }
    }
}

/// 存储上下文
#[derive(Debug, Clone)]
pub struct StorageContext {
    pub transaction_id: Option<String>,
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub priority: u8,
    pub timeout_ms: u64,
    pub retry_count: u32,
}

impl Default for StorageContext {
    fn default() -> Self {
        Self {
            transaction_id: None,
            session_id: None,
            user_id: None,
            priority: 0,
            timeout_ms: 5000,
            retry_count: 3,
        }
    }
}

/// 存储选项
#[derive(Debug, Clone)]
pub struct StorageOptions {
    pub consistency_level: ConsistencyLevel,
    pub isolation_level: IsolationLevel,
    pub timeout_ms: u64,
    pub retry_count: u32,
    pub batch_size: u32,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            consistency_level: ConsistencyLevel::Strong,
            isolation_level: IsolationLevel::ReadCommitted,
            timeout_ms: 5000,
            retry_count: 3,
            batch_size: 100,
        }
    }
}

/// 一致性级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsistencyLevel {
    Strong,
    Eventual,
    ReadUncommitted,
}

/// 隔离级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// 通用错误类型
pub type Error = StorageError;
pub type Result<T> = std::result::Result<T, Error>;








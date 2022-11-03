use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// 数据库名称
pub type DatabaseName = String;

/// 表名
pub type TableName = String;

/// 列名
pub type ColumnName = String;

/// 行 ID
pub type RowId = u64;

/// 事务 ID
pub type TransactionId = Uuid;

/// 会话 ID
pub type SessionId = Uuid;

/// 数据类型
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DataType {
    Null,
    Boolean,
    Integer,
    BigInt,
    Float,
    Double,
    String,
    Binary,
    Timestamp,
    Date,
    Decimal { precision: u8, scale: u8 },
}

/// 列定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: ColumnName,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub default_value: Option<Value>,
}

/// 表定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub name: TableName,
    pub columns: Vec<Column>,
    pub primary_key: Vec<ColumnName>,
    pub indexes: Vec<Index>,
}

/// 索引定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<ColumnName>,
    pub unique: bool,
}

/// 数据值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i32),
    BigInt(i64),
    Float(f32),
    Double(f64),
    String(String),
    Binary(Vec<u8>),
    Timestamp(i64),
    Date(i64),
    Decimal(String),
}

/// 行数据
pub type Row = HashMap<ColumnName, Value>;

/// 查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
}

/// 事务状态
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionStatus {
    Active,
    Committed,
    Aborted,
}

/// 事务信息
#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: TransactionId,
    pub status: TransactionStatus,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub isolation_level: IsolationLevel,
}

/// 隔离级别
#[derive(Debug, Clone, PartialEq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// 锁类型
#[derive(Debug, Clone, PartialEq)]
pub enum LockType {
    Shared,
    Exclusive,
}

/// 锁信息
#[derive(Debug, Clone)]
pub struct Lock {
    pub resource: String,
    pub lock_type: LockType,
    pub transaction_id: TransactionId,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            Value::Integer(i) => Some(i.to_string()),
            Value::BigInt(i) => Some(i.to_string()),
            Value::Float(f) => Some(f.to_string()),
            Value::Double(d) => Some(d.to_string()),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i32> {
        match self {
            Value::Integer(i) => Some(*i),
            Value::BigInt(i) => Some(*i as i32),
            _ => None,
        }
    }
}

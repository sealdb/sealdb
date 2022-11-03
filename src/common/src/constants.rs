/// 系统常量定义
/// 默认数据库名称
pub const DEFAULT_DATABASE: &str = "default";

/// 系统表前缀
pub const SYSTEM_TABLE_PREFIX: &str = "__sealdb_";

/// 元数据表名
pub const METADATA_TABLE: &str = "__sealdb_metadata";

/// 事务表名
pub const TRANSACTION_TABLE: &str = "__sealdb_transactions";

/// 锁表名
pub const LOCK_TABLE: &str = "__sealdb_locks";

/// 默认事务超时时间（毫秒）
pub const DEFAULT_TRANSACTION_TIMEOUT: u64 = 30000;

/// 默认锁超时时间（毫秒）
pub const DEFAULT_LOCK_TIMEOUT: u64 = 10000;

/// 默认批处理大小
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// 最大批处理大小
pub const MAX_BATCH_SIZE: usize = 10000;

/// 默认连接池大小
pub const DEFAULT_CONNECTION_POOL_SIZE: usize = 10;

/// 最大连接池大小
pub const MAX_CONNECTION_POOL_SIZE: usize = 100;

/// 默认查询超时时间（毫秒）
pub const DEFAULT_QUERY_TIMEOUT: u64 = 30000;

/// 最大查询超时时间（毫秒）
pub const MAX_QUERY_TIMEOUT: u64 = 300000;

/// 默认内存使用限制（字节）
pub const DEFAULT_MEMORY_LIMIT: usize = 1024 * 1024 * 1024; // 1GB

/// 最大内存使用限制（字节）
pub const MAX_MEMORY_LIMIT: usize = 8 * 1024 * 1024 * 1024; // 8GB

/// 默认日志级别
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// 支持的日志级别
pub const SUPPORTED_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// 默认 TiKV 连接超时时间（毫秒）
pub const DEFAULT_TIKV_CONNECT_TIMEOUT: u64 = 5000;

/// 默认 TiKV 请求超时时间（毫秒）
pub const DEFAULT_TIKV_REQUEST_TIMEOUT: u64 = 10000;

/// 默认 TiKV 重试次数
pub const DEFAULT_TIKV_RETRY_TIMES: u32 = 3;

/// 默认 TiKV 重试间隔（毫秒）
pub const DEFAULT_TIKV_RETRY_INTERVAL: u64 = 100;

/// 系统版本
pub const SEALDB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 系统名称
pub const SEALDB_NAME: &str = "SealDB";

/// 系统描述
pub const SEALDB_DESCRIPTION: &str = "A distributed database system built with Rust";

/// 系统作者
pub const SEALDB_AUTHOR: &str = "SealDB Team";

/// 系统许可证
pub const SEALDB_LICENSE: &str = "MIT";

/// 系统仓库
pub const SEALDB_REPOSITORY: &str = "https://github.com/sealdb/sealdb";

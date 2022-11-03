use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant};
use uuid::Uuid;

/// 请求优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RequestPriority {
    /// 系统级请求（最高优先级）
    System = 0,
    /// 管理请求
    Admin = 1,
    /// 高优先级用户请求
    High = 2,
    /// 普通用户请求
    Normal = 3,
    /// 低优先级请求（如批量操作）
    Low = 4,
    /// 后台任务（最低优先级）
    Background = 5,
}

/// 请求类型
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RequestType {
    /// 查询请求
    Query,
    /// 写入请求
    Write,
    /// 事务请求
    Transaction,
    /// 管理请求
    Admin,
    /// 系统请求
    System,
    /// 批量操作
    Batch,
}

/// 连接状态
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// 空闲
    Idle,
    /// 忙碌
    Busy,
    /// 关闭中
    Closing,
    /// 已关闭
    Closed,
}

/// 连接信息
#[derive(Debug, Clone)]
pub struct Connection {
    pub id: Uuid,
    pub user_id: Option<String>,
    pub database: Option<String>,
    pub state: ConnectionState,
    pub created_at: Instant,
    pub last_used: Instant,
    pub request_count: u64,
    pub total_execution_time: Duration,
}

/// 请求信息
#[derive(Debug, Clone)]
pub struct Request {
    pub id: Uuid,
    pub priority: RequestPriority,
    pub request_type: RequestType,
    pub sql: String,
    pub connection_id: Uuid,
    pub user_id: Option<String>,
    pub database: Option<String>,
    pub created_at: Instant,
    pub timeout: Duration,
    pub estimated_cost: u64, // 预估执行成本
}

/// 线程池配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadPoolConfig {
    /// 核心线程数
    pub core_threads: usize,
    /// 最大线程数
    pub max_threads: usize,
    /// 队列大小
    pub queue_size: usize,
    /// 线程空闲超时时间（秒）
    pub thread_idle_timeout: u64,
    /// 请求超时时间（秒）
    pub request_timeout: u64,
    /// 连接池大小
    pub connection_pool_size: usize,
    /// 连接空闲超时时间（秒）
    pub connection_idle_timeout: u64,
    /// 是否启用优先级队列
    pub enable_priority_queue: bool,
    /// 是否启用资源限制
    pub enable_resource_limit: bool,
    /// 最大内存使用量（MB）
    pub max_memory_usage: usize,
    /// CPU 使用率限制（百分比）
    pub max_cpu_usage: f64,
}

impl Default for ThreadPoolConfig {
    fn default() -> Self {
        Self {
            core_threads: 4,
            max_threads: 16,
            queue_size: 1000,
            thread_idle_timeout: 60,
            request_timeout: 30,
            connection_pool_size: 100,
            connection_idle_timeout: 300,
            enable_priority_queue: true,
            enable_resource_limit: true,
            max_memory_usage: 1024, // 1GB
            max_cpu_usage: 80.0,
        }
    }
}

/// 线程池统计信息
#[derive(Debug, Clone)]
pub struct ThreadPoolStats {
    /// 活跃线程数
    pub active_threads: usize,
    /// 空闲线程数
    pub idle_threads: usize,
    /// 队列中的请求数
    pub queued_requests: usize,
    /// 活跃连接数
    pub active_connections: usize,
    /// 空闲连接数
    pub idle_connections: usize,
    /// 平均响应时间（毫秒）
    pub avg_response_time: f64,
    /// 每秒处理的请求数
    pub requests_per_second: f64,
    /// 内存使用量（MB）
    pub memory_usage: usize,
    /// CPU 使用率（百分比）
    pub cpu_usage: f64,
}

/// 资源使用情况
#[derive(Debug, Clone)]
pub struct ResourceUsage {
    /// 内存使用量（字节）
    pub memory_usage: usize,
    /// CPU 使用率（百分比）
    pub cpu_usage: f64,
    /// 活跃连接数
    pub active_connections: usize,
    /// 队列长度
    pub queue_length: usize,
}

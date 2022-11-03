use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant};
use tracing::{debug, info};
use uuid::Uuid;

use crate::{
    thread_pool::{Connection, ConnectionState},
    Error, Result,
};

/// 连接池配置
#[derive(Debug, Clone)]
pub struct ConnectionPoolConfig {
    /// 最大连接数
    pub max_connections: usize,
    /// 最小连接数
    pub min_connections: usize,
    /// 连接空闲超时时间（秒）
    pub idle_timeout: u64,
    /// 连接最大生存时间（秒）
    pub max_lifetime: u64,
    /// 连接获取超时时间（秒）
    pub acquire_timeout: u64,
}

impl Default for ConnectionPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 100,
            min_connections: 5,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        }
    }
}

/// 连接池统计信息
#[derive(Debug, Clone)]
pub struct ConnectionPoolStats {
    /// 总连接数
    pub total_connections: usize,
    /// 活跃连接数
    pub active_connections: usize,
    /// 空闲连接数
    pub idle_connections: usize,
    /// 等待连接的请求数
    pub waiting_requests: usize,
    /// 连接获取平均时间（毫秒）
    pub avg_acquire_time: f64,
}

/// 连接管理器
pub struct ConnectionManager {
    /// 配置
    config: ConnectionPoolConfig,
    /// 连接池
    connections: Arc<RwLock<HashMap<Uuid, Connection>>>,
    /// 空闲连接队列
    idle_connections: Arc<Mutex<Vec<Uuid>>>,
    /// 统计信息
    stats: Arc<RwLock<ConnectionPoolStats>>,
    /// 清理任务句柄
    cleanup_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectionManager {
    /// 创建新的连接管理器
    pub async fn new(config: ConnectionPoolConfig) -> Result<Self> {
        let manager = Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            idle_connections: Arc::new(Mutex::new(Vec::new())),
            stats: Arc::new(RwLock::new(ConnectionPoolStats {
                total_connections: 0,
                active_connections: 0,
                idle_connections: 0,
                waiting_requests: 0,
                avg_acquire_time: 0.0,
            })),
            cleanup_handle: None,
        };

        // 启动清理任务
        let cleanup_handle = manager.start_cleanup_task().await?;

        info!("ConnectionManager initialized successfully");
        Ok(Self {
            cleanup_handle: Some(cleanup_handle),
            ..manager
        })
    }

    /// 启动清理任务
    async fn start_cleanup_task(&self) -> Result<tokio::task::JoinHandle<()>> {
        let connections = self.connections.clone();
        let idle_connections = self.idle_connections.clone();
        let stats = self.stats.clone();
        let config = self.config.clone();

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));

            loop {
                interval.tick().await;

                let now = Instant::now();
                let mut to_remove = Vec::new();
                let mut to_idle = Vec::new();

                // 检查连接状态
                {
                    let mut connections_guard = connections.write().await;
                    for (id, connection) in connections_guard.iter_mut() {
                        // 检查连接是否超时
                        if now.duration_since(connection.last_used).as_secs() > config.idle_timeout
                            && connection.state == ConnectionState::Idle
                        {
                            to_remove.push(*id);
                        }

                        // 检查连接是否超过最大生存时间
                        if now.duration_since(connection.created_at).as_secs() > config.max_lifetime
                        {
                            to_remove.push(*id);
                        }

                        // 将长时间忙碌的连接标记为空闲
                        if connection.state == ConnectionState::Busy
                            && now.duration_since(connection.last_used).as_secs() > 30
                        {
                            connection.state = ConnectionState::Idle;
                            to_idle.push(*id);
                        }
                    }

                    // 移除过期连接
                    for id in &to_remove {
                        connections_guard.remove(id);
                    }
                }

                // 更新空闲连接队列
                {
                    let mut idle_guard = idle_connections.lock().await;
                    for id in &to_idle {
                        if !idle_guard.contains(id) {
                            idle_guard.push(*id);
                        }
                    }
                }

                // 更新统计
                {
                    let mut stats_guard = stats.write().await;
                    let connections_guard = connections.read().await;
                    stats_guard.total_connections = connections_guard.len();
                    stats_guard.idle_connections = idle_connections.lock().await.len();
                    stats_guard.active_connections =
                        connections_guard.len() - stats_guard.idle_connections;
                }

                debug!(
                    "Connection cleanup completed: removed {}, moved to idle: {}",
                    to_remove.len(),
                    to_idle.len()
                );
            }
        });

        Ok(handle)
    }

    /// 获取连接
    pub async fn get_connection(
        &self,
        user_id: Option<String>,
        database: Option<String>,
    ) -> Result<Uuid> {
        let start_time = Instant::now();

        // 首先尝试从空闲连接池获取
        {
            let mut idle_guard = self.idle_connections.lock().await;
            if let Some(connection_id) = idle_guard.pop() {
                // 检查连接是否仍然有效
                let mut connections_guard = self.connections.write().await;
                if let Some(connection) = connections_guard.get_mut(&connection_id) {
                    connection.state = ConnectionState::Busy;
                    connection.last_used = Instant::now();
                    connection.user_id = user_id.clone();
                    connection.database = database.clone();

                    // 更新统计
                    {
                        let mut stats_guard = self.stats.write().await;
                        stats_guard.idle_connections -= 1;
                        stats_guard.active_connections += 1;
                        stats_guard.avg_acquire_time = (stats_guard.avg_acquire_time
                            + start_time.elapsed().as_millis() as f64)
                            / 2.0;
                    }

                    debug!("Reused connection: {}", connection_id);
                    return Ok(connection_id);
                }
            }
        }

        // 创建新连接
        if self.can_create_new_connection().await {
            let connection_id = self.create_connection(user_id, database).await?;

            // 更新统计
            {
                let mut stats_guard = self.stats.write().await;
                stats_guard.total_connections += 1;
                stats_guard.active_connections += 1;
                stats_guard.avg_acquire_time =
                    (stats_guard.avg_acquire_time + start_time.elapsed().as_millis() as f64) / 2.0;
            }

            debug!("Created new connection: {}", connection_id);
            return Ok(connection_id);
        }

        // 等待可用连接
        self.wait_for_connection(user_id, database).await
    }

    /// 检查是否可以创建新连接
    async fn can_create_new_connection(&self) -> bool {
        let stats = self.stats.read().await;
        stats.total_connections < self.config.max_connections
    }

    /// 创建新连接
    async fn create_connection(
        &self,
        user_id: Option<String>,
        database: Option<String>,
    ) -> Result<Uuid> {
        let connection_id = Uuid::new_v4();
        let connection = Connection {
            id: connection_id,
            user_id,
            database,
            state: ConnectionState::Busy,
            created_at: Instant::now(),
            last_used: Instant::now(),
            request_count: 0,
            total_execution_time: Duration::ZERO,
        };

        {
            let mut connections_guard = self.connections.write().await;
            connections_guard.insert(connection_id, connection);
        }

        Ok(connection_id)
    }

    /// 等待可用连接
    async fn wait_for_connection(
        &self,
        user_id: Option<String>,
        database: Option<String>,
    ) -> Result<Uuid> {
        let timeout = Duration::from_secs(self.config.acquire_timeout);
        let start_time = Instant::now();

        // 更新统计
        {
            let mut stats_guard = self.stats.write().await;
            stats_guard.waiting_requests += 1;
        }

        loop {
            if start_time.elapsed() > timeout {
                // 更新统计
                {
                    let mut stats_guard = self.stats.write().await;
                    stats_guard.waiting_requests -= 1;
                }
                return Err(Error::Execution("Connection acquire timeout".to_string()));
            }

            // 尝试直接创建或获取连接
            if self.can_create_new_connection().await {
                let connection_id = self
                    .create_connection(user_id.clone(), database.clone())
                    .await?;

                // 更新统计
                {
                    let mut stats_guard = self.stats.write().await;
                    stats_guard.waiting_requests -= 1;
                    stats_guard.total_connections += 1;
                    stats_guard.active_connections += 1;
                }
                return Ok(connection_id);
            }

            // 检查是否有空闲连接
            {
                let mut idle_guard = self.idle_connections.lock().await;
                if let Some(connection_id) = idle_guard.pop() {
                    // 检查连接是否仍然有效
                    let mut connections_guard = self.connections.write().await;
                    if let Some(connection) = connections_guard.get_mut(&connection_id) {
                        connection.state = ConnectionState::Busy;
                        connection.last_used = Instant::now();
                        connection.user_id = user_id.clone();
                        connection.database = database.clone();

                        // 更新统计
                        {
                            let mut stats_guard = self.stats.write().await;
                            stats_guard.waiting_requests -= 1;
                            stats_guard.idle_connections -= 1;
                            stats_guard.active_connections += 1;
                        }
                        return Ok(connection_id);
                    }
                }
            }

            // 等待一段时间后重试
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// 释放连接
    pub async fn release_connection(&self, connection_id: Uuid) -> Result<()> {
        {
            let mut connections_guard = self.connections.write().await;
            if let Some(connection) = connections_guard.get_mut(&connection_id) {
                connection.state = ConnectionState::Idle;
                connection.last_used = Instant::now();
                connection.request_count += 1;
            }
        }

        // 添加到空闲连接队列
        {
            let mut idle_guard = self.idle_connections.lock().await;
            if !idle_guard.contains(&connection_id) {
                idle_guard.push(connection_id);
            }
        }

        // 更新统计
        {
            let mut stats_guard = self.stats.write().await;
            stats_guard.active_connections -= 1;
            stats_guard.idle_connections += 1;
        }

        debug!("Released connection: {}", connection_id);
        Ok(())
    }

    /// 关闭连接
    pub async fn close_connection(&self, connection_id: Uuid) -> Result<()> {
        {
            let mut connections_guard = self.connections.write().await;
            connections_guard.remove(&connection_id);
        }

        // 从空闲连接队列中移除
        {
            let mut idle_guard = self.idle_connections.lock().await;
            idle_guard.retain(|&id| id != connection_id);
        }

        // 更新统计
        {
            let mut stats_guard = self.stats.write().await;
            stats_guard.total_connections -= 1;
            stats_guard.active_connections -= 1;
        }

        debug!("Closed connection: {}", connection_id);
        Ok(())
    }

    /// 获取连接池统计信息
    pub async fn get_stats(&self) -> ConnectionPoolStats {
        self.stats.read().await.clone()
    }

    /// 关闭连接管理器
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down ConnectionManager...");

        // 取消清理任务
        if let Some(handle) = &self.cleanup_handle {
            handle.abort();
        }

        // 关闭所有连接
        {
            let mut connections_guard = self.connections.write().await;
            connections_guard.clear();
        }

        {
            let mut idle_guard = self.idle_connections.lock().await;
            idle_guard.clear();
        }

        info!("ConnectionManager shutdown completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connection_manager_new() {
        let config = ConnectionPoolConfig {
            max_connections: 10,
            min_connections: 2,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();
        let stats = manager.get_stats().await;
        assert_eq!(stats.total_connections, 0);
        assert_eq!(stats.active_connections, 0);
        assert_eq!(stats.idle_connections, 0);
    }

    #[tokio::test]
    async fn test_connection_manager_create_connection() {
        let config = ConnectionPoolConfig {
            max_connections: 5,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        let _connection_id = manager
            .get_connection(Some("user1".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();
        let stats = manager.get_stats().await;
        assert_eq!(stats.active_connections, 1);
        assert_eq!(stats.idle_connections, 0);
    }

    #[tokio::test]
    async fn test_connection_manager_get_connection() {
        let config = ConnectionPoolConfig {
            max_connections: 5,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        // 获取连接
        let _connection_id = manager
            .get_connection(Some("user1".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();
        let stats = manager.get_stats().await;
        assert_eq!(stats.active_connections, 1);
        assert_eq!(stats.idle_connections, 0);
    }

    #[tokio::test]
    async fn test_connection_manager_release_connection() {
        let config = ConnectionPoolConfig {
            max_connections: 5,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        // 获取连接
        let connection_id = manager
            .get_connection(Some("user1".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();

        // 释放连接
        manager.release_connection(connection_id).await.unwrap();

        let stats = manager.get_stats().await;
        assert_eq!(stats.active_connections, 0);
        assert_eq!(stats.idle_connections, 1);
    }

    #[tokio::test]
    async fn test_connection_manager_close_connection() {
        let config = ConnectionPoolConfig {
            max_connections: 5,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        // 获取连接
        let connection_id = manager
            .get_connection(Some("user1".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();

        // 关闭连接
        manager.close_connection(connection_id).await.unwrap();

        let stats = manager.get_stats().await;
        assert_eq!(stats.total_connections, 0);
        assert_eq!(stats.active_connections, 0);
        assert_eq!(stats.idle_connections, 0);
    }

    #[tokio::test]
    async fn test_connection_manager_max_connections() {
        let config = ConnectionPoolConfig {
            max_connections: 2,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        // 获取两个连接
        let _conn1 = manager
            .get_connection(Some("user1".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();
        let _conn2 = manager
            .get_connection(Some("user2".to_string()), Some("test_db".to_string()))
            .await
            .unwrap();

        let stats = manager.get_stats().await;
        assert_eq!(stats.active_connections, 2);

        // 尝试获取第三个连接应该失败
        let result = manager
            .get_connection(Some("user3".to_string()), Some("test_db".to_string()))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connection_manager_get_stats() {
        let config = ConnectionPoolConfig {
            max_connections: 10,
            min_connections: 1,
            idle_timeout: 300,
            max_lifetime: 3600,
            acquire_timeout: 30,
        };
        let manager = ConnectionManager::new(config).await.unwrap();

        // 获取一些连接
        for i in 0..3 {
            let _connection_id = manager
                .get_connection(Some(format!("user{i}")), Some("test_db".to_string()))
                .await
                .unwrap();
        }

        let stats = manager.get_stats().await;
        assert_eq!(stats.active_connections, 3);
        assert_eq!(stats.idle_connections, 0);
        assert_eq!(stats.total_connections, 3);
    }
}

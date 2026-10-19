//! 连接池实现
//!
//! 管理存储引擎连接的生命周期和复用

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use tokio::sync::Semaphore;
use tracing::{debug, info};

use crate::common::*;
use crate::engine::StorageEngine;

/// 连接池
pub struct ConnectionPool {
    config: StorageConfig,
    connections: Arc<RwLock<HashMap<EngineType, Vec<Arc<dyn StorageEngine>>>>>,
    semaphore: Arc<Semaphore>,
    max_connections: u32,
}

impl ConnectionPool {
    /// 创建新的连接池
    pub fn new(config: StorageConfig) -> Self {
        let max_connections = config.max_connections;
        Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(max_connections as usize)),
            max_connections,
        }
    }

    /// 获取连接
    pub async fn get_connection(&self, engine_type: EngineType) -> Result<Arc<dyn StorageEngine>> {
        // 获取信号量许可
        let _permit = self.semaphore.acquire().await.map_err(|e| {
            StorageError::Connection(format!("Failed to acquire connection permit: {}", e))
        })?;

        // 检查是否有可用连接
        {
            let connections = self.connections.read();
            if let Some(engine_connections) = connections.get(&engine_type) {
                if !engine_connections.is_empty() {
                    // 返回第一个可用连接
                    return Ok(engine_connections[0].clone());
                }
            }
        }

        // 没有可用连接，创建新连接
        self.create_connection(engine_type).await
    }

    /// 创建新连接
    async fn create_connection(&self, engine_type: EngineType) -> Result<Arc<dyn StorageEngine>> {
        // 这里应该根据引擎类型创建相应的连接
        // 简化实现，返回错误
        Err(StorageError::Connection(
            format!("Connection creation not implemented for engine: {:?}", engine_type)
        ).into())
    }

    /// 释放连接
    pub async fn release_connection(&self, _engine: Arc<dyn StorageEngine>) {
        // 简化实现，信号量会在 _permit 被 drop 时自动释放
        debug!("Connection released back to pool");
    }

    /// 健康检查所有连接
    pub async fn health_check(&self) -> HashMap<EngineType, bool> {
        let mut results = HashMap::new();
        let connections = self.connections.read();

        for (engine_type, engine_connections) in connections.iter() {
            let mut is_healthy = false;
            for engine in engine_connections {
                if let Ok(healthy) = engine.health_check().await {
                    is_healthy = healthy;
                    if is_healthy {
                        break;
                    }
                }
            }
            results.insert(*engine_type, is_healthy);
        }

        results
    }

    /// 关闭连接池
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down connection pool");
        let connections = self.connections.read();

        for (engine_type, engine_connections) in connections.iter() {
            for engine in engine_connections {
                // 注意：Arc<dyn StorageEngine> 不能调用 shutdown，因为需要 &mut self
                // 这里只能记录日志，实际关闭应该在工厂层面处理
                debug!("Engine {:?} will be cleaned up by factory", engine_type);
            }
        }

        info!("Connection pool shut down successfully");
        Ok(())
    }
}
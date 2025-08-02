//! 存储引擎工厂
//! 
//! 负责创建和管理不同的存储引擎实例

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use common::Result;

use crate::common::*;
use crate::engine::{StorageEngine, TiKVEngine, MemoryEngine};

/// 存储引擎工厂
pub struct StorageEngineFactory {
    engines: Arc<RwLock<HashMap<EngineType, Box<dyn StorageEngine>>>>,
    configs: Arc<RwLock<HashMap<EngineType, StorageConfig>>>,
}

impl StorageEngineFactory {
    /// 创建新的存储引擎工厂
    pub fn new() -> Self {
        Self {
            engines: Arc::new(RwLock::new(HashMap::new())),
            configs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 注册存储引擎配置
    pub async fn register_engine(
        &self,
        engine_type: EngineType,
        config: StorageConfig,
    ) -> Result<()> {
        let mut configs = self.configs.write();
        configs.insert(engine_type, config);
        Ok(())
    }

    /// 创建存储引擎实例
    pub async fn create_engine(&self, engine_type: EngineType) -> Result<Box<dyn StorageEngine>> {
        let config = {
            let configs = self.configs.read();
            configs.get(&engine_type)
                .cloned()
                .ok_or_else(|| StorageError::Configuration(format!("No config found for engine: {:?}", engine_type)))?
        };

        let engine: Box<dyn StorageEngine> = match engine_type {
            EngineType::TiKV => {
                let mut tikv_engine = TiKVEngine::new();
                tikv_engine.initialize(&config).await?;
                Box::new(tikv_engine)
            }
            EngineType::Memory => {
                let mut memory_engine = MemoryEngine::new();
                memory_engine.initialize(&config).await?;
                Box::new(memory_engine)
            }
            _ => {
                return Err(StorageError::Configuration(
                    format!("Engine type {:?} not supported yet", engine_type)
                ).into());
            }
        };

        // 缓存引擎实例
        let mut engines = self.engines.write();
        engines.insert(engine_type, engine.clone());
        
        Ok(engine)
    }

    /// 获取存储引擎实例
    pub async fn get_engine(&self, engine_type: EngineType) -> Result<Box<dyn StorageEngine>> {
        // 先尝试从缓存获取
        {
            let engines = self.engines.read();
            if let Some(engine) = engines.get(&engine_type) {
                return Ok(engine.clone());
            }
        }

        // 如果缓存中没有，则创建新的实例
        self.create_engine(engine_type).await
    }

    /// 获取所有已注册的引擎类型
    pub fn get_registered_engines(&self) -> Vec<EngineType> {
        let configs = self.configs.read();
        configs.keys().cloned().collect()
    }

    /// 移除存储引擎
    pub async fn remove_engine(&self, engine_type: EngineType) -> Result<()> {
        let mut engines = self.engines.write();
        if let Some(mut engine) = engines.remove(&engine_type) {
            engine.shutdown().await?;
        }
        Ok(())
    }

    /// 关闭所有存储引擎
    pub async fn shutdown_all(&self) -> Result<()> {
        let mut engines = self.engines.write();
        for (_, mut engine) in engines.drain() {
            if let Err(e) = engine.shutdown().await {
                tracing::error!("Failed to shutdown engine: {}", e);
            }
        }
        Ok(())
    }

    /// 健康检查所有引擎
    pub async fn health_check_all(&self) -> HashMap<EngineType, bool> {
        let mut results = HashMap::new();
        let engines = self.engines.read();
        
        for (engine_type, engine) in engines.iter() {
            let is_healthy = engine.health_check().await.unwrap_or(false);
            results.insert(*engine_type, is_healthy);
        }
        
        results
    }

    /// 获取所有引擎的统计信息
    pub async fn get_all_stats(&self) -> HashMap<EngineType, StorageStats> {
        let mut results = HashMap::new();
        let engines = self.engines.read();
        
        for (engine_type, engine) in engines.iter() {
            if let Ok(stats) = engine.get_stats().await {
                results.insert(*engine_type, stats);
            }
        }
        
        results
    }
}

impl Default for StorageEngineFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for StorageEngineFactory {
    fn drop(&mut self) {
        // 在 drop 时尝试关闭所有引擎
        let engines = self.engines.read();
        for (_, engine) in engines.iter() {
            // 注意：这里不能使用 async，所以只能记录日志
            tracing::warn!("Storage engine factory dropped without proper shutdown");
        }
    }
} 
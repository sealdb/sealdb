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
        match engine_type {
            EngineType::TiKV => {
                let config = {
                    let configs = self.configs.read();
                    match configs.get(&engine_type) {
                        Some(config) => config.clone(),
                        None => return Err(common::Error::Config(format!("No config found for engine: {:?}", engine_type))),
                    }
                };

                let mut engine = TiKVEngine::new();
                engine.initialize(&config).await.map_err(|e| common::Error::Storage(e.to_string()))?;
                Ok(Box::new(engine))
            }
            EngineType::Memory => {
                let config = {
                    let configs = self.configs.read();
                    let default_config = StorageConfig::default();
                    configs.get(&engine_type)
                        .unwrap_or(&default_config)
                        .clone()
                };

                let mut engine = MemoryEngine::new();
                engine.initialize(&config).await.map_err(|e| common::Error::Storage(e.to_string()))?;
                Ok(Box::new(engine))
            }
            _ => {
                Err(common::Error::Config(format!("Unsupported engine type: {:?}", engine_type)))
            }
        }
    }

    /// 获取存储引擎实例
    pub async fn get_engine(&self, engine_type: EngineType) -> Result<Box<dyn StorageEngine>> {
        // 由于 Box<dyn StorageEngine> 不能克隆，每次都创建新实例
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
            engine.shutdown().await.map_err(|e| common::Error::Storage(e.to_string()))?;
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
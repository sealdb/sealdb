//! SealDB 存储层
//!
//! 提供统一的存储抽象，支持多种存储引擎：
//! - TiKV
//! - RocksDB (计划中)
//! - MySQL (计划中)
//! - PostgreSQL (计划中)

pub mod common;
pub mod engine;
pub mod client;

// 重新导出 common 模块
pub use common::*;

// 重新导出主要类型
pub use engine::*;
pub use client::*;

// 明确导出常用类型
pub use common::{EngineType, StorageConfig, StorageContext, StorageOptions, StorageResult, StorageStats, StorageError, StorageOperation, StorageOperationResult};
pub use engine::{StorageEngine, StorageTransaction, StorageEngineFactory};
pub use client::StorageClient;

/// 存储层版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 存储层初始化
pub async fn init_storage_layer() -> Result<()> {
    tracing::info!("Initializing SealDB storage layer v{}", VERSION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio;

    #[tokio::test]
    async fn test_storage_layer_init() {
        let result = init_storage_layer().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_storage_engine_factory() {
        let factory = StorageEngineFactory::new();

        // 注册 TiKV 引擎配置
        let mut config = StorageConfig::default();
        config.engine_type = EngineType::TiKV;
        config.connection_string = "127.0.0.1:2379".to_string();

        let result = factory.register_engine(EngineType::TiKV, config).await;
        assert!(result.is_ok());

        let registered_engines = factory.get_registered_engines();
        assert!(registered_engines.contains(&EngineType::TiKV));
    }

    #[tokio::test]
    async fn test_memory_engine() {
        let mut engine = MemoryEngine::new();
        let config = StorageConfig::default();

        // 初始化引擎
        let result = engine.initialize(&config).await;
        assert!(result.is_ok());

        // 测试基本操作
        let context = StorageContext::default();
        let options = StorageOptions::default();

        // 测试 put
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();
        let result = engine.put(&key, &value, &context, &options).await;
        assert!(result.is_ok());

        // 测试 get
        let result = engine.get(&key, &context, &options).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.value, Some(value));

        // 测试 delete
        let result = engine.delete(&key, &context, &options).await;
        assert!(result.is_ok());

        // 验证删除后获取不到
        let result = engine.get(&key, &context, &options).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.value, None);
    }

    #[tokio::test]
    async fn test_storage_client() {
        let config = StorageConfig::default();
        let client = StorageClient::new(config).await;
        assert!(client.is_ok());
    }
}
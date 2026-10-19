//! 存储客户端层
//!
//! 提供高级存储操作接口，包括连接池、重试机制、负载均衡等


use common::Result;
use std::collections::HashMap;
use std::sync::Arc;



use crate::common::*;
use crate::engine::{StorageEngine, StorageEngineFactory, StorageTransaction};

pub mod connection_pool;
pub mod retry;
pub mod load_balancer;

pub use connection_pool::ConnectionPool;
pub use retry::RetryPolicy;
pub use load_balancer::LoadBalancer;

/// 存储客户端
pub struct StorageClient {
    factory: Arc<StorageEngineFactory>,
    connection_pool: Arc<ConnectionPool>,
    retry_policy: Arc<RetryPolicy>,
    load_balancer: Arc<LoadBalancer>,
    default_context: StorageContext,
    default_options: StorageOptions,
}

impl StorageClient {
    /// 创建新的存储客户端
    pub async fn new(config: StorageConfig) -> Result<Self> {
        let factory = Arc::new(StorageEngineFactory::new());

        // 注册引擎配置
        factory.register_engine(config.engine_type, config.clone()).await?;

        let connection_pool = Arc::new(ConnectionPool::new(config.clone()));
        let retry_policy = Arc::new(RetryPolicy::new(config.retry_count, config.retry_delay_ms));
        let load_balancer = Arc::new(LoadBalancer::new());

        let default_context = StorageContext::default();
        let default_options = StorageOptions::default();

        Ok(Self {
            factory,
            connection_pool,
            retry_policy,
            load_balancer,
            default_context,
            default_options,
        })
    }

    /// 获取存储引擎
    async fn get_engine(&self, engine_type: EngineType) -> Result<Box<dyn StorageEngine>> {
        self.factory.get_engine(engine_type).await
    }

    /// 执行带重试的操作
    async fn execute_with_retry<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = std::result::Result<T, StorageError>> + Send,
    {
        let mut last_error = None;

        for attempt in 0..self.retry_policy.max_retries {
            match operation().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.retry_policy.max_retries - 1 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            self.retry_policy.retry_delay_ms * (attempt + 1) as u64
                        )).await;
                    }
                }
            }
        }

        Err(last_error.map(|e| common::Error::Storage(e.to_string())).unwrap_or_else(|| {
            common::Error::Internal("Max retries exceeded".to_string())
        }))
    }

    /// 获取键值
    pub async fn get(
        &self,
        key: &Key,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<Option<Value>>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await.map_err(|e| StorageError::Engine(e.to_string()))?;
            engine.get(key, &context, &options).await.map_err(|e| StorageError::Engine(e.to_string()))
        }).await
    }

    /// 设置键值
    pub async fn put(
        &self,
        key: &Key,
        value: &Value,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<()>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.put(key, value, &context, &options).await
        }).await
    }

    /// 删除键
    pub async fn delete(
        &self,
        key: &Key,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<()>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.delete(key, &context, &options).await
        }).await
    }

    /// 扫描键值
    pub async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<Vec<KeyValue>>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.scan(start_key, end_key, limit, &context, &options).await
        }).await
    }

    /// 批量获取
    pub async fn batch_get(
        &self,
        keys: &[Key],
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<HashMap<Key, Option<Value>>>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.batch_get(keys, &context, &options).await
        }).await
    }

    /// 批量设置
    pub async fn batch_put(
        &self,
        key_values: &[KeyValue],
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<()>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.batch_put(key_values, &context, &options).await
        }).await
    }

    /// 批量删除
    pub async fn batch_delete(
        &self,
        keys: &[Key],
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<()>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.batch_delete(keys, &context, &options).await
        }).await
    }

    /// 开始事务
    pub async fn begin_transaction(
        &self,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<Box<dyn StorageTransaction>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.begin_transaction(&context, &options).await
        }).await
    }

    /// 执行存储计划
    pub async fn execute_plan(
        &self,
        operations: Vec<StorageOperation>,
        engine_type: EngineType,
        context: Option<StorageContext>,
        options: Option<StorageOptions>,
    ) -> Result<StorageResult<Vec<StorageOperationResult>>> {
        let context = context.unwrap_or_else(|| self.default_context.clone());
        let options = options.unwrap_or_else(|| self.default_options.clone());

        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.execute_plan(operations.clone(), &context, &options).await
        }).await
    }

    /// 健康检查
    pub async fn health_check(&self, engine_type: EngineType) -> Result<bool> {
        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.health_check().await
        }).await
    }

    /// 获取统计信息
    pub async fn get_stats(&self, engine_type: EngineType) -> Result<StorageStats> {
        self.execute_with_retry(|| async {
            let engine = self.get_engine(engine_type).await?;
            engine.get_stats().await
        }).await
    }

    /// 关闭客户端
    pub async fn shutdown(&self) -> Result<()> {
        self.factory.shutdown_all().await
    }
}
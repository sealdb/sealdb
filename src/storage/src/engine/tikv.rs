//! TiKV 存储引擎实现
//!
//! 基于 TiKV 客户端实现分布式存储引擎

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use tikv_client::{RawClient, Value as TiKVValue, TransactionClient};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::common::*;
use crate::engine::{StorageEngine, StorageTransaction};

/// TiKV 存储引擎
pub struct TiKVEngine {
    raw_client: Option<RawClient>,
    transaction_client: Option<TransactionClient>,
    config: Option<StorageConfig>,
    stats: Arc<RwLock<StorageStats>>,
    is_initialized: bool,
}

impl TiKVEngine {
    /// 创建新的 TiKV 引擎
    pub fn new() -> Self {
        Self {
            raw_client: None,
            transaction_client: None,
            config: None,
            stats: Arc::new(RwLock::new(StorageStats::default())),
            is_initialized: false,
        }
    }

    /// 获取原始客户端
    fn get_raw_client(&self) -> Result<&RawClient> {
        self.raw_client
            .as_ref()
            .ok_or_else(|| StorageError::Connection("TiKV raw client not initialized".to_string()))
    }

    /// 获取事务客户端
    fn get_transaction_client(&self) -> Result<&TransactionClient> {
        self.transaction_client
            .as_ref()
            .ok_or_else(|| StorageError::Connection("TiKV transaction client not initialized".to_string()))
    }

    /// 更新统计信息
    fn update_stats(&self, operation_success: bool, latency_ms: u64) {
        let mut stats = self.stats.write();
        stats.total_operations += 1;
        if operation_success {
            stats.successful_operations += 1;
        } else {
            stats.failed_operations += 1;
        }
        stats.total_latency_ms += latency_ms;
        stats.avg_latency_ms = stats.total_latency_ms as f64 / stats.total_operations as f64;
    }
}

#[async_trait]
impl StorageEngine for TiKVEngine {
    fn engine_type(&self) -> EngineType {
        EngineType::TiKV
    }

    fn name(&self) -> &str {
        "TiKV"
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn initialize(&mut self, config: &StorageConfig) -> Result<()> {
        if self.is_initialized {
            return Ok(());
        }

        info!("Initializing TiKV engine with config: {:?}", config);

        // 解析连接字符串
        let pd_endpoints = config.connection_string
            .split(',')
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>();

        if pd_endpoints.is_empty() {
            return Err(Error::Configuration("No PD endpoints provided".to_string()));
        }

        // 创建原始客户端
        let raw_client = match RawClient::new(pd_endpoints.clone()).await {
            Ok(client) => client,
            Err(e) => return Err(Error::Connection(format!("Failed to create TiKV raw client: {e}"))),
        };

        // 创建事务客户端
        let transaction_client = match TransactionClient::new(pd_endpoints).await {
            Ok(client) => client,
            Err(e) => return Err(Error::Connection(format!("Failed to create TiKV transaction client: {e}"))),
        };

        self.raw_client = Some(raw_client);
        self.transaction_client = Some(transaction_client);
        self.config = Some(config.clone());
        self.is_initialized = true;

        info!("TiKV engine initialized successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if !self.is_initialized {
            return Ok(());
        }

        info!("Shutting down TiKV engine");

        // 关闭客户端
        self.raw_client = None;
        self.transaction_client = None;
        self.is_initialized = false;

        info!("TiKV engine shut down successfully");
        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        if !self.is_initialized {
            return Ok(false);
        }

        // 简单的健康检查：尝试获取一个不存在的键
        let test_key = b"health_check_test_key".to_vec();
        match self.get(&test_key, &StorageContext::default(), &StorageOptions::default()).await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("TiKV health check failed: {}", e);
                Ok(false)
            }
        }
    }

    async fn get_stats(&self) -> Result<StorageStats> {
        let stats = self.stats.read();
        Ok(stats.clone())
    }

    async fn get(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Option<Value>>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_key = tikv_client::Key::from(key.clone());

        match raw_client.get(tikv_key).await {
            Ok(Some(value)) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV get success: {:?}", key);
                Ok(StorageResult::new(Some(value), latency, EngineType::TiKV))
            }
            Ok(None) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV get: key not found: {:?}", key);
                Ok(StorageResult::new(None, latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV get failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Get operation failed: {e}")))
            }
        }
    }

    async fn put(
        &self,
        key: &Key,
        value: &Value,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_key = tikv_client::Key::from(key.clone());
        let tikv_value = tikv_client::Value::from(value.as_slice());

        match raw_client.put(tikv_key, tikv_value).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV put success: {:?}", key);
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV put failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Put operation failed: {e}")))
            }
        }
    }

    async fn delete(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_key = tikv_client::Key::from(key.clone());

        match raw_client.delete(tikv_key).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV delete success: {:?}", key);
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV delete failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Delete operation failed: {e}")))
            }
        }
    }

    async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<KeyValue>>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_start_key = tikv_client::Key::from(start_key.clone());
        let tikv_end_key = tikv_client::Key::from(end_key.clone());

        match raw_client.scan(tikv_start_key..tikv_end_key, limit).await {
            Ok(pairs) => {
                let result: Vec<KeyValue> = pairs
                    .into_iter()
                    .map(|pair| (pair.key().clone().into(), pair.value().clone()))
                    .collect();

                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV scan success, found {} pairs", result.len());
                Ok(StorageResult::new(result, latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV scan failed: {}", e);
                Err(Error::Engine(format!("Scan operation failed: {e}")))
            }
        }
    }

    async fn batch_get(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<HashMap<Key, Option<Value>>>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_keys: Vec<tikv_client::Key> = keys.iter().map(|k| tikv_client::Key::from(k.clone())).collect();

        match raw_client.batch_get(tikv_keys).await {
            Ok(pairs) => {
                let mut result = HashMap::new();
                for pair in pairs {
                    result.insert(pair.key().clone().into(), Some(pair.value().clone()));
                }

                // 对于没有返回的键，设置为 None
                for key in keys {
                    result.entry(key.clone()).or_insert(None);
                }

                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV batch_get success, processed {} keys", keys.len());
                Ok(StorageResult::new(result, latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV batch_get failed: {}", e);
                Err(Error::Engine(format!("Batch get operation failed: {e}")))
            }
        }
    }

    async fn batch_put(
        &self,
        key_values: &[KeyValue],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_pairs: Vec<(tikv_client::Key, TiKVValue)> = key_values
            .iter()
            .map(|(k, v)| (tikv_client::Key::from(k.clone()), TiKVValue::from(v.as_slice())))
            .collect();

        match raw_client.batch_put(tikv_pairs).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV batch_put success, processed {} pairs", key_values.len());
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV batch_put failed: {}", e);
                Err(Error::Engine(format!("Batch put operation failed: {e}")))
            }
        }
    }

    async fn batch_delete(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();

        let raw_client = self.get_raw_client()?;
        let tikv_keys: Vec<tikv_client::Key> = keys.iter().map(|k| tikv_client::Key::from(k.clone())).collect();

        match raw_client.batch_delete(tikv_keys).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(true, latency);
                debug!("TiKV batch_delete success, processed {} keys", keys.len());
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                self.update_stats(false, latency);
                error!("TiKV batch_delete failed: {}", e);
                Err(StorageError::Engine(format!("Batch delete operation failed: {e}")).into())
            }
        }
    }

    async fn begin_transaction(
        &self,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<Box<dyn StorageTransaction>> {
        let transaction_client = self.get_transaction_client()?;

        // 暂时返回错误，因为 TiKV 客户端的事务 API 可能需要不同的调用方式
        Err(Error::TransactionConflict("TiKV transaction API not implemented yet".to_string()))
    }

    async fn execute_plan(
        &self,
        operations: Vec<StorageOperation>,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<StorageOperationResult>>> {
        let start_time = std::time::Instant::now();
        let mut results = Vec::new();
        let mut total_rows = 0;

        // 执行计划中的每个操作
        for operation in operations {
            let operation_start = std::time::Instant::now();
            let mut operation_result = StorageOperationResult {
                operation_type: operation.operation_type,
                key: operation.key.clone(),
                value: operation.value.clone(),
                success: false,
                error_message: None,
                latency_ms: 0,
            };

            match operation.operation_type {
                OperationType::Get => {
                    if let Some(key) = &operation.key {
                        match self.get(key, context, options).await {
                            Ok(result) => {
                                operation_result.success = true;
                                operation_result.latency_ms = result.latency_ms;
                            }
                            Err(e) => {
                                operation_result.error_message = Some(e.to_string());
                            }
                        }
                    }
                }
                OperationType::Put => {
                    if let (Some(key), Some(value)) = (&operation.key, &operation.value) {
                        match self.put(key, value, context, options).await {
                            Ok(result) => {
                                operation_result.success = true;
                                operation_result.latency_ms = result.latency_ms;
                            }
                            Err(e) => {
                                operation_result.error_message = Some(e.to_string());
                            }
                        }
                    }
                }
                OperationType::Delete => {
                    if let Some(key) = &operation.key {
                        match self.delete(key, context, options).await {
                            Ok(result) => {
                                operation_result.success = true;
                                operation_result.latency_ms = result.latency_ms;
                            }
                            Err(e) => {
                                operation_result.error_message = Some(e.to_string());
                            }
                        }
                    }
                }
                OperationType::Scan => {
                    if let (Some(start_key), Some(end_key)) = (&operation.start_key, &operation.end_key) {
                        let limit = operation.limit.unwrap_or(1000);
                        match self.scan(start_key, end_key, limit, context, options).await {
                            Ok(result) => {
                                operation_result.success = true;
                                operation_result.latency_ms = result.latency_ms;
                            }
                            Err(e) => {
                                operation_result.error_message = Some(e.to_string());
                            }
                        }
                    }
                }
                _ => {
                    operation_result.error_message = Some("Unsupported operation type".to_string());
                }
            }

            results.push(operation_result);
        }

        let total_latency = start_time.elapsed().as_millis() as u64;
        Ok(StorageResult::new(results, total_latency, EngineType::TiKV))
    }
}

/// TiKV 事务实现
pub struct TiKVTransaction {
    transaction: tikv_client::Transaction,
    transaction_id: String,
    context: StorageContext,
}

impl TiKVTransaction {
    pub fn new(transaction: tikv_client::Transaction, context: StorageContext) -> Self {
        Self {
            transaction,
            transaction_id: Uuid::new_v4().to_string(),
            context,
        }
    }
}

#[async_trait]
impl StorageTransaction for TiKVTransaction {
    fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    async fn commit(&mut self) -> Result<()> {
        match self.transaction.commit().await {
            Ok(_) => {
                debug!("TiKV transaction committed: {}", self.transaction_id);
                Ok(())
            }
            Err(e) => {
                error!("TiKV transaction commit failed: {}, error: {}", self.transaction_id, e);
                Err(Error::TransactionConflict(format!("Commit failed: {e}")))
            }
        }
    }

    async fn rollback(&mut self) -> Result<()> {
        match self.transaction.rollback().await {
            Ok(_) => {
                debug!("TiKV transaction rolled back: {}", self.transaction_id);
                Ok(())
            }
            Err(e) => {
                error!("TiKV transaction rollback failed: {}, error: {}", self.transaction_id, e);
                Err(Error::TransactionConflict(format!("Rollback failed: {e}")))
            }
        }
    }

    async fn get(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> Result<StorageResult<Option<Value>>> {
        let start_time = std::time::Instant::now();
        let tikv_key = tikv_client::Key::from(key.clone());

        match self.transaction.get(tikv_key).await {
            Ok(Some(value)) => {
                let latency = start_time.elapsed().as_millis() as u64;
                debug!("TiKV transaction get success: {:?}", key);
                Ok(StorageResult::new(Some(value), latency, EngineType::TiKV))
            }
            Ok(None) => {
                let latency = start_time.elapsed().as_millis() as u64;
                debug!("TiKV transaction get: key not found: {:?}", key);
                Ok(StorageResult::new(None, latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                error!("TiKV transaction get failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Transaction get failed: {e}")))
            }
        }
    }

    async fn put(
        &mut self,
        key: &Key,
        value: &Value,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();
        let tikv_key = tikv_client::Key::from(key.clone());
        let tikv_value = TiKVValue::from(value.as_slice());

        match self.transaction.put(tikv_key, tikv_value).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                debug!("TiKV transaction put success: {:?}", key);
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                error!("TiKV transaction put failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Transaction put failed: {e}")))
            }
        }
    }

    async fn delete(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>> {
        let start_time = std::time::Instant::now();
        let tikv_key = tikv_client::Key::from(key.clone());

        match self.transaction.delete(tikv_key).await {
            Ok(_) => {
                let latency = start_time.elapsed().as_millis() as u64;
                debug!("TiKV transaction delete success: {:?}", key);
                Ok(StorageResult::new((), latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                error!("TiKV transaction delete failed: {:?}, error: {}", key, e);
                Err(Error::Engine(format!("Transaction delete failed: {e}")))
            }
        }
    }

    async fn scan(
        &mut self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<KeyValue>>> {
        let start_time = std::time::Instant::now();
        let tikv_start_key = tikv_client::Key::from(start_key.clone());
        let tikv_end_key = tikv_client::Key::from(end_key.clone());

        match self.transaction.scan(tikv_start_key..tikv_end_key, limit).await {
            Ok(pairs) => {
                let result: Vec<KeyValue> = pairs
                    .into_iter()
                    .map(|pair| (pair.key().clone().into(), pair.value().clone()))
                    .collect();

                let latency = start_time.elapsed().as_millis() as u64;
                debug!("TiKV transaction scan success, found {} pairs", result.len());
                Ok(StorageResult::new(result, latency, EngineType::TiKV))
            }
            Err(e) => {
                let latency = start_time.elapsed().as_millis() as u64;
                error!("TiKV transaction scan failed: {}", e);
                Err(Error::Engine(format!("Transaction scan failed: {e}")))
            }
        }
    }
}


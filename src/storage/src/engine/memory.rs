//! 内存存储引擎实现
//!
//! 基于内存的存储引擎，用于测试和开发

use async_trait::async_trait;

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

use crate::common::*;
use crate::engine::{StorageEngine, StorageTransaction};

/// 内存存储引擎
pub struct MemoryEngine {
    data: Arc<RwLock<HashMap<Key, Value>>>,
    config: Option<StorageConfig>,
    stats: Arc<RwLock<StorageStats>>,
    is_initialized: bool,
}

impl MemoryEngine {
    /// 创建新的内存引擎
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            config: None,
            stats: Arc::new(RwLock::new(StorageStats::default())),
            is_initialized: false,
        }
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
impl StorageEngine for MemoryEngine {
    fn engine_type(&self) -> EngineType {
        EngineType::Memory
    }

    fn name(&self) -> &str {
        "Memory"
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn initialize(&mut self, config: &StorageConfig) -> std::result::Result<(), StorageError> {
        if self.is_initialized {
            return Ok(());
        }

        info!("Initializing Memory engine");
        self.config = Some(config.clone());
        self.is_initialized = true;
        info!("Memory engine initialized successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> std::result::Result<(), StorageError> {
        if !self.is_initialized {
            return Ok(());
        }

        info!("Shutting down Memory engine");
        let mut data = self.data.write();
        data.clear();
        self.is_initialized = false;
        info!("Memory engine shut down successfully");
        Ok(())
    }

    async fn health_check(&self) -> std::result::Result<bool, StorageError> {
        Ok(self.is_initialized)
    }

    async fn get_stats(&self) -> std::result::Result<StorageStats, StorageError> {
        let stats = self.stats.read();
        Ok(stats.clone())
    }

    async fn get(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Option<Value>>, StorageError> {
        let start_time = std::time::Instant::now();

        let data = self.data.read();
        let result = data.get(key).cloned();

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory get: {:?} -> {:?}", key, result.is_some());
        Ok(StorageResult::new(result, latency, EngineType::Memory))
    }

    async fn put(
        &self,
        key: &Key,
        value: &Value,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        let mut data = self.data.write();
        data.insert(key.clone(), value.clone());

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory put: {:?}", key);
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn delete(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        let mut data = self.data.write();
        data.remove(key);

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory delete: {:?}", key);
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<KeyValue>>, StorageError> {
        let start_time = std::time::Instant::now();

        let data = self.data.read();
        let mut result = Vec::new();

        for (key, value) in data.iter() {
            if key >= start_key && key < end_key {
                result.push((key.clone(), value.clone()));
                if result.len() >= limit as usize {
                    break;
                }
            }
        }

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory scan: found {} pairs", result.len());
        Ok(StorageResult::new(result, latency, EngineType::Memory))
    }

    async fn batch_get(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<HashMap<Key, Option<Value>>>, StorageError> {
        let start_time = std::time::Instant::now();

        let data = self.data.read();
        let mut result = HashMap::new();

        for key in keys {
            result.insert(key.clone(), data.get(key).cloned());
        }

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory batch_get: processed {} keys", keys.len());
        Ok(StorageResult::new(result, latency, EngineType::Memory))
    }

    async fn batch_put(
        &self,
        key_values: &[KeyValue],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        let mut data = self.data.write();
        for (key, value) in key_values {
            data.insert(key.clone(), value.clone());
        }

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory batch_put: processed {} pairs", key_values.len());
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn batch_delete(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        let mut data = self.data.write();
        for key in keys {
            data.remove(key);
        }

        let latency = start_time.elapsed().as_millis() as u64;
        self.update_stats(true, latency);
        debug!("Memory batch_delete: processed {} keys", keys.len());
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn begin_transaction(
        &self,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<Box<dyn StorageTransaction>, StorageError> {
        let transaction = MemoryTransaction::new(
            self.data.clone(),
            context.clone(),
        );
        Ok(Box::new(transaction))
    }

    async fn execute_plan(
        &self,
        operations: Vec<StorageOperation>,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<StorageOperationResult>>, StorageError> {
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
        Ok(StorageResult::new(results, total_latency, EngineType::Memory))
    }
}

/// 内存事务实现
pub struct MemoryTransaction {
    data: Arc<RwLock<HashMap<Key, Value>>>,
    transaction_id: String,
    context: StorageContext,
    pending_changes: HashMap<Key, Option<Value>>, // None 表示删除
}

impl MemoryTransaction {
    pub fn new(data: Arc<RwLock<HashMap<Key, Value>>>, context: StorageContext) -> Self {
        Self {
            data,
            transaction_id: Uuid::new_v4().to_string(),
            context,
            pending_changes: HashMap::new(),
        }
    }
}

#[async_trait]
impl StorageTransaction for MemoryTransaction {
    fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    async fn commit(&mut self) -> std::result::Result<(), StorageError> {
        let mut data = self.data.write();
        for (key, value_opt) in self.pending_changes.drain() {
            match value_opt {
                Some(value) => data.insert(key, value),
                None => data.remove(&key),
            };
        }
        debug!("Memory transaction committed: {}", self.transaction_id);
        Ok(())
    }

    async fn rollback(&mut self) -> std::result::Result<(), StorageError> {
        self.pending_changes.clear();
        debug!("Memory transaction rolled back: {}", self.transaction_id);
        Ok(())
    }

    async fn get(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Option<Value>>, StorageError> {
        let start_time = std::time::Instant::now();

        // 先检查待处理的更改
        if let Some(value_opt) = self.pending_changes.get(key) {
            let latency = start_time.elapsed().as_millis() as u64;
            return Ok(StorageResult::new(value_opt.clone(), latency, EngineType::Memory));
        }

        // 从主数据中获取
        let data = self.data.read();
        let result = data.get(key).cloned();

        let latency = start_time.elapsed().as_millis() as u64;
        debug!("Memory transaction get: {:?} -> {:?}", key, result.is_some());
        Ok(StorageResult::new(result, latency, EngineType::Memory))
    }

    async fn put(
        &mut self,
        key: &Key,
        value: &Value,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        self.pending_changes.insert(key.clone(), Some(value.clone()));

        let latency = start_time.elapsed().as_millis() as u64;
        debug!("Memory transaction put: {:?}", key);
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn delete(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError> {
        let start_time = std::time::Instant::now();

        self.pending_changes.insert(key.clone(), None);

        let latency = start_time.elapsed().as_millis() as u64;
        debug!("Memory transaction delete: {:?}", key);
        Ok(StorageResult::new((), latency, EngineType::Memory))
    }

    async fn scan(
        &mut self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<KeyValue>>, StorageError> {
        let start_time = std::time::Instant::now();

        let data = self.data.read();
        let mut result = Vec::new();

        for (key, value) in data.iter() {
            if key >= start_key && key < end_key {
                // 检查是否有待处理的更改
                if let Some(pending_value) = self.pending_changes.get(key) {
                    match pending_value {
                        Some(value) => result.push((key.clone(), value.clone())),
                        None => continue, // 跳过已删除的键
                    }
                } else {
                    result.push((key.clone(), value.clone()));
                }

                if result.len() >= limit as usize {
                    break;
                }
            }
        }

        let latency = start_time.elapsed().as_millis() as u64;
        debug!("Memory transaction scan: found {} pairs", result.len());
        Ok(StorageResult::new(result, latency, EngineType::Memory))
    }
}
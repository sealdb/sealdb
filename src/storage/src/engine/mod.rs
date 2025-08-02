//! 存储引擎抽象层
//! 
//! 提供统一的存储引擎接口，支持多种存储引擎的插件化实现

use async_trait::async_trait;
use common::Result;
use std::collections::HashMap;

use crate::common::*;

pub mod tikv;
pub mod memory;
pub mod factory;

pub use factory::StorageEngineFactory;
pub use tikv::TiKVEngine;
pub use memory::MemoryEngine;

/// 存储引擎 trait
/// 
/// 所有存储引擎都必须实现这个 trait，提供统一的存储操作接口
#[async_trait]
pub trait StorageEngine: Send + Sync {
    /// 引擎类型
    fn engine_type(&self) -> EngineType;
    
    /// 引擎名称
    fn name(&self) -> &str;
    
    /// 引擎版本
    fn version(&self) -> &str;
    
    /// 初始化引擎
    async fn initialize(&mut self, config: &StorageConfig) -> Result<()>;
    
    /// 关闭引擎
    async fn shutdown(&mut self) -> Result<()>;
    
    /// 健康检查
    async fn health_check(&self) -> Result<bool>;
    
    /// 获取统计信息
    async fn get_stats(&self) -> Result<StorageStats>;
    
    /// 获取键值
    async fn get(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Option<Value>>>;
    
    /// 设置键值
    async fn put(
        &self,
        key: &Key,
        value: &Value,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 删除键
    async fn delete(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 扫描键值
    async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<KeyValue>>>;
    
    /// 批量获取
    async fn batch_get(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<HashMap<Key, Option<Value>>>>;
    
    /// 批量设置
    async fn batch_put(
        &self,
        key_values: &[KeyValue],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 批量删除
    async fn batch_delete(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 开始事务
    async fn begin_transaction(
        &self,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<Box<dyn StorageTransaction>>;
    
    /// 执行存储计划（简化版本）
    async fn execute_plan(
        &self,
        operations: Vec<StorageOperation>,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<StorageOperationResult>>>;
}

/// 存储事务 trait
#[async_trait]
pub trait StorageTransaction: Send + Sync {
    /// 事务 ID
    fn transaction_id(&self) -> &str;
    
    /// 提交事务
    async fn commit(&mut self) -> Result<()>;
    
    /// 回滚事务
    async fn rollback(&mut self) -> Result<()>;
    
    /// 获取键值
    async fn get(
        &self,
        key: &Key,
        options: &StorageOptions,
    ) -> Result<StorageResult<Option<Value>>>;
    
    /// 设置键值
    async fn put(
        &mut self,
        key: &Key,
        value: &Value,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 删除键
    async fn delete(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> Result<StorageResult<()>>;
    
    /// 扫描键值
    async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        options: &StorageOptions,
    ) -> Result<StorageResult<Vec<KeyValue>>>;
}

/// 存储计划
#[derive(Debug, Clone)]
pub struct StoragePlan {
    pub plan_id: String,
    pub operations: Vec<StorageOperation>,
    pub dependencies: Vec<String>,
    pub estimated_cost: f64,
    pub estimated_rows: u64,
}

/// 存储操作
#[derive(Debug, Clone)]
pub struct StorageOperation {
    pub operation_id: String,
    pub operation_type: OperationType,
    pub keys: Vec<Key>,
    pub values: Vec<Value>,
    pub start_key: Option<Key>,
    pub end_key: Option<Key>,
    pub limit: Option<u32>,
    pub options: StorageOptions,
}

/// 存储操作结果
#[derive(Debug, Clone)]
pub struct StorageOperationResult {
    pub operation_id: String,
    pub operation_type: OperationType,
    pub success: bool,
    pub latency_ms: u64,
    pub rows_affected: u64,
    pub error: Option<String>,
    pub data: Option<Vec<KeyValue>>,
} 
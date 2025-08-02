//! 存储引擎抽象层
//!
//! 提供统一的存储引擎接口，支持多种存储引擎的插件化实现

use async_trait::async_trait;
use std::collections::HashMap;

use crate::common::{StorageConfig, StorageContext, StorageOptions, StorageResult, StorageStats, StorageOperation, StorageOperationResult, Key, Value, KeyValue, EngineType, StorageError};

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
    async fn initialize(&mut self, config: &StorageConfig) -> std::result::Result<(), StorageError>;

    /// 关闭引擎
    async fn shutdown(&mut self) -> std::result::Result<(), StorageError>;

    /// 健康检查
    async fn health_check(&self) -> std::result::Result<bool, StorageError>;

    /// 获取统计信息
    async fn get_stats(&self) -> std::result::Result<StorageStats, StorageError>;

        /// 获取键值
    async fn get(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Option<Value>>, StorageError>;
    
    /// 设置键值
    async fn put(
        &self,
        key: &Key,
        value: &Value,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 删除键
    async fn delete(
        &self,
        key: &Key,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 扫描键值
    async fn scan(
        &self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<KeyValue>>, StorageError>;

        /// 批量获取
    async fn batch_get(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<HashMap<Key, Option<Value>>>, StorageError>;
    
    /// 批量设置
    async fn batch_put(
        &self,
        key_values: &[KeyValue],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 批量删除
    async fn batch_delete(
        &self,
        keys: &[Key],
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 开始事务
    async fn begin_transaction(
        &self,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<Box<dyn StorageTransaction>, StorageError>;
    
    /// 执行存储计划（简化版本）
    async fn execute_plan(
        &self,
        operations: Vec<StorageOperation>,
        context: &StorageContext,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<StorageOperationResult>>, StorageError>;
}

/// 存储事务 trait
#[async_trait]
pub trait StorageTransaction: Send + Sync {
    /// 事务 ID
    fn transaction_id(&self) -> &str;

        /// 提交事务
    async fn commit(&mut self) -> std::result::Result<(), StorageError>;
    
    /// 回滚事务
    async fn rollback(&mut self) -> std::result::Result<(), StorageError>;
    
    /// 获取键值
    async fn get(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Option<Value>>, StorageError>;
    
    /// 设置键值
    async fn put(
        &mut self,
        key: &Key,
        value: &Value,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 删除键
    async fn delete(
        &mut self,
        key: &Key,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<()>, StorageError>;
    
    /// 扫描键值
    async fn scan(
        &mut self,
        start_key: &Key,
        end_key: &Key,
        limit: u32,
        options: &StorageOptions,
    ) -> std::result::Result<StorageResult<Vec<KeyValue>>, StorageError>;
}


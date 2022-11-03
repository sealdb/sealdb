//! 存储感知的执行器
//!
//! 集成存储处理器，将 SQL 执行计划转换为存储操作


use common::Result;
use tracing::{debug, info};

use crate::executor::execution_models::QueryResult;
use crate::executor::executor::ExecutionContext;
use crate::storage::handler::StorageHandler;
use storage::EngineType;

/// 存储感知的执行器
pub struct StorageExecutor {
    storage_handler: StorageHandler,
    default_engine_type: EngineType,
}

impl StorageExecutor {
    /// 创建新的存储感知执行器
    pub fn new() -> Self {
        Self {
            storage_handler: StorageHandler::new(),
            default_engine_type: EngineType::Memory, // 默认使用内存引擎
        }
    }

    /// 设置默认存储引擎
    pub fn set_default_engine(&mut self, engine_type: EngineType) {
        self.default_engine_type = engine_type;
        self.storage_handler.set_default_engine(engine_type);
    }

    /// 注册存储引擎
    pub async fn register_storage_engine(&self, engine_type: EngineType, config: storage::StorageConfig) -> Result<()> {
        self.storage_handler.register_engine(engine_type, config).await
    }

    /// 执行表扫描
    pub async fn execute_table_scan(
        &self,
        table_name: &str,
        columns: &[String],
        limit: Option<u32>,
        _context: &ExecutionContext,
    ) -> Result<QueryResult> {
        info!("执行表扫描: {}", table_name);

        let engine_type = self.default_engine_type; // TODO: 从 context 获取引擎类型
        let result = self.storage_handler.scan_table(table_name, columns, limit, Some(engine_type)).await?;

        debug!("表扫描完成: {} 行", result.rows.len());
        Ok(result)
    }

    /// 执行点查询
    pub async fn execute_point_query(
        &self,
        table_name: &str,
        key: &str,
        _context: &ExecutionContext,
    ) -> Result<QueryResult> {
        info!("执行点查询: {} -> {}", table_name, key);

        let engine_type = self.default_engine_type; // TODO: 从 context 获取引擎类型
        let result = self.storage_handler.point_query(table_name, key, Some(engine_type)).await?;

        debug!("点查询完成: {} 行", result.rows.len());
        Ok(result)
    }

    /// 执行插入操作
    pub async fn execute_insert(
        &self,
        table_name: &str,
        key: &str,
        value: &str,
        _context: &ExecutionContext,
    ) -> Result<u64> {
        info!("执行插入: {} -> {}", table_name, key);

        let engine_type = self.default_engine_type; // TODO: 从 context 获取引擎类型
        let result = self.storage_handler.insert_row(table_name, key, value, Some(engine_type)).await?;

        debug!("插入完成: {} 行", result);
        Ok(result)
    }

    /// 执行删除操作
    pub async fn execute_delete(
        &self,
        table_name: &str,
        key: &str,
        _context: &ExecutionContext,
    ) -> Result<u64> {
        info!("执行删除: {} -> {}", table_name, key);

        let engine_type = self.default_engine_type; // TODO: 从 context 获取引擎类型
        let result = self.storage_handler.delete_row(table_name, key, Some(engine_type)).await?;

        debug!("删除完成: {} 行", result);
        Ok(result)
    }

    /// 执行批量操作
    pub async fn execute_batch_operations(
        &self,
        operations: Vec<StorageOperation>,
        _context: &ExecutionContext,
    ) -> Result<Vec<StorageOperationResult>> {
        info!("执行批量操作: {} 个操作", operations.len());

        let mut results = Vec::new();
        let engine_type = self.default_engine_type; // TODO: 从 context 获取引擎类型

        for operation in operations {
            let result = match operation.operation_type {
                StorageOperationType::Insert => {
                    let key = operation.key.unwrap_or_default();
                    let value = operation.value.unwrap_or_default();
                    let table_name = operation.table_name.unwrap_or_default();

                    match self.storage_handler.insert_row(&table_name, &key, &value, Some(engine_type)).await {
                        Ok(affected_rows) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: true,
                            affected_rows,
                            error: None,
                        },
                        Err(e) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: false,
                            affected_rows: 0,
                            error: Some(e.to_string()),
                        },
                    }
                }
                StorageOperationType::Delete => {
                    let key = operation.key.unwrap_or_default();
                    let table_name = operation.table_name.unwrap_or_default();

                    match self.storage_handler.delete_row(&table_name, &key, Some(engine_type)).await {
                        Ok(affected_rows) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: true,
                            affected_rows,
                            error: None,
                        },
                        Err(e) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: false,
                            affected_rows: 0,
                            error: Some(e.to_string()),
                        },
                    }
                }
                StorageOperationType::Select => {
                    let key = operation.key.unwrap_or_default();
                    let table_name = operation.table_name.unwrap_or_default();

                    match self.storage_handler.point_query(&table_name, &key, Some(engine_type)).await {
                        Ok(query_result) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: true,
                            affected_rows: query_result.rows.len() as u64,
                            error: None,
                        },
                        Err(e) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: false,
                            affected_rows: 0,
                            error: Some(e.to_string()),
                        },
                    }
                }
                StorageOperationType::Update => {
                    let key = operation.key.unwrap_or_default();
                    let value = operation.value.unwrap_or_default();
                    let table_name = operation.table_name.unwrap_or_default();

                    match self.storage_handler.insert_row(&table_name, &key, &value, Some(engine_type)).await {
                        Ok(affected_rows) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: true,
                            affected_rows,
                            error: None,
                        },
                        Err(e) => StorageOperationResult {
                            operation_id: operation.operation_id,
                            success: false,
                            affected_rows: 0,
                            error: Some(e.to_string()),
                        },
                    }
                }
            };

            results.push(result);
        }

        debug!("批量操作完成: {} 个结果", results.len());
        Ok(results)
    }
}

impl Default for StorageExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// 存储操作类型
#[derive(Debug, Clone)]
pub enum StorageOperationType {
    Insert,
    Delete,
    Select,
    Update,
}

/// 存储操作
#[derive(Debug, Clone)]
pub struct StorageOperation {
    pub operation_id: String,
    pub operation_type: StorageOperationType,
    pub table_name: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
}

/// 存储操作结果
#[derive(Debug, Clone)]
pub struct StorageOperationResult {
    pub operation_id: String,
    pub success: bool,
    pub affected_rows: u64,
    pub error: Option<String>,
}
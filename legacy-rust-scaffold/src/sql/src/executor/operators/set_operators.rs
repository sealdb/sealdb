use common::Result;
use std::sync::Arc;
use std::collections::HashMap;
use std::collections::HashSet;
use async_trait::async_trait;
use tracing::{debug, info, warn};
use std::time::Duration;
use tokio::time;

use crate::executor::execution_models::QueryResult;
use crate::storage::buffer_pool::{BufferPool, PageId};
use crate::storage::memory::MemoryManager;
use crate::storage::worker_pool::WorkerPool;
use super::operator_trait::Operator;

/// Union操作符
#[derive(Debug)]
pub struct UnionOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub distinct: bool, // 是否去重
}

impl UnionOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            memory_manager,
            buffer_pool,
            distinct: false,
        }
    }

    pub fn set_distinct(&mut self, distinct: bool) {
        self.distinct = distinct;
    }

    async fn perform_union(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing union operation with distinct: {}", self.distinct);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let mut union_rows = Vec::new();

        // 添加左表数据
        union_rows.extend(left_data.rows);

        // 添加右表数据
        union_rows.extend(right_data.rows);

        // 如果需要去重
        if self.distinct {
            let mut seen = HashSet::new();
            let mut distinct_rows = Vec::new();

            for row in union_rows {
                let row_key = self.create_row_key(&row);
                if !seen.contains(&row_key) {
                    seen.insert(row_key);
                    distinct_rows.push(row);
                }
            }

            union_rows = distinct_rows;
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(union_rows)
    }

    fn create_row_key(&self, row: &[String]) -> String {
        row.join("|")
    }
}

#[async_trait]
impl Operator for UnionOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing union operation");

        // 模拟左表数据
        let left_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        // 模拟右表数据
        let right_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let union_rows = self.perform_union(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = union_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Union completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// Intersect操作符
#[derive(Debug)]
pub struct IntersectOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
}

impl IntersectOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            memory_manager,
            buffer_pool,
        }
    }

    async fn perform_intersect(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing intersect operation");

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 构建右表的哈希集合
        let mut right_set = HashSet::new();
        for row in &right_data.rows {
            let row_key = self.create_row_key(row);
            right_set.insert(row_key);
        }

        // 查找交集
        let mut intersect_rows = Vec::new();
        for row in &left_data.rows {
            let row_key = self.create_row_key(row);
            if right_set.contains(&row_key) {
                intersect_rows.push(row.clone());
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(intersect_rows)
    }

    fn create_row_key(&self, row: &[String]) -> String {
        row.join("|")
    }
}

#[async_trait]
impl Operator for IntersectOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing intersect operation");

        // 模拟左表数据
        let left_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        // 模拟右表数据
        let right_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let intersect_rows = self.perform_intersect(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = intersect_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Intersect completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// Except操作符
#[derive(Debug)]
pub struct ExceptOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
}

impl ExceptOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            memory_manager,
            buffer_pool,
        }
    }

    async fn perform_except(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing except operation");

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 构建右表的哈希集合
        let mut right_set = HashSet::new();
        for row in &right_data.rows {
            let row_key = self.create_row_key(row);
            right_set.insert(row_key);
        }

        // 查找差集
        let mut except_rows = Vec::new();
        for row in &left_data.rows {
            let row_key = self.create_row_key(row);
            if !right_set.contains(&row_key) {
                except_rows.push(row.clone());
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(except_rows)
    }

    fn create_row_key(&self, row: &[String]) -> String {
        row.join("|")
    }
}

#[async_trait]
impl Operator for ExceptOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing except operation");

        // 模拟左表数据
        let left_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        // 模拟右表数据
        let right_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let except_rows = self.perform_except(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = except_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Except completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}
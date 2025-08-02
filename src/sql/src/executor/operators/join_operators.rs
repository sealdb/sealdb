use common::Result;
use std::sync::Arc;
use std::collections::HashMap;
use async_trait::async_trait;
use tracing::{debug, info, warn};
use std::time::Duration;
use tokio::time;

use crate::executor::execution_models::QueryResult;
use crate::storage::buffer_pool::{BufferPool, PageId};
use crate::storage::memory::MemoryManager;
use crate::storage::worker_pool::WorkerPool;
use super::operator_trait::Operator;

/// 连接操作符
#[derive(Debug)]
pub struct JoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
    pub memory_manager: Arc<MemoryManager>,
}

impl JoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
            memory_manager,
        }
    }

    async fn perform_join(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing {} join with condition: {}", self.join_type, self.condition);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let join_rows = match self.join_type.to_lowercase().as_str() {
            "inner" => self.inner_join(&left_data, &right_data).await?,
            "left" => self.left_join(&left_data, &right_data).await?,
            "right" => self.right_join(&left_data, &right_data).await?,
            "full" => self.full_join(&left_data, &right_data).await?,
            _ => {
                warn!("Unknown join type: {}, falling back to inner join", self.join_type);
                self.inner_join(&left_data, &right_data).await?
            }
        };

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(join_rows)
    }

    async fn inner_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut join_rows = Vec::new();

        for left_row in &left.rows {
            for right_row in &right.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    join_rows.push(joined_row);
                }
            }
        }

        Ok(join_rows)
    }

    async fn left_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut join_rows = Vec::new();

        for left_row in &left.rows {
            let mut matched = false;
            for right_row in &right.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    join_rows.push(joined_row);
                    matched = true;
                }
            }
            if !matched {
                let mut joined_row = left_row.clone();
                joined_row.extend(vec![String::new(); right.columns.len()]);
                join_rows.push(joined_row);
            }
        }

        Ok(join_rows)
    }

    async fn right_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut join_rows = Vec::new();

        for right_row in &right.rows {
            let mut matched = false;
            for left_row in &left.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    join_rows.push(joined_row);
                    matched = true;
                }
            }
            if !matched {
                let mut joined_row = vec![String::new(); left.columns.len()];
                joined_row.extend(right_row.clone());
                join_rows.push(joined_row);
            }
        }

        Ok(join_rows)
    }

    async fn full_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut join_rows = Vec::new();
        let mut left_matched = vec![false; left.rows.len()];
        let mut right_matched = vec![false; right.rows.len()];

        // 执行内连接
        for (i, left_row) in left.rows.iter().enumerate() {
            for (j, right_row) in right.rows.iter().enumerate() {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    join_rows.push(joined_row);
                    left_matched[i] = true;
                    right_matched[j] = true;
                }
            }
        }

        // 添加左表未匹配的行
        for (i, left_row) in left.rows.iter().enumerate() {
            if !left_matched[i] {
                let mut joined_row = left_row.clone();
                joined_row.extend(vec![String::new(); right.columns.len()]);
                join_rows.push(joined_row);
            }
        }

        // 添加右表未匹配的行
        for (j, right_row) in right.rows.iter().enumerate() {
            if !right_matched[j] {
                let mut joined_row = vec![String::new(); left.columns.len()];
                joined_row.extend(right_row.clone());
                join_rows.push(joined_row);
            }
        }

        Ok(join_rows)
    }

    fn matches_join_condition(&self, left_row: &[String], right_row: &[String]) -> Result<bool> {
        // 简单的连接条件匹配，假设连接条件是第一列相等
        if !left_row.is_empty() && !right_row.is_empty() {
            Ok(left_row[0] == right_row[0])
        } else {
            Ok(false)
        }
    }
}

#[async_trait]
impl Operator for JoinOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing {} join operation", self.join_type);

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
            columns: vec!["id".to_string(), "dept".to_string(), "salary".to_string()],
            rows: vec![
                vec!["1".to_string(), "IT".to_string(), "5000".to_string()],
                vec!["2".to_string(), "HR".to_string(), "4000".to_string()],
                vec!["4".to_string(), "Finance".to_string(), "6000".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let join_rows = self.perform_join(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string(), 
                             "id".to_string(), "dept".to_string(), "salary".to_string()];
        result.rows = join_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Join completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 嵌套循环连接操作符
#[derive(Debug)]
pub struct NestedLoopJoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub batch_size: usize,
}

impl NestedLoopJoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
            memory_manager,
            buffer_pool,
            batch_size: 1000,
        }
    }

    pub fn set_batch_size(&mut self, batch_size: usize) {
        self.batch_size = batch_size;
    }

    async fn perform_nested_loop_join(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing nested loop join with batch size: {}", self.batch_size);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let mut join_rows = Vec::new();

        // 分批处理左表数据
        for left_batch in left_data.rows.chunks(self.batch_size) {
            for left_row in left_batch {
                for right_row in &right_data.rows {
                    if self.matches_join_condition(left_row, right_row)? {
                        let mut joined_row = left_row.clone();
                        joined_row.extend(right_row.clone());
                        join_rows.push(joined_row);
                    }
                }
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(join_rows)
    }

    fn matches_join_condition(&self, left_row: &[String], right_row: &[String]) -> Result<bool> {
        // 简单的连接条件匹配
        if !left_row.is_empty() && !right_row.is_empty() {
            Ok(left_row[0] == right_row[0])
        } else {
            Ok(false)
        }
    }
}

#[async_trait]
impl Operator for NestedLoopJoinOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing nested loop join operation");

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
            columns: vec!["id".to_string(), "dept".to_string(), "salary".to_string()],
            rows: vec![
                vec!["1".to_string(), "IT".to_string(), "5000".to_string()],
                vec!["2".to_string(), "HR".to_string(), "4000".to_string()],
                vec!["4".to_string(), "Finance".to_string(), "6000".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let join_rows = self.perform_nested_loop_join(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string(), 
                             "id".to_string(), "dept".to_string(), "salary".to_string()];
        result.rows = join_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Nested loop join completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// Hash连接操作符
#[derive(Debug)]
pub struct HashJoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub hash_table_size: usize,
    pub join_keys: Vec<String>,
}

impl HashJoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
            memory_manager,
            buffer_pool,
            hash_table_size: 10000,
            join_keys: vec!["id".to_string()],
        }
    }

    pub fn set_hash_table_size(&mut self, size: usize) {
        self.hash_table_size = size;
    }

    pub fn set_join_keys(&mut self, keys: Vec<String>) {
        self.join_keys = keys;
    }

    async fn perform_hash_join(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing hash join with hash table size: {}", self.hash_table_size);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 构建左表的哈希表
        let mut hash_table: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        for left_row in &left_data.rows {
            let join_key = self.extract_join_key(left_row, &left_data.columns)?;
            hash_table.entry(join_key).or_insert_with(Vec::new).push(left_row.clone());
        }

        // 探测右表
        let mut join_rows = Vec::new();

        for right_row in &right_data.rows {
            let join_key = self.extract_join_key(right_row, &right_data.columns)?;
            if let Some(left_rows) = hash_table.get(&join_key) {
                for left_row in left_rows {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    join_rows.push(joined_row);
                }
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(join_rows)
    }

    fn extract_join_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for join_key in &self.join_keys {
            if let Some(col_index) = columns.iter().position(|c| c == join_key) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }
}

#[async_trait]
impl Operator for HashJoinOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing hash join operation");

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
            columns: vec!["id".to_string(), "dept".to_string(), "salary".to_string()],
            rows: vec![
                vec!["1".to_string(), "IT".to_string(), "5000".to_string()],
                vec!["2".to_string(), "HR".to_string(), "4000".to_string()],
                vec!["4".to_string(), "Finance".to_string(), "6000".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let join_rows = self.perform_hash_join(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string(), 
                             "id".to_string(), "dept".to_string(), "salary".to_string()];
        result.rows = join_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Hash join completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 归并连接操作符
#[derive(Debug)]
pub struct MergeJoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub sort_keys: Vec<String>,
}

impl MergeJoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
            memory_manager,
            buffer_pool,
            sort_keys: vec!["id".to_string()],
        }
    }

    pub fn set_sort_keys(&mut self, keys: Vec<String>) {
        self.sort_keys = keys;
    }

    async fn perform_merge_join(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing merge join with sort keys: {:?}", self.sort_keys);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 对两个表进行排序
        let sorted_left = self.sort_data(left_data.rows, &left_data.columns)?;
        let sorted_right = self.sort_data(right_data.rows, &right_data.columns)?;

        // 执行归并连接
        let mut join_rows = Vec::new();
        let mut left_index = 0;
        let mut right_index = 0;

        while left_index < sorted_left.len() && right_index < sorted_right.len() {
            let left_key = self.extract_sort_key(&sorted_left[left_index], &left_data.columns)?;
            let right_key = self.extract_sort_key(&sorted_right[right_index], &right_data.columns)?;

            match left_key.cmp(&right_key) {
                std::cmp::Ordering::Less => left_index += 1,
                std::cmp::Ordering::Greater => right_index += 1,
                std::cmp::Ordering::Equal => {
                    // 找到匹配的行
                    let mut joined_row = sorted_left[left_index].clone();
                    joined_row.extend(sorted_right[right_index].clone());
                    join_rows.push(joined_row);
                    right_index += 1;
                }
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(join_rows)
    }

    fn sort_data(&self, mut rows: Vec<Vec<String>>, columns: &[String]) -> Result<Vec<Vec<String>>> {
        rows.sort_by(|a, b| {
            let a_key = self.extract_sort_key(a, columns).unwrap_or_default();
            let b_key = self.extract_sort_key(b, columns).unwrap_or_default();
            a_key.cmp(&b_key)
        });
        Ok(rows)
    }

    fn extract_sort_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for sort_key in &self.sort_keys {
            if let Some(col_index) = columns.iter().position(|c| c == sort_key) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }
}

#[async_trait]
impl Operator for MergeJoinOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing merge join operation");

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
            columns: vec!["id".to_string(), "dept".to_string(), "salary".to_string()],
            rows: vec![
                vec!["1".to_string(), "IT".to_string(), "5000".to_string()],
                vec!["2".to_string(), "HR".to_string(), "4000".to_string()],
                vec!["4".to_string(), "Finance".to_string(), "6000".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let join_rows = self.perform_merge_join(left_data, right_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string(), 
                             "id".to_string(), "dept".to_string(), "salary".to_string()];
        result.rows = join_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Merge join completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
} 
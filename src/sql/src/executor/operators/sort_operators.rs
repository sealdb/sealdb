use common::Result;
use std::sync::Arc;
use std::collections::HashMap;
use async_trait::async_trait;
use tracing::{debug, info, warn};
use std::time::Duration;
use tokio::time;
use std::collections::BinaryHeap;
use std::cmp::Ordering;

use crate::executor::execution_models::QueryResult;
use crate::storage::buffer_pool::{BufferPool, PageId};
use crate::storage::memory::MemoryManager;
use crate::storage::worker_pool::WorkerPool;
use super::operator_trait::Operator;

/// 排序操作符
#[derive(Debug)]
pub struct SortOperator {
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
}

impl SortOperator {
    pub fn new(input: crate::optimizer::PlanNode, order_by: Vec<String>, memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            input,
            order_by,
            memory_manager,
        }
    }

    async fn perform_sort(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing sort with order by: {:?}", self.order_by);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let mut sorted_rows = input_data.rows.clone();
        sorted_rows.sort_by(|a, b| {
            for order_col in &self.order_by {
                if let Some(col_index) = input_data.columns.iter().position(|c| c == order_col) {
                    if col_index < a.len() && col_index < b.len() {
                        let cmp = a[col_index].cmp(&b[col_index]);
                        if cmp != Ordering::Equal {
                            return cmp;
                        }
                    }
                }
            }
            Ordering::Equal
        });

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(sorted_rows)
    }
}

#[async_trait]
impl Operator for SortOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing sort operation");

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
            ],
            affected_rows: 5,
            last_insert_id: None,
        };

        let sorted_rows = self.perform_sort(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = sorted_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Sort completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 外部排序操作符
#[derive(Debug)]
pub struct ExternalSortOperator {
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub temp_dir: String,
    pub chunk_size: usize,
    pub max_memory: usize,
}

impl ExternalSortOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        order_by: Vec<String>,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            input,
            order_by,
            memory_manager,
            buffer_pool,
            temp_dir: "/tmp".to_string(),
            chunk_size: 1000,
            max_memory: 1024 * 1024, // 1MB
        }
    }

    pub fn set_temp_dir(&mut self, temp_dir: String) {
        self.temp_dir = temp_dir;
    }

    pub fn set_chunk_size(&mut self, chunk_size: usize) {
        self.chunk_size = chunk_size;
    }

    pub fn set_max_memory(&mut self, max_memory: usize) {
        self.max_memory = max_memory;
    }

    async fn perform_external_sort(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing external sort with chunk size: {}, max memory: {}",
              self.chunk_size, self.max_memory);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(self.max_memory)?;

        // 将数据分割成块
        let chunks = self.split_into_chunks(&input_data.rows)?;

        // 对每个块进行排序
        let mut sorted_chunks = Vec::new();
        for chunk in chunks {
            let sorted_chunk = self.sort_chunk(&chunk, &input_data.columns)?;
            sorted_chunks.push(sorted_chunk);
        }

        // 合并排序后的块
        let merged_rows = self.merge_sorted_chunks(sorted_chunks, &input_data.columns)?;

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(merged_rows)
    }

    fn split_into_chunks(&self, rows: &[Vec<String>]) -> Result<Vec<Vec<Vec<String>>>> {
        let mut chunks = Vec::new();
        let mut current_chunk = Vec::new();

        for row in rows {
            current_chunk.push(row.clone());
            if current_chunk.len() >= self.chunk_size {
                chunks.push(current_chunk.clone());
                current_chunk.clear();
            }
        }

        if !current_chunk.is_empty() {
            chunks.push(current_chunk);
        }

        Ok(chunks)
    }

    fn sort_chunk(&self, chunk: &[Vec<String>], columns: &[String]) -> Result<Vec<Vec<String>>> {
        let mut sorted_chunk = chunk.to_vec();
        sorted_chunk.sort_by(|a, b| {
            for order_col in &self.order_by {
                if let Some(col_index) = columns.iter().position(|c| c == order_col) {
                    if col_index < a.len() && col_index < b.len() {
                        let cmp = a[col_index].cmp(&b[col_index]);
                        if cmp != Ordering::Equal {
                            return cmp;
                        }
                    }
                }
            }
            Ordering::Equal
        });
        Ok(sorted_chunk)
    }

    fn merge_sorted_chunks(&self, chunks: Vec<Vec<Vec<String>>>, columns: &[String]) -> Result<Vec<Vec<String>>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        if chunks.len() == 1 {
            return Ok(chunks[0].clone());
        }

        // 简单的两两合并策略
        let mut merged = chunks[0].clone();
        for chunk in &chunks[1..] {
            merged = self.merge_two_sorted_chunks(&merged, chunk, columns)?;
        }

        Ok(merged)
    }

    fn merge_two_sorted_chunks(&self, chunk1: &[Vec<String>], chunk2: &[Vec<String>], columns: &[String]) -> Result<Vec<Vec<String>>> {
        let mut merged = Vec::new();
        let mut i = 0;
        let mut j = 0;

        while i < chunk1.len() && j < chunk2.len() {
            let cmp = self.compare_rows(&chunk1[i], &chunk2[j], columns);
            if cmp == Ordering::Less || cmp == Ordering::Equal {
                merged.push(chunk1[i].clone());
                i += 1;
            } else {
                merged.push(chunk2[j].clone());
                j += 1;
            }
        }

        // 添加剩余的行
        while i < chunk1.len() {
            merged.push(chunk1[i].clone());
            i += 1;
        }

        while j < chunk2.len() {
            merged.push(chunk2[j].clone());
            j += 1;
        }

        Ok(merged)
    }

    fn compare_rows(&self, a: &[String], b: &[String], columns: &[String]) -> Ordering {
        for order_col in &self.order_by {
            if let Some(col_index) = columns.iter().position(|c| c == order_col) {
                if col_index < a.len() && col_index < b.len() {
                    let cmp = a[col_index].cmp(&b[col_index]);
                    if cmp != Ordering::Equal {
                        return cmp;
                    }
                }
            }
        }
        Ordering::Equal
    }
}

#[async_trait]
impl Operator for ExternalSortOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing external sort operation");

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
            ],
            affected_rows: 5,
            last_insert_id: None,
        };

        let sorted_rows = self.perform_external_sort(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = sorted_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("External sort completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// TopN操作符
#[derive(Debug)]
pub struct TopNOperator {
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
    pub limit: usize,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
}

impl TopNOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        order_by: Vec<String>,
        limit: usize,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            input,
            order_by,
            limit,
            memory_manager,
            buffer_pool,
        }
    }

    async fn perform_top_n_sort(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing top {} sort with order by: {:?}", self.limit, self.order_by);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 使用最小堆来维护前N个元素
        let mut heap = BinaryHeap::new();

        for row in &input_data.rows {
            let sort_key = self.extract_sort_key(row, &input_data.columns)?;
            let heap_item = HeapItem {
                sort_key,
                row: row.clone(),
            };

            heap.push(heap_item);

            // 保持堆的大小为limit
            if heap.len() > self.limit {
                heap.pop();
            }
        }

        // 从堆中提取结果
        let mut result_rows = Vec::new();
        while let Some(item) = heap.pop() {
            result_rows.push(item.row);
        }

        // 反转结果以保持正确的顺序
        result_rows.reverse();

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(result_rows)
    }

    fn extract_sort_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for order_col in &self.order_by {
            if let Some(col_index) = columns.iter().position(|c| c == order_col) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }
}

#[derive(Debug)]
struct HeapItem {
    sort_key: String,
    row: Vec<String>,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key == other.sort_key
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // 使用逆序比较，因为BinaryHeap是最大堆
        other.sort_key.cmp(&self.sort_key)
    }
}

#[async_trait]
impl Operator for TopNOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing top {} operation", self.limit);

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["5".to_string(), "Eve".to_string(), "500".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["4".to_string(), "David".to_string(), "400".to_string()],
            ],
            affected_rows: 5,
            last_insert_id: None,
        };

        let top_rows = self.perform_top_n_sort(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = top_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Top {} operation completed, returned {} rows", self.limit, result.affected_rows);
        Ok(result)
    }
}
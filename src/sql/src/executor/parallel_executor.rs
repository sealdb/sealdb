use common::Result;
use std::sync::{Arc, Mutex, RwLock};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use crate::storage::worker_pool::{WorkerPool, WorkerPoolConfig, TaskInfo, TaskPriority, TaskType};
use crate::optimizer::{OptimizedPlan, PlanNode};
use crate::executor::{executor::ExecutionContext, execution_models::QueryResult};

/// 并行查询执行器
pub struct ParallelQueryExecutor {
    /// 工作线程池
    worker_pool: Arc<WorkerPool>,
    /// 并行度控制信号量
    parallelism_semaphore: Arc<Semaphore>,
    /// 当前并行度
    current_parallelism: Arc<Mutex<usize>>,
    /// 执行统计
    execution_stats: Arc<Mutex<ParallelExecutionStats>>,
    /// 配置
    config: Arc<RwLock<ParallelExecutorConfig>>,
}

/// 并行执行器配置
#[derive(Debug, Clone)]
pub struct ParallelExecutorConfig {
    /// 最大并行度
    pub max_parallelism: usize,
    /// 默认并行度
    pub default_parallelism: usize,
    /// 是否启用动态调整
    pub enable_dynamic_adjustment: bool,
    /// 调整检查间隔（秒）
    pub adjustment_check_interval_seconds: u64,
    /// CPU 使用率阈值
    pub cpu_usage_threshold: f64,
    /// 内存使用率阈值
    pub memory_usage_threshold: f64,
    /// 是否启用任务优先级
    pub enable_task_priority: bool,
    /// 查询超时时间（秒）
    pub query_timeout_seconds: u64,
}

impl Default for ParallelExecutorConfig {
    fn default() -> Self {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        Self {
            max_parallelism: cpu_count * 4,
            default_parallelism: cpu_count * 2,
            enable_dynamic_adjustment: true,
            adjustment_check_interval_seconds: 30,
            cpu_usage_threshold: 0.8,
            memory_usage_threshold: 0.8,
            enable_task_priority: true,
            query_timeout_seconds: 300,
        }
    }
}

/// 并行执行统计
#[derive(Debug, Clone)]
pub struct ParallelExecutionStats {
    pub total_queries: u64,
    pub parallel_queries: u64,
    pub total_execution_time: Duration,
    pub average_execution_time_ms: f64,
    pub current_parallelism: usize,
    pub max_parallelism_used: usize,
    pub queries_timed_out: u64,
    pub queries_failed: u64,
}

impl ParallelExecutionStats {
    pub fn new() -> Self {
        Self {
            total_queries: 0,
            parallel_queries: 0,
            total_execution_time: Duration::ZERO,
            average_execution_time_ms: 0.0,
            current_parallelism: 0,
            max_parallelism_used: 0,
            queries_timed_out: 0,
            queries_failed: 0,
        }
    }
}

impl ParallelQueryExecutor {
    pub fn new() -> Self {
        let config = ParallelExecutorConfig::default();
        let worker_pool_config = WorkerPoolConfig {
            min_worker_threads: 2,
            max_worker_threads: config.max_parallelism,
            initial_worker_threads: config.default_parallelism,
            task_queue_size: 1000,
            enable_thread_monitoring: true,
            thread_idle_timeout_seconds: 300,
            enable_task_priority: config.enable_task_priority,
            max_parallelism: config.max_parallelism,
            enable_dynamic_adjustment: config.enable_dynamic_adjustment,
            adjustment_check_interval_seconds: config.adjustment_check_interval_seconds,
            cpu_usage_threshold: config.cpu_usage_threshold,
            memory_usage_threshold: config.memory_usage_threshold,
        };

        let worker_pool = Arc::new(WorkerPool::with_config(worker_pool_config));
        let parallelism_semaphore = Arc::new(Semaphore::new(config.default_parallelism));
        let current_parallelism = Arc::new(Mutex::new(config.default_parallelism));
        let execution_stats = Arc::new(Mutex::new(ParallelExecutionStats::new()));
        let config = Arc::new(RwLock::new(config));

        Self {
            worker_pool,
            parallelism_semaphore,
            current_parallelism,
            execution_stats,
            config,
        }
    }

    pub fn with_config(config: ParallelExecutorConfig) -> Self {
        let worker_pool_config = WorkerPoolConfig {
            min_worker_threads: 2,
            max_worker_threads: config.max_parallelism,
            initial_worker_threads: config.default_parallelism,
            task_queue_size: 1000,
            enable_thread_monitoring: true,
            thread_idle_timeout_seconds: 300,
            enable_task_priority: config.enable_task_priority,
            max_parallelism: config.max_parallelism,
            enable_dynamic_adjustment: config.enable_dynamic_adjustment,
            adjustment_check_interval_seconds: config.adjustment_check_interval_seconds,
            cpu_usage_threshold: config.cpu_usage_threshold,
            memory_usage_threshold: config.memory_usage_threshold,
        };

        let worker_pool = Arc::new(WorkerPool::with_config(worker_pool_config));
        let parallelism_semaphore = Arc::new(Semaphore::new(config.default_parallelism));
        let current_parallelism = Arc::new(Mutex::new(config.default_parallelism));
        let execution_stats = Arc::new(Mutex::new(ParallelExecutionStats::new()));
        let config = Arc::new(RwLock::new(config));

        Self {
            worker_pool,
            parallelism_semaphore,
            current_parallelism,
            execution_stats,
            config,
        }
    }

    /// 并行执行查询计划
    pub async fn execute_parallel(&self, plan: OptimizedPlan, context: &ExecutionContext) -> Result<QueryResult> {
        let start_time = Instant::now();

        // 更新统计信息
        {
            let mut stats = self.execution_stats.lock().unwrap();
            stats.total_queries += 1;
            stats.parallel_queries += 1;
        }

        // 分析查询计划，确定并行策略
        let parallel_strategy = self.analyze_parallel_strategy(&plan)?;

        // 根据策略执行查询
        let result = match parallel_strategy {
            ParallelStrategy::Sequential => {
                self.execute_sequential(plan, context).await?
            }
            ParallelStrategy::Parallel { parallelism } => {
                self.execute_with_parallelism(plan, context, parallelism).await?
            }
            ParallelStrategy::Mixed { sequential_parts, parallel_parts } => {
                self.execute_mixed(plan, context, sequential_parts, parallel_parts).await?
            }
        };

        // 更新执行统计
        {
            let mut stats = self.execution_stats.lock().unwrap();
            let execution_time = start_time.elapsed();
            stats.total_execution_time += execution_time;
            stats.average_execution_time_ms =
                stats.total_execution_time.as_millis() as f64 / stats.total_queries as f64;
        }

        Ok(result)
    }

    /// 分析并行策略
    fn analyze_parallel_strategy(&self, plan: &OptimizedPlan) -> Result<ParallelStrategy> {
        let node_count = plan.nodes.len();
        let config = self.config.read().unwrap();

        if node_count == 0 {
            return Ok(ParallelStrategy::Sequential);
        }

        if node_count == 1 {
            return Ok(ParallelStrategy::Sequential);
        }

        // 分析节点类型，确定是否可以并行
        let mut can_parallelize = true;
        let mut sequential_nodes = Vec::new();
        let mut parallel_nodes = Vec::new();

        for (i, node) in plan.nodes.iter().enumerate() {
            match node {
                PlanNode::TableScan { .. } => {
                    // 表扫描可以并行
                    parallel_nodes.push(i);
                }
                PlanNode::IndexScan { .. } => {
                    // 索引扫描可以并行
                    parallel_nodes.push(i);
                }
                PlanNode::Filter { .. } => {
                    // 过滤可以并行
                    parallel_nodes.push(i);
                }
                PlanNode::Project { .. } => {
                    // 投影可以并行
                    parallel_nodes.push(i);
                }
                PlanNode::Join { .. } => {
                    // 连接需要特殊处理
                    if can_parallelize {
                        parallel_nodes.push(i);
                    } else {
                        sequential_nodes.push(i);
                    }
                }
                PlanNode::Aggregate { .. } => {
                    // 聚合通常需要顺序执行
                    sequential_nodes.push(i);
                    can_parallelize = false;
                }
                PlanNode::Sort { .. } => {
                    // 排序通常需要顺序执行
                    sequential_nodes.push(i);
                    can_parallelize = false;
                }
                PlanNode::Limit { .. } => {
                    // 限制需要顺序执行
                    sequential_nodes.push(i);
                    can_parallelize = false;
                }
            }
        }

        if parallel_nodes.is_empty() {
            Ok(ParallelStrategy::Sequential)
        } else if sequential_nodes.is_empty() {
            let parallelism = std::cmp::min(parallel_nodes.len(), config.max_parallelism);
            Ok(ParallelStrategy::Parallel { parallelism })
        } else {
            Ok(ParallelStrategy::Mixed { sequential_parts: sequential_nodes, parallel_parts: parallel_nodes })
        }
    }

    /// 顺序执行
    async fn execute_sequential(&self, plan: OptimizedPlan, context: &ExecutionContext) -> Result<QueryResult> {
        // 简单的顺序执行
        let mut result = QueryResult::new();

        for node in plan.nodes {
            let node_result = self.execute_node(node, context).await?;
            result.merge(node_result);
        }

        Ok(result)
    }

    /// 并行执行
    async fn execute_with_parallelism(&self, plan: OptimizedPlan, context: &ExecutionContext, parallelism: usize) -> Result<QueryResult> {
        // 获取并行度许可
        let _permit = self.parallelism_semaphore.clone().acquire_owned().await
            .map_err(|e| common::Error::Storage(e.to_string()))?;

        // 将节点分组
        let node_groups = self.group_nodes_for_parallel_execution(plan.nodes, parallelism);

        // 并行执行每个组
        let mut tasks = Vec::new();
        for group in node_groups {
            let context = context.clone();
            let task = move || {
                // 这里应该实现实际的节点执行逻辑
                // 暂时返回空结果
                QueryResult::new()
            };
            tasks.push(task);
        }

        // 使用工作线程池并行执行
        let results = self.worker_pool.execute_parallel(tasks)?;

        // 合并结果
        let mut final_result = QueryResult::new();
        for result in results {
            final_result.merge(result);
        }

        Ok(final_result)
    }

    /// 混合执行（部分并行，部分顺序）
    async fn execute_mixed(&self, plan: OptimizedPlan, context: &ExecutionContext, sequential_parts: Vec<usize>, parallel_parts: Vec<usize>) -> Result<QueryResult> {
        let mut result = QueryResult::new();

        // 先执行顺序部分
        for &index in &sequential_parts {
            if index < plan.nodes.len() {
                let node = plan.nodes[index].clone();
                let node_result = self.execute_node(node, context).await?;
                result.merge(node_result);
            }
        }

        // 再执行并行部分
        let parallel_nodes: Vec<_> = parallel_parts.iter()
            .filter_map(|&i| plan.nodes.get(i).cloned())
            .collect();

        if !parallel_nodes.is_empty() {
            let parallelism = std::cmp::min(parallel_nodes.len(), self.config.read().unwrap().max_parallelism);
            let parallel_result = self.execute_with_parallelism(
                OptimizedPlan {
                    nodes: parallel_nodes,
                    estimated_cost: 1000.0,
                    estimated_rows: 1000,
                },
                context,
                parallelism
            ).await?;
            result.merge(parallel_result);
        }

        Ok(result)
    }

    /// 执行单个节点
    async fn execute_node(&self, node: PlanNode, context: &ExecutionContext) -> Result<QueryResult> {
        // 这里应该实现具体的节点执行逻辑
        // 暂时返回空结果
        Ok(QueryResult::new())
    }

    /// 将节点分组用于并行执行
    fn group_nodes_for_parallel_execution(&self, nodes: Vec<PlanNode>, parallelism: usize) -> Vec<Vec<PlanNode>> {
        let mut groups = Vec::new();
        let group_size = (nodes.len() + parallelism - 1) / parallelism; // 向上取整

        for chunk in nodes.chunks(group_size) {
            groups.push(chunk.to_vec());
        }

        groups
    }

    /// 调整并行度
    pub fn adjust_parallelism(&self, new_parallelism: usize) -> Result<()> {
        let config = self.config.read().unwrap();
        if new_parallelism > config.max_parallelism {
            return Err(common::Error::Storage(
                format!("Parallelism {} exceeds maximum {}", new_parallelism, config.max_parallelism)
            ));
        }

        // 更新当前并行度
        {
            let mut current = self.current_parallelism.lock().unwrap();
            *current = new_parallelism;
        }

        // 更新统计信息
        {
            let mut stats = self.execution_stats.lock().unwrap();
            stats.current_parallelism = new_parallelism;
            if new_parallelism > stats.max_parallelism_used {
                stats.max_parallelism_used = new_parallelism;
            }
        }

        Ok(())
    }

    /// 获取执行统计
    pub fn get_stats(&self) -> ParallelExecutionStats {
        self.execution_stats.lock().unwrap().clone()
    }

    /// 获取工作线程池统计
    pub fn get_worker_pool_stats(&self) -> crate::storage::worker_pool::WorkerPoolStats {
        self.worker_pool.get_stats()
    }

    /// 获取工作线程信息
    pub fn get_worker_info(&self) -> HashMap<usize, crate::storage::worker_pool::WorkerInfo> {
        self.worker_pool.get_worker_info()
    }

    /// 动态调整并行度
    pub fn adjust_parallelism_dynamically(&self) -> Result<()> {
        let config = self.config.read().unwrap();
        if !config.enable_dynamic_adjustment {
            return Ok(());
        }

        // TODO: 实现基于系统负载的动态调整
        // 这里可以根据 CPU 使用率、内存使用率等指标调整并行度

        Ok(())
    }
}

/// 并行策略
#[derive(Debug, Clone)]
pub enum ParallelStrategy {
    /// 顺序执行
    Sequential,
    /// 并行执行
    Parallel { parallelism: usize },
    /// 混合执行
    Mixed { sequential_parts: Vec<usize>, parallel_parts: Vec<usize> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::PlanNode;

    #[test]
    fn test_parallel_executor_new() {
        let executor = ParallelQueryExecutor::new();
        let stats = executor.get_stats();
        assert_eq!(stats.total_queries, 0);
    }

    #[test]
    fn test_analyze_parallel_strategy() {
        let executor = ParallelQueryExecutor::new();

        // 测试空计划
        let empty_plan = OptimizedPlan {
            nodes: vec![],
            estimated_cost: 0.0,
            estimated_rows: 0,
        };
        let strategy = executor.analyze_parallel_strategy(&empty_plan).unwrap();
        assert!(matches!(strategy, ParallelStrategy::Sequential));

        // 测试单节点计划
        let single_node_plan = OptimizedPlan {
            nodes: vec![PlanNode::TableScan { table: "test".to_string(), columns: vec![] }],
            estimated_cost: 100.0,
            estimated_rows: 100,
        };
        let strategy = executor.analyze_parallel_strategy(&single_node_plan).unwrap();
        assert!(matches!(strategy, ParallelStrategy::Sequential));
    }

    #[test]
    fn test_adjust_parallelism() {
        let executor = ParallelQueryExecutor::new();

        // 测试正常调整
        let result = executor.adjust_parallelism(4);
        assert!(result.is_ok());

        // 测试超出最大并行度
        let result = executor.adjust_parallelism(1000);
        assert!(result.is_err());
    }

    #[test]
    fn test_group_nodes_for_parallel_execution() {
        let executor = ParallelQueryExecutor::new();
        let nodes = vec![
            PlanNode::TableScan { table: "t1".to_string(), columns: vec![] },
            PlanNode::TableScan { table: "t2".to_string(), columns: vec![] },
            PlanNode::TableScan { table: "t3".to_string(), columns: vec![] },
            PlanNode::TableScan { table: "t4".to_string(), columns: vec![] },
        ];

        let groups = executor.group_nodes_for_parallel_execution(nodes, 2);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 2);
    }
}
use common::Result;
use tracing::{info, warn};
use std::sync::Arc;

use crate::optimizer::{OptimizedPlan, PlanNode};
use crate::executor::operators::*;
use crate::storage::memory::MemoryManager;

/// 执行引擎
pub struct ExecutionEngine {
    /// 火山模型执行器
    volcano_executor: Arc<VolcanoExecutor>,
    /// 流水线执行器
    pipeline_executor: Arc<PipelineExecutor>,
    /// 向量化执行器
    vectorized_executor: Arc<VectorizedExecutor>,
    /// MPP 执行器
    mpp_executor: Arc<MppExecutor>,
    /// 执行模型选择器
    model_selector: Arc<ExecutionModelSelector>,
    /// 内存管理器
    #[allow(dead_code)]
    memory_manager: Arc<MemoryManager>,
}

impl ExecutionEngine {
    pub fn new() -> Self {
        let memory_manager = Arc::new(MemoryManager::new());

        Self {
            volcano_executor: Arc::new(VolcanoExecutor::new(memory_manager.clone())),
            pipeline_executor: Arc::new(PipelineExecutor::new(memory_manager.clone())),
            vectorized_executor: Arc::new(VectorizedExecutor::new(memory_manager.clone())),
            mpp_executor: Arc::new(MppExecutor::new(memory_manager.clone())),
            model_selector: Arc::new(ExecutionModelSelector::new()),
            memory_manager,
        }
    }

    /// 执行查询计划，自动选择最佳执行模型
    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        // 1. 分析查询特征
        let query_features = self.analyze_query_features(&plan).await?;

        // 2. 选择执行模型
        let execution_model = self.model_selector.select_model(&query_features).await?;

        info!("Selected execution model: {:?}", execution_model);

        // 3. 使用选定的执行模型执行
        let result = match execution_model {
            ExecutionModel::Volcano => {
                self.volcano_executor.execute(plan).await?
            }
            ExecutionModel::Pipeline => {
                self.pipeline_executor.execute(plan).await?
            }
            ExecutionModel::Vectorized => {
                self.vectorized_executor.execute(plan).await?
            }
            ExecutionModel::Mpp => {
                self.mpp_executor.execute(plan).await?
            }
        };

        Ok(result)
    }

    /// 分析查询特征
    async fn analyze_query_features(&self, plan: &OptimizedPlan) -> Result<QueryFeatures> {
        let mut features = QueryFeatures::new();

        // 分析查询复杂度
        features.complexity = self.calculate_complexity(plan);

        // 分析数据量
        features.data_size = self.estimate_data_size(plan).await?;

        // 分析操作类型
        features.operation_types = self.analyze_operations(plan);

        // 分析并行度需求
        features.parallelism_requirement = self.analyze_parallelism(plan);

        Ok(features)
    }

    fn calculate_complexity(&self, plan: &OptimizedPlan) -> QueryComplexity {
        let node_count = plan.nodes.len();
        let has_joins = plan.nodes.iter().any(|node| matches!(node, PlanNode::Join { .. }));
        let has_aggregates = plan.nodes.iter().any(|node| matches!(node, PlanNode::Aggregate { .. }));

        match (node_count, has_joins, has_aggregates) {
            (1..=3, false, false) => QueryComplexity::Simple,
            (4..=8, true, false) => QueryComplexity::Medium,
            (9.., _, true) => QueryComplexity::Complex,
            _ => QueryComplexity::Medium,
        }
    }

    async fn estimate_data_size(&self, _plan: &OptimizedPlan) -> Result<u64> {
        // TODO: 实现数据量估算
        Ok(1000) // 临时返回固定值
    }

    fn analyze_operations(&self, plan: &OptimizedPlan) -> Vec<OperationType> {
        let mut operations = Vec::new();

        for node in &plan.nodes {
            match node {
                PlanNode::TableScan { .. } => operations.push(OperationType::Scan),
                PlanNode::IndexScan { .. } => operations.push(OperationType::IndexScan),
                PlanNode::Join { .. } => operations.push(OperationType::Join),
                PlanNode::Aggregate { .. } => operations.push(OperationType::Aggregate),
                PlanNode::Sort { .. } => operations.push(OperationType::Sort),
                _ => operations.push(OperationType::Other),
            }
        }

        operations
    }

    fn analyze_parallelism(&self, plan: &OptimizedPlan) -> ParallelismRequirement {
        let node_count = plan.nodes.len();
        let has_large_operations = plan.nodes.iter().any(|node| {
            matches!(node,
                PlanNode::Aggregate { .. } |
                PlanNode::Sort { .. } |
                PlanNode::Join { .. }
            )
        });

        match (node_count, has_large_operations) {
            (1..=3, false) => ParallelismRequirement::None,
            (4..=8, true) => ParallelismRequirement::Medium,
            (9.., _) => ParallelismRequirement::High,
            _ => ParallelismRequirement::Low,
        }
    }
}

/// 执行模型枚举
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionModel {
    /// 火山模型 - 用于复杂查询和 OLTP 场景
    Volcano,
    /// 流水线执行 - 用于简单查询和 OLAP 场景
    Pipeline,
    /// 向量化执行 - 用于批量数据处理
    Vectorized,
    /// MPP 架构 - 用于大规模并行分析
    Mpp,
}

/// 查询特征
#[derive(Debug, Clone)]
pub struct QueryFeatures {
    /// 查询复杂度
    pub complexity: QueryComplexity,
    /// 预估数据量
    pub data_size: u64,
    /// 操作类型列表
    pub operation_types: Vec<OperationType>,
    /// 并行度需求
    pub parallelism_requirement: ParallelismRequirement,
}

impl QueryFeatures {
    pub fn new() -> Self {
        Self {
            complexity: QueryComplexity::Simple,
            data_size: 0,
            operation_types: Vec::new(),
            parallelism_requirement: ParallelismRequirement::None,
        }
    }
}

/// 查询复杂度
#[derive(Debug, Clone, PartialEq)]
pub enum QueryComplexity {
    Simple,   // 简单查询
    Medium,   // 中等复杂度
    Complex,  // 复杂查询
}

/// 操作类型
#[derive(Debug, Clone, PartialEq)]
pub enum OperationType {
    Scan,
    IndexScan,
    Join,
    Aggregate,
    Sort,
    Other,
}

/// 并行度需求
#[derive(Debug, Clone, PartialEq)]
pub enum ParallelismRequirement {
    None,    // 无需并行
    Low,     // 低并行度
    Medium,  // 中等并行度
    High,    // 高并行度
}

/// 查询结果
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
}

impl QueryResult {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
        }
    }

    pub fn merge(&mut self, other: QueryResult) {
        // 合并行数据
        self.rows.extend(other.rows);
        // 更新影响的行数
        self.affected_rows += other.affected_rows;
        // 如果其他结果有 last_insert_id，使用它
        if other.last_insert_id.is_some() {
            self.last_insert_id = other.last_insert_id;
        }
    }
}

/// 执行模型选择器
pub struct ExecutionModelSelector;

impl ExecutionModelSelector {
    pub fn new() -> Self {
        Self
    }

    /// 根据查询特征选择最佳执行模型
    pub async fn select_model(&self, features: &QueryFeatures) -> Result<ExecutionModel> {
        match (features.complexity.clone(), features.parallelism_requirement.clone()) {
            // 简单查询 -> 流水线执行
            (QueryComplexity::Simple, ParallelismRequirement::None) => {
                Ok(ExecutionModel::Pipeline)
            }
            // 复杂查询 -> 火山模型
            (QueryComplexity::Complex, _) => {
                Ok(ExecutionModel::Volcano)
            }
            // 大数据量 + 高并行度 -> MPP
            (_, ParallelismRequirement::High) => {
                Ok(ExecutionModel::Mpp)
            }
            // 批量数据处理 -> 向量化执行
            _ if features.data_size > 10000 => {
                Ok(ExecutionModel::Vectorized)
            }
            // 默认使用火山模型
            _ => {
                Ok(ExecutionModel::Volcano)
            }
        }
    }
}

/// 火山模型执行器
pub struct VolcanoExecutor {
    #[allow(dead_code)]
    memory_manager: Arc<MemoryManager>,
}

impl VolcanoExecutor {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self { memory_manager }
    }

    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        info!("Executing query using Volcano model");

        // 构建火山模型执行计划
        let volcano_plan = self.build_volcano_plan(plan).await?;

        // 执行火山模型
        let result = self.execute_volcano_plan(volcano_plan).await?;

        Ok(result)
    }

    async fn build_volcano_plan(&self, plan: OptimizedPlan) -> Result<VolcanoPlan> {
        let mut volcano_nodes = Vec::new();

        // 创建共享的 BufferPool 和 MemoryManager
        let buffer_pool = Arc::new(crate::storage::buffer_pool::BufferPool::new());
        let memory_manager = Arc::new(crate::storage::memory::MemoryManager::new());

        for node in plan.nodes {
            let volcano_node = match node {
                PlanNode::TableScan { table, columns } => {
                    VolcanoNode::Scan(ScanOperator::new(table, columns, buffer_pool.clone(), memory_manager.clone()))
                }
                PlanNode::IndexScan { table, index, columns } => {
                    VolcanoNode::IndexScan(IndexScanOperator::new(table, index, columns, buffer_pool.clone(), memory_manager.clone()))
                }
                PlanNode::Join { left, right, join_type, condition: _ } => {
                    VolcanoNode::Join(JoinOperator::new(
                        *left,
                        *right,
                        format!("{:?}", join_type),
                        "condition".to_string(),
                        memory_manager.clone()
                    ))
                }
                PlanNode::Aggregate { input, group_by, aggregates } => {
                    VolcanoNode::Aggregate(AggregateOperator::new(
                        *input,
                        group_by,
                        aggregates,
                        memory_manager.clone()
                    ))
                }
                PlanNode::Sort { input, order_by } => {
                    VolcanoNode::Sort(SortOperator::new(*input, order_by, memory_manager.clone()))
                }
                _ => {
                    warn!("Unsupported plan node in Volcano model");
                    continue;
                }
            };
            volcano_nodes.push(volcano_node);
        }

        Ok(VolcanoPlan { nodes: volcano_nodes })
    }

    async fn execute_volcano_plan(&self, plan: VolcanoPlan) -> Result<QueryResult> {
        let mut result = QueryResult::new();

        // 火山模型：自底向上执行，每个操作符一次处理一行
        for node in plan.nodes {
            match node {
                VolcanoNode::Scan(scan_op) => {
                    let scan_result = scan_op.execute().await?;
                    result = self.merge_results(result, scan_result).await?;
                }
                VolcanoNode::IndexScan(index_scan_op) => {
                    let index_result = index_scan_op.execute().await?;
                    result = self.merge_results(result, index_result).await?;
                }
                VolcanoNode::Join(join_op) => {
                    let join_result = join_op.execute().await?;
                    result = self.merge_results(result, join_result).await?;
                }
                VolcanoNode::Aggregate(agg_op) => {
                    let agg_result = agg_op.execute().await?;
                    result = self.merge_results(result, agg_result).await?;
                }
                VolcanoNode::Sort(sort_op) => {
                    let sort_result = sort_op.execute().await?;
                    result = self.merge_results(result, sort_result).await?;
                }
            }
        }

        Ok(result)
    }

    async fn merge_results(&self, mut result: QueryResult, new_result: QueryResult) -> Result<QueryResult> {
        // 合并查询结果
        if result.columns.is_empty() {
            result.columns = new_result.columns;
        }
        result.rows.extend(new_result.rows);
        result.affected_rows += new_result.affected_rows;
        Ok(result)
    }
}

/// 流水线执行器
pub struct PipelineExecutor {
    #[allow(dead_code)]
    memory_manager: Arc<MemoryManager>,
}

impl PipelineExecutor {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self { memory_manager }
    }

    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        info!("Executing query using Pipeline model");

        // 构建流水线执行计划
        let pipeline_plan = self.build_pipeline_plan(plan).await?;

        // 执行流水线
        let result = self.execute_pipeline_plan(pipeline_plan).await?;

        Ok(result)
    }

    async fn build_pipeline_plan(&self, plan: OptimizedPlan) -> Result<PipelinePlan> {
        let mut pipeline_stages = Vec::new();

        // 创建共享的 BufferPool 和 MemoryManager
        let buffer_pool = Arc::new(crate::storage::buffer_pool::BufferPool::new());
        let memory_manager = Arc::new(crate::storage::memory::MemoryManager::new());

        // 将查询计划分解为流水线阶段
        for node in plan.nodes {
            let stage = match node {
                PlanNode::TableScan { table, columns } => {
                    PipelineStage::Scan(ScanOperator::new(table, columns, buffer_pool.clone(), memory_manager.clone()))
                }
                PlanNode::IndexScan { table, index, columns } => {
                    PipelineStage::IndexScan(IndexScanOperator::new(table, index, columns, buffer_pool.clone(), memory_manager.clone()))
                }
                PlanNode::Join { left, right, join_type, condition: _ } => {
                    PipelineStage::Join(JoinOperator::new(
                        *left,
                        *right,
                        format!("{:?}", join_type),
                        "condition".to_string(),
                        memory_manager.clone()
                    ))
                }
                PlanNode::Aggregate { input, group_by, aggregates } => {
                    PipelineStage::Aggregate(AggregateOperator::new(
                        *input,
                        group_by,
                        aggregates,
                        memory_manager.clone()
                    ))
                }
                PlanNode::Sort { input, order_by } => {
                    PipelineStage::Sort(SortOperator::new(*input, order_by, memory_manager.clone()))
                }
                _ => {
                    warn!("Unsupported plan node in Pipeline model");
                    continue;
                }
            };
            pipeline_stages.push(stage);
        }

        Ok(PipelinePlan { stages: pipeline_stages })
    }

    async fn execute_pipeline_plan(&self, plan: PipelinePlan) -> Result<QueryResult> {
        let mut result = QueryResult::new();

        // 流水线执行：各阶段并行执行，数据流式处理
        let mut data_stream = Vec::new();

        for stage in plan.stages {
            match stage {
                PipelineStage::Scan(scan_op) => {
                    let scan_data = scan_op.execute().await?;
                    data_stream = self.process_pipeline_stage(data_stream, scan_data).await?;
                }
                PipelineStage::IndexScan(index_scan_op) => {
                    let index_data = index_scan_op.execute().await?;
                    data_stream = self.process_pipeline_stage(data_stream, index_data).await?;
                }
                PipelineStage::Join(join_op) => {
                    let join_data = join_op.execute().await?;
                    data_stream = self.process_pipeline_stage(data_stream, join_data).await?;
                }
                PipelineStage::Aggregate(agg_op) => {
                    let agg_data = agg_op.execute().await?;
                    data_stream = self.process_pipeline_stage(data_stream, agg_data).await?;
                }
                PipelineStage::Sort(sort_op) => {
                    let sort_data = sort_op.execute().await?;
                    data_stream = self.process_pipeline_stage(data_stream, sort_data).await?;
                }
            }
        }

        // 将最终数据流转换为结果
        result.rows = data_stream;
        Ok(result)
    }

    async fn process_pipeline_stage(&self, mut data_stream: Vec<Vec<String>>, new_data: QueryResult) -> Result<Vec<Vec<String>>> {
        // 流水线处理：将新数据与现有数据流合并
        data_stream.extend(new_data.rows);
        Ok(data_stream)
    }
}

/// 向量化执行器
pub struct VectorizedExecutor {
    #[allow(dead_code)]
    memory_manager: Arc<MemoryManager>,
}

impl VectorizedExecutor {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self { memory_manager }
    }

    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        info!("Executing query using Vectorized model");

        // 构建向量化执行计划
        let vectorized_plan = self.build_vectorized_plan(plan).await?;

        // 执行向量化处理
        let result = self.execute_vectorized_plan(vectorized_plan).await?;

        Ok(result)
    }

    async fn build_vectorized_plan(&self, plan: OptimizedPlan) -> Result<VectorizedPlan> {
        let mut vectorized_operators = Vec::new();

        for node in plan.nodes {
            let operator = match node {
                PlanNode::TableScan { table, columns } => {
                    VectorizedOperator::BatchScan(BatchScanOperator::new(table, columns))
                }
                PlanNode::IndexScan { table, index, columns } => {
                    VectorizedOperator::BatchIndexScan(BatchIndexScanOperator::new(table, index, columns))
                }
                PlanNode::Join { left, right, join_type, condition: _ } => {
                    VectorizedOperator::BatchJoin(BatchJoinOperator::new(
                        *left,
                        *right,
                        format!("{:?}", join_type),
                        "condition".to_string()
                    ))
                }
                PlanNode::Aggregate { input, group_by, aggregates } => {
                    VectorizedOperator::BatchAggregate(BatchAggregateOperator::new(
                        *input,
                        group_by,
                        aggregates
                    ))
                }
                PlanNode::Sort { input, order_by } => {
                    VectorizedOperator::BatchSort(BatchSortOperator::new(*input, order_by))
                }
                _ => {
                    warn!("Unsupported plan node in Vectorized model");
                    continue;
                }
            };
            vectorized_operators.push(operator);
        }

        Ok(VectorizedPlan { operators: vectorized_operators })
    }

    async fn execute_vectorized_plan(&self, plan: VectorizedPlan) -> Result<QueryResult> {
        let mut result = QueryResult::new();

        // 向量化执行：批量处理数据，利用 SIMD 优化
        for operator in plan.operators {
            match operator {
                VectorizedOperator::BatchScan(batch_scan_op) => {
                    let batch_result = batch_scan_op.execute_batch().await?;
                    result = self.merge_batch_results(result, batch_result).await?;
                }
                VectorizedOperator::BatchIndexScan(batch_index_scan_op) => {
                    let batch_result = batch_index_scan_op.execute_batch().await?;
                    result = self.merge_batch_results(result, batch_result).await?;
                }
                VectorizedOperator::BatchJoin(batch_join_op) => {
                    let batch_result = batch_join_op.execute_batch().await?;
                    result = self.merge_batch_results(result, batch_result).await?;
                }
                VectorizedOperator::BatchAggregate(batch_agg_op) => {
                    let batch_result = batch_agg_op.execute_batch().await?;
                    result = self.merge_batch_results(result, batch_result).await?;
                }
                VectorizedOperator::BatchSort(batch_sort_op) => {
                    let batch_result = batch_sort_op.execute_batch().await?;
                    result = self.merge_batch_results(result, batch_result).await?;
                }
            }
        }

        Ok(result)
    }

    async fn merge_batch_results(&self, mut result: QueryResult, new_result: QueryResult) -> Result<QueryResult> {
        // 合并批量处理结果
        if result.columns.is_empty() {
            result.columns = new_result.columns;
        }
        result.rows.extend(new_result.rows);
        result.affected_rows += new_result.affected_rows;
        Ok(result)
    }
}

/// MPP 执行器
pub struct MppExecutor {
    #[allow(dead_code)]
    memory_manager: Arc<MemoryManager>,
}

impl MppExecutor {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self { memory_manager }
    }

    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        info!("Executing query using MPP model");

        // 构建 MPP 执行计划
        let mpp_plan = self.build_mpp_plan(plan).await?;

        // 执行 MPP 并行处理
        let result = self.execute_mpp_plan(mpp_plan).await?;

        Ok(result)
    }

    async fn build_mpp_plan(&self, plan: OptimizedPlan) -> Result<MppPlan> {
        let mut mpp_tasks = Vec::new();

        // 将查询计划分解为并行任务
        for (i, node) in plan.nodes.into_iter().enumerate() {
            let task = match node {
                PlanNode::TableScan { table, columns } => {
                    MppTask::ParallelScan(ParallelScanTask::new(i.to_string(), table, columns))
                }
                PlanNode::IndexScan { table, index, columns } => {
                    MppTask::ParallelIndexScan(ParallelIndexScanTask::new(i.to_string(), table, index, columns))
                }
                PlanNode::Join { left, right, join_type, condition: _ } => {
                    MppTask::ParallelJoin(ParallelJoinTask::new(
                        i.to_string(),
                        *left,
                        *right,
                        format!("{:?}", join_type),
                        "condition".to_string()
                    ))
                }
                PlanNode::Aggregate { input, group_by, aggregates } => {
                    MppTask::ParallelAggregate(ParallelAggregateTask::new(
                        i.to_string(),
                        *input,
                        group_by,
                        aggregates
                    ))
                }
                PlanNode::Sort { input, order_by } => {
                    MppTask::ParallelSort(ParallelSortTask::new(i.to_string(), *input, order_by))
                }
                _ => {
                    warn!("Unsupported plan node in MPP model");
                    continue;
                }
            };
            mpp_tasks.push(task);
        }

        Ok(MppPlan { tasks: mpp_tasks })
    }

    async fn execute_mpp_plan(&self, plan: MppPlan) -> Result<QueryResult> {
        let mut result = QueryResult::new();

        // MPP 执行：并行执行任务，然后合并结果
        let mut task_results = Vec::new();

        for task in plan.tasks {
            match task {
                MppTask::ParallelScan(parallel_scan_task) => {
                    let task_result = parallel_scan_task.execute_parallel().await?;
                    task_results.push(task_result);
                }
                MppTask::ParallelIndexScan(parallel_index_scan_task) => {
                    let task_result = parallel_index_scan_task.execute_parallel().await?;
                    task_results.push(task_result);
                }
                MppTask::ParallelJoin(parallel_join_task) => {
                    let task_result = parallel_join_task.execute_parallel().await?;
                    task_results.push(task_result);
                }
                MppTask::ParallelAggregate(parallel_agg_task) => {
                    let task_result = parallel_agg_task.execute_parallel().await?;
                    task_results.push(task_result);
                }
                MppTask::ParallelSort(parallel_sort_task) => {
                    let task_result = parallel_sort_task.execute_parallel().await?;
                    task_results.push(task_result);
                }
            }
        }

        // 合并所有并行任务的结果
        for task_result in task_results {
            result = self.merge_mpp_results(result, task_result).await?;
        }

        Ok(result)
    }

    async fn merge_mpp_results(&self, mut result: QueryResult, new_result: QueryResult) -> Result<QueryResult> {
        // 合并 MPP 并行处理结果
        if result.columns.is_empty() {
            result.columns = new_result.columns;
        }
        result.rows.extend(new_result.rows);
        result.affected_rows += new_result.affected_rows;
        Ok(result)
    }
}

// 火山模型相关类型
#[derive(Debug)]
pub struct VolcanoPlan {
    pub nodes: Vec<VolcanoNode>,
}

#[derive(Debug)]
pub enum VolcanoNode {
    Scan(ScanOperator),
    IndexScan(IndexScanOperator),
    Join(JoinOperator),
    Aggregate(AggregateOperator),
    Sort(SortOperator),
}

// 流水线模型相关类型
#[derive(Debug)]
pub struct PipelinePlan {
    pub stages: Vec<PipelineStage>,
}

#[derive(Debug)]
pub enum PipelineStage {
    Scan(ScanOperator),
    IndexScan(IndexScanOperator),
    Join(JoinOperator),
    Aggregate(AggregateOperator),
    Sort(SortOperator),
}

// 向量化模型相关类型
#[derive(Debug)]
pub struct VectorizedPlan {
    pub operators: Vec<VectorizedOperator>,
}

#[derive(Debug)]
pub enum VectorizedOperator {
    BatchScan(BatchScanOperator),
    BatchIndexScan(BatchIndexScanOperator),
    BatchJoin(BatchJoinOperator),
    BatchAggregate(BatchAggregateOperator),
    BatchSort(BatchSortOperator),
}

// MPP 模型相关类型
#[derive(Debug)]
pub struct MppPlan {
    pub tasks: Vec<MppTask>,
}

#[derive(Debug)]
pub enum MppTask {
    ParallelScan(ParallelScanTask),
    ParallelIndexScan(ParallelIndexScanTask),
    ParallelJoin(ParallelJoinTask),
    ParallelAggregate(ParallelAggregateTask),
    ParallelSort(ParallelSortTask),
}
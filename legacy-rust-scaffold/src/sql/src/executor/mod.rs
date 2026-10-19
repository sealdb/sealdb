//! SealDB 查询执行器模块
//!
//! 负责查询执行，包括各种执行模型和操作符

pub mod execution_models;
pub mod operators;
pub mod executor;
pub mod parallel_executor;
pub mod storage_executor;

// 重新导出执行器相关类型
pub use executor::Executor;
pub use execution_models::ExecutionEngine;

// 重新导出基础操作符 trait
pub use operators::operator_trait::Operator;

// 重新导出存储感知执行器
pub use storage_executor::{StorageExecutor, StorageOperationType};
pub use storage::{StorageOperation, StorageOperationResult, EngineType};

// 重新导出扫描操作符
pub use operators::scan_operators::{
    ScanOperator,
    IndexScanOperator,
    SeqScanOperator,
    EnhancedIndexScanOperator,
    BitmapScanOperator,
    BitmapCondition,
};

// 重新导出连接操作符
pub use operators::join_operators::{
    JoinOperator,
    NestedLoopJoinOperator,
    HashJoinOperator,
    MergeJoinOperator,
};

// 重新导出聚合操作符
pub use operators::aggregate_operators::{
    AggregateOperator,
    HashAggOperator,
    GroupAggOperator,
};

// 重新导出排序操作符
pub use operators::sort_operators::{
    SortOperator,
    ExternalSortOperator,
    TopNOperator,
};

// 重新导出集合操作符
pub use operators::set_operators::{
    UnionOperator,
    IntersectOperator,
    ExceptOperator,
};

// 重新导出批处理操作符
pub use operators::batch_operators::{
    BatchScanOperator,
    BatchIndexScanOperator,
    BatchJoinOperator,
    BatchAggregateOperator,
    BatchSortOperator,
};

// 重新导出并行操作符
pub use operators::parallel_operators::{
    ParallelScanTask,
    ParallelIndexScanTask,
    ParallelJoinTask,
    ParallelAggregateTask,
    ParallelSortTask,
    ParallelScanOperator,
    ParallelSortOperator,
};

// 重新导出分布式操作符
pub use operators::distributed_operators::{
    ShardScanOperator,
    ShardInfo,
    ShardNode,
    DistributedAggOperator,
};
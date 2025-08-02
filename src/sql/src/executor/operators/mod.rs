pub mod operator_trait;
pub mod scan_operators;
pub mod join_operators;
pub mod aggregate_operators;
pub mod sort_operators;
pub mod set_operators;
pub mod batch_operators;
pub mod parallel_operators;
pub mod distributed_operators;

// 重新导出基础操作符 trait
pub use operator_trait::Operator;

// 重新导出扫描操作符
pub use scan_operators::{
    ScanOperator,
    IndexScanOperator,
    SeqScanOperator,
    EnhancedIndexScanOperator,
    BitmapScanOperator,
    BitmapCondition,
};

// 重新导出连接操作符
pub use join_operators::{
    JoinOperator,
    NestedLoopJoinOperator,
    HashJoinOperator,
    MergeJoinOperator,
};

// 重新导出聚合操作符
pub use aggregate_operators::{
    AggregateOperator,
    HashAggOperator,
    GroupAggOperator,
};

// 重新导出排序操作符
pub use sort_operators::{
    SortOperator,
    ExternalSortOperator,
    TopNOperator,
};

// 重新导出集合操作符
pub use set_operators::{
    UnionOperator,
    IntersectOperator,
    ExceptOperator,
};

// 重新导出批处理操作符
pub use batch_operators::{
    BatchScanOperator,
    BatchIndexScanOperator,
    BatchJoinOperator,
    BatchAggregateOperator,
    BatchSortOperator,
};

// 重新导出并行操作符
pub use parallel_operators::{
    ParallelScanTask,
    ParallelIndexScanTask,
    ParallelJoinTask,
    ParallelAggregateTask,
    ParallelSortTask,
    ParallelScanOperator,
    ParallelSortOperator,
};

// 重新导出分布式操作符
pub use distributed_operators::{
    ShardScanOperator,
    ShardInfo,
    ShardNode,
    DistributedAggOperator,
}; 
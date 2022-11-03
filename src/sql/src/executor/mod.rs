//! SealDB 查询执行器模块
//!
//! 负责查询执行，包括各种执行模型和操作符

pub mod executor;
pub mod execution_models;
pub mod operators;
pub mod parallel_executor;

pub use executor::Executor;
pub use execution_models::{QueryResult, ExecutionEngine, ExecutionModel};
pub use operators::{Operator, ScanOperator, JoinOperator, AggregateOperator, SortOperator};
pub use parallel_executor::{ParallelQueryExecutor, ParallelExecutorConfig, ParallelExecutionStats, ParallelStrategy};
//! SealDB 查询优化器模块
//!
//! 负责查询优化，包括基于成本的优化 (CBO)

pub mod optimizer;
pub mod cbo;
pub mod cost_model;
pub mod statistics;

pub use optimizer::*;
pub use cbo::*;
pub use cost_model::{CostModel, CostEstimate};
pub use statistics::{StatisticsManager, StatisticsCollector, TableStatistics, ColumnStatistics, IndexStatistics};
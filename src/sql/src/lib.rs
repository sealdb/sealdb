//! SealDB SQL 模块
//!
//! 提供完整的 SQL 处理功能，包括解析、优化、执行等

pub mod parser;
pub mod optimizer;
pub mod executor;
pub mod storage;
pub mod distributed;
pub mod config;

// 重新导出主要类型
pub use parser::{SqlParser, ParsedStatement, ParsedExpression};
pub use optimizer::{Optimizer, RuleBasedOptimizer, CostBasedOptimizer};
pub use executor::{Executor, ExecutionEngine};
// 注意：这些类型在 storage crate 中不存在，需要从 sql crate 内部导入
// pub use storage::{MemoryManager, BufferPool, CacheManager, WorkerPool};
pub use distributed::{DistributedExecutor, NodeManager};
pub use config::*;

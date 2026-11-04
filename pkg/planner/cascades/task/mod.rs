//! TiDB `pkg/planner/cascades/task`.

pub mod task_scheduler;

pub use task_scheduler::TaskScheduler;

/// Cascades optimizer task (stub).
#[derive(Clone, Debug, Default)]
pub struct Task;

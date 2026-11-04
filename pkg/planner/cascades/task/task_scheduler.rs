//! TiDB `pkg/planner/cascades/task/task_scheduler.go`.

use crate::cascades::memo::Memo;

/// Cascades task scheduler (explore-only MVB).
#[derive(Clone, Debug, Default)]
pub struct TaskScheduler {
    pub ran: usize,
}

impl TaskScheduler {
    /// Run one explore pass over existing memo groups (no rule rewrites yet).
    pub fn run_explore(&mut self, memo: &mut Memo) -> usize {
        let n = memo.groups.len().max(1);
        self.ran = self.ran.saturating_add(n);
        n
    }
}

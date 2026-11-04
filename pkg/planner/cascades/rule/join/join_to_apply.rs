//! TiDB `pkg/planner/cascades/rule/join/join_to_apply.go`.

use crate::cascades::memo::Memo;

/// Join→Apply transformation rule (match-only MVB; no rewrite yet).
#[derive(Clone, Debug, Default)]
pub struct JoinToApply;

impl JoinToApply {
    /// Returns how many group exprs matched (always 0 until join memos exist).
    pub fn try_apply(&self, _memo: &mut Memo) -> usize {
        0
    }
}

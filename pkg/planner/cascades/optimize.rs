//! TiDB `pkg/planner/cascades/optimize.go` — Cascades entry.
//!
//! Production plans still come from RBO (`pkg/planner/core`). When
//! `SEALDB_CASCADES=1`, PlanBuilder invokes this pass as an identity hook that
//! seeds a memo, runs the task scheduler, and applies transformation rules
//! (EliminateTrue/FalseSelection, DeduplicateGroupExprs, JoinToApply stubs,
//! plus ImplementationRules / EnforcerRules fingerprint MVB).

use super::enforcer_rules::EnforcerRules;
use super::implementation_rules::ImplementationRules;
use super::memo::{Group, GroupExpr, Memo};
use super::task::task_scheduler::TaskScheduler;
use super::transformation_rules::TransformationRules;
use sealdb_errno::Result;

/// Result of a Cascades optimize pass.
#[derive(Clone, Debug, Default)]
pub struct OptimizeResult {
    pub applied_rules: usize,
    pub implemented: usize,
    pub enforced: usize,
    pub memo_groups: usize,
    pub scheduled_tasks: usize,
}

/// Whether PlanBuilder should invoke the Cascades identity pass.
pub fn cascades_enabled() -> bool {
    std::env::var("SEALDB_CASCADES")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Cascades optimize entry: seed memo from `root` label, explore, apply rules.
///
/// Does not rewrite the RBO physical plan returned to callers.
pub fn optimize(root: &str) -> Result<OptimizeResult> {
    let mut memo = Memo::default();
    let fingerprint = fingerprint_label(root);
    memo.groups.push(Group {
        id: 0,
        exprs: vec![GroupExpr { fingerprint }],
    });
    let mut sched = TaskScheduler::default();
    let scheduled_tasks = sched.run_explore(&mut memo);
    let applied_rules = TransformationRules.apply_all(&mut memo);
    let implemented = ImplementationRules.apply_all(&mut memo);
    let enforced = EnforcerRules.apply_all(&mut memo);
    Ok(OptimizeResult {
        applied_rules,
        implemented,
        enforced,
        memo_groups: memo.groups.len(),
        scheduled_tasks,
    })
}

/// Optional Cascades pass used by PlanBuilder.
pub fn maybe_optimize(root: &str) -> Result<OptimizeResult> {
    if !cascades_enabled() {
        return Ok(OptimizeResult::default());
    }
    optimize(root)
}

fn fingerprint_label(root: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascades::memo::{Group, GroupExpr, Memo};
    use crate::cascades::transformation_rules::TransformationRules;
    use crate::cascades::transformation_rules::true_selection_fingerprint;

    #[test]
    fn test_optimize_seeds_memo() {
        let out = optimize("select").unwrap();
        assert_eq!(out.memo_groups, 1);
        assert!(out.scheduled_tasks >= 1);
        // Root label is not a tautology Selection — no rule fires.
        assert_eq!(out.applied_rules, 0);
    }

    #[test]
    fn test_optimize_applies_true_selection_rule() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![GroupExpr {
                    fingerprint: true_selection_fingerprint(),
                }],
            }],
        };
        let applied = TransformationRules.apply_all(&mut memo);
        assert_eq!(applied, 1);
        assert!(memo.groups[0].exprs.is_empty());
    }

    #[test]
    fn test_maybe_optimize_respects_flag() {
        if cascades_enabled() {
            let out = maybe_optimize("select").unwrap();
            assert_eq!(out.memo_groups, 1);
        } else {
            let out = maybe_optimize("select").unwrap();
            assert_eq!(out.memo_groups, 0);
            assert_eq!(out.scheduled_tasks, 0);
        }
    }
}

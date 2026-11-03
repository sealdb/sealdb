//! TiDB `pkg/planner/cascades/optimize.go` — Cascades entry (stub).

use sealdb_errno::Result;

/// Result of a Cascades optimize pass (placeholder).
#[derive(Clone, Debug, Default)]
pub struct OptimizeResult {
    pub applied_rules: usize,
}

/// Cascades optimize entry. Currently a no-op; callers keep using RBO plans.
pub fn optimize(_root: &str) -> Result<OptimizeResult> {
    Ok(OptimizeResult::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimize_noop() {
        let out = optimize("select").unwrap();
        assert_eq!(out.applied_rules, 0);
    }
}

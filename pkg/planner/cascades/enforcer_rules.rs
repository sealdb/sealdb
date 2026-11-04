//! TiDB `pkg/planner/cascades/enforcer_rules.go`.

use super::memo::{GroupExpr, Memo};
use std::collections::HashSet;

/// Physical property enforcer rules (fingerprint MVB).
#[derive(Clone, Debug, Default)]
pub struct EnforcerRules;

fn fnv_label(label: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in label {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Required ordered property placeholder.
pub fn required_order_fingerprint() -> u64 {
    fnv_label(b"PropRequiredOrder")
}

/// PhysicalSort enforcer inserted when order is required but missing.
pub fn physical_sort_enforcer_fingerprint() -> u64 {
    fnv_label(b"PhysicalSortEnforcer")
}

impl EnforcerRules {
    /// If a group carries `PropRequiredOrder` without a sort enforcer, add one.
    pub fn apply_all(&self, memo: &mut Memo) -> usize {
        let mut added = 0;
        let need = required_order_fingerprint();
        let enforcer = physical_sort_enforcer_fingerprint();
        for group in &mut memo.groups {
            let fps: HashSet<u64> = group.exprs.iter().map(|e| e.fingerprint).collect();
            if fps.contains(&need) && !fps.contains(&enforcer) {
                group.exprs.push(GroupExpr {
                    fingerprint: enforcer,
                });
                added += 1;
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascades::memo::{Group, GroupExpr, Memo};

    #[test]
    fn test_enforce_required_order() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![GroupExpr {
                    fingerprint: required_order_fingerprint(),
                }],
            }],
        };
        assert_eq!(EnforcerRules.apply_all(&mut memo), 1);
        assert!(
            memo.groups[0]
                .exprs
                .iter()
                .any(|e| e.fingerprint == physical_sort_enforcer_fingerprint())
        );
        assert_eq!(EnforcerRules.apply_all(&mut memo), 0);
    }
}

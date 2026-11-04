//! TiDB `pkg/planner/cascades/transformation_rules.go`.

use super::memo::Memo;
use super::rule::join::JoinToApply;
use super::rule::{Rule, RuleType};

/// Logical transformation rules.
#[derive(Clone, Debug, Default)]
pub struct TransformationRules;

impl TransformationRules {
    pub fn default_set() -> Vec<Rule> {
        vec![
            Rule {
                tp: RuleType::EliminateTrueSelection,
            },
            Rule {
                tp: RuleType::JoinToApply,
            },
        ]
    }

    /// Apply registered transformation rules; returns total applications.
    pub fn apply_all(&self, memo: &mut Memo) -> usize {
        let mut applied = 0;
        for rule in Self::default_set() {
            applied += match rule.tp {
                RuleType::EliminateTrueSelection => eliminate_true_selection(memo),
                RuleType::JoinToApply => JoinToApply.try_apply(memo),
                RuleType::Unspecified => 0,
            };
        }
        applied
    }
}

/// Fingerprint used for tautology Selection placeholders in tests / hooks.
pub fn true_selection_fingerprint() -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in b"true" {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn eliminate_true_selection(memo: &mut Memo) -> usize {
    let target = true_selection_fingerprint();
    let mut removed = 0;
    for group in &mut memo.groups {
        let before = group.exprs.len();
        group.exprs.retain(|e| e.fingerprint != target);
        removed += before - group.exprs.len();
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascades::memo::{Group, GroupExpr, Memo};

    #[test]
    fn test_eliminate_true_selection() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![
                    GroupExpr {
                        fingerprint: true_selection_fingerprint(),
                    },
                    GroupExpr { fingerprint: 42 },
                ],
            }],
        };
        let n = TransformationRules.apply_all(&mut memo);
        assert_eq!(n, 1);
        assert_eq!(memo.groups[0].exprs.len(), 1);
        assert_eq!(memo.groups[0].exprs[0].fingerprint, 42);
    }
}

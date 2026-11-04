//! TiDB `pkg/planner/cascades/transformation_rules.go`.

use super::memo::Memo;
use super::rule::join::JoinToApply;
use super::rule::{Rule, RuleType};
use std::collections::HashSet;

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
                tp: RuleType::EliminateFalseSelection,
            },
            Rule {
                tp: RuleType::DeduplicateGroupExprs,
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
                RuleType::EliminateTrueSelection => {
                    eliminate_selection_fingerprint(memo, true_selection_fingerprint())
                }
                RuleType::EliminateFalseSelection => {
                    eliminate_selection_fingerprint(memo, false_selection_fingerprint())
                }
                RuleType::DeduplicateGroupExprs => deduplicate_group_exprs(memo),
                RuleType::JoinToApply => JoinToApply.try_apply(memo),
                RuleType::Unspecified => 0,
            };
        }
        applied
    }
}

fn fnv_label(label: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in label {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Fingerprint used for tautology Selection placeholders in tests / hooks.
pub fn true_selection_fingerprint() -> u64 {
    fnv_label(b"true")
}

/// Fingerprint used for contradiction Selection placeholders.
pub fn false_selection_fingerprint() -> u64 {
    fnv_label(b"false")
}

fn eliminate_selection_fingerprint(memo: &mut Memo, target: u64) -> usize {
    let mut removed = 0;
    for group in &mut memo.groups {
        let before = group.exprs.len();
        group.exprs.retain(|e| e.fingerprint != target);
        removed += before - group.exprs.len();
    }
    removed
}

fn deduplicate_group_exprs(memo: &mut Memo) -> usize {
    let mut removed = 0;
    for group in &mut memo.groups {
        let mut seen = HashSet::new();
        let before = group.exprs.len();
        group.exprs.retain(|e| seen.insert(e.fingerprint));
        removed += before - group.exprs.len();
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascades::memo::{Group, GroupExpr, Memo};

    #[test]
    fn test_eliminate_true_and_false_selection() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![
                    GroupExpr {
                        fingerprint: true_selection_fingerprint(),
                    },
                    GroupExpr {
                        fingerprint: false_selection_fingerprint(),
                    },
                    GroupExpr { fingerprint: 42 },
                ],
            }],
        };
        let n = TransformationRules.apply_all(&mut memo);
        assert_eq!(n, 2);
        assert_eq!(memo.groups[0].exprs.len(), 1);
        assert_eq!(memo.groups[0].exprs[0].fingerprint, 42);
    }

    #[test]
    fn test_deduplicate_group_exprs() {
        let mut memo = Memo {
            groups: vec![Group {
                id: 0,
                exprs: vec![
                    GroupExpr { fingerprint: 7 },
                    GroupExpr { fingerprint: 7 },
                    GroupExpr { fingerprint: 8 },
                ],
            }],
        };
        let n = deduplicate_group_exprs(&mut memo);
        assert_eq!(n, 1);
        assert_eq!(memo.groups[0].exprs.len(), 2);
    }
}

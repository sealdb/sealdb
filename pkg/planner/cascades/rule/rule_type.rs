//! TiDB `pkg/planner/cascades/rule/rule_type.go`.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RuleType {
    #[default]
    Unspecified,
    /// Join → Apply (decorrelate) — stub match only.
    JoinToApply,
    /// Drop tautology Selection (fingerprint of `"true"`).
    EliminateTrueSelection,
}

//! TiDB `pkg/planner/cascades/rule`.

pub mod binder;
pub mod join;
pub mod rule_type;

pub use rule_type::RuleType;

/// Cascades transformation / implementation rule (stub).
#[derive(Clone, Debug, Default)]
pub struct Rule {
    pub tp: RuleType,
}

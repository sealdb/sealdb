//! TiDB `pkg/planner/cascades` — Cascades optimizer (C6 stub).
//!
//! SealDB's production path remains rule-based (`pkg/planner/core`). Cascades
//! modules mirror TiDB filenames so package parity can track deepen work.

pub mod base;
pub mod enforcer_rules;
pub mod implementation_rules;
pub mod memo;
pub mod optimize;
pub mod rule;
pub mod stringer;
pub mod task;
pub mod transformation_rules;

pub use optimize::{OptimizeResult, cascades_enabled, maybe_optimize, optimize};

//! TiDB `pkg/planner/cascades/base`.

pub mod hash_equaler;

/// Marker trait for Cascades expressions (stub).
pub trait CascadesExpr: Send + Sync {}

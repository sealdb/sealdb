//! Plan cache value and helpers (TiDB `pkg/planner/core/plan_cache.go`, MVB).
//!
//! Prepared EXECUTE keys the Domain cache by the unbound template SQL
//! (see `plan_cache_param::normalize_prep_cache_key`); ad-hoc cached SQL still
//! uses restored bound SQL. Validity is gated by infoschema meta version.

use crate::core::physical_plans::PhysicalPlan;

/// Cached physical plan plus the schema version it was built against.
#[derive(Clone, Debug)]
pub struct PlanCacheValue {
    pub schema_ver: i64,
    pub plan: PhysicalPlan,
}

impl PlanCacheValue {
    pub fn new(schema_ver: i64, plan: PhysicalPlan) -> Self {
        Self { schema_ver, plan }
    }

    /// Returns the plan when `schema_ver` still matches.
    pub fn plan_if_valid(&self, schema_ver: i64) -> Option<PhysicalPlan> {
        if self.schema_ver == schema_ver {
            Some(self.plan.clone())
        } else {
            None
        }
    }
}

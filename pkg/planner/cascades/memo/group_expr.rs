//! TiDB `pkg/planner/cascades/memo/group_expr.go`.

#[derive(Clone, Debug, Default)]
pub struct GroupExpr {
    pub fingerprint: u64,
}

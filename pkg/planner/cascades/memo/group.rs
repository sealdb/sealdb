//! TiDB `pkg/planner/cascades/memo/group.go`.

use super::GroupExpr;

#[derive(Clone, Debug, Default)]
pub struct Group {
    pub id: u64,
    pub exprs: Vec<GroupExpr>,
}

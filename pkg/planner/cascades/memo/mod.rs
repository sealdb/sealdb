//! TiDB `pkg/planner/cascades/memo`.

pub mod group;
pub mod group_expr;
pub mod group_id_generator;
pub mod memo_expr;

pub use group::Group;
pub use group_expr::GroupExpr;
pub use group_id_generator::GroupIdGenerator;
pub use memo_expr::MemoExpr;

/// Cascades memo table (stub).
#[derive(Clone, Debug, Default)]
pub struct Memo {
    pub groups: Vec<Group>,
}

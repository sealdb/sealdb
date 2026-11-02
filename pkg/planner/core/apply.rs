/// F14 core: PhysicalApply / NestedLoopApply (TiDB `LogicalApply`).
/// Simple correlated EXISTS / IN / NOT EXISTS / `= ANY` are rewritten to
/// HashJoin Semi/Anti in `rule_semi_join` (F09). Completeness wave: parallel
/// apply, SemiJoinRewriter (semi→inner+agg).
pub struct LogicalApply;

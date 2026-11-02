/// F14 core: PhysicalApply / NestedLoopApply (TiDB `LogicalApply`).
/// Simple correlated EXISTS / IN / NOT EXISTS are rewritten to HashJoin
/// Semi/Anti in `rule_semi_join` (F09). Completeness wave: parallel apply,
/// SemiJoinRewriter (semi→inner+agg), ALL/ANY.
pub struct LogicalApply;

/// PhysicalApply / NestedLoopApply (TiDB `LogicalApply`).
/// Simple correlated EXISTS / IN / NOT EXISTS / `= ANY` are rewritten to
/// HashJoin Semi/Anti in `rule_semi_join`. Parallel apply is
/// `SEALDB_APPLY_CONCURRENCY` on NestedLoopApply. Completeness wave:
/// SemiJoinRewriter (semi→inner+agg).
pub struct LogicalApply;

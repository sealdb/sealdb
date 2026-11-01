/// F14 core: PhysicalApply / NestedLoopApply (TiDB `LogicalApply`).
/// Completeness wave / not this feature: parallel apply, semi/anti join
/// rewrite, decorrelation of ALL/ANY.
pub struct LogicalApply;

/// F16 core: PhysicalWindow (TiDB `LogicalWindow`).
/// Frame clauses are evaluated in WindowExec. Completeness wave / not this
/// feature: named windows, pipelined window, derive TopN from window.
pub struct LogicalWindow;

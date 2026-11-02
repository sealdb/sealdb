/// F16 core: PhysicalWindow (TiDB `LogicalWindow`).
/// Frame clauses and named `WINDOW w AS` specs are resolved in lowering /
/// WindowExec. WindowExec emits one chunk per shared PARTITION BY group.
/// Completeness wave / not this feature: derive TopN from window.
pub struct LogicalWindow;

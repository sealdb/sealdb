/// F16 core: PhysicalWindow (TiDB `LogicalWindow`).
/// Frame clauses and named `WINDOW w AS` specs are resolved in lowering /
/// WindowExec. Completeness wave / not this feature: pipelined window,
/// derive TopN from window.
pub struct LogicalWindow;

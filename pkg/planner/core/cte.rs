/// F17 core: PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// F14 derived tables (`FROM (SELECT ...) alias`) flatten into this node.
/// WITH RECURSIVE UNION/UNION ALL iterates in CTEExec. Completeness wave /
/// not this feature: CTETableReader shared storage, MATERIALIZED hint,
/// UNION in non-recursive CTE, CTE in JOIN/DML.
pub struct LogicalCTE;

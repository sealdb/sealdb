/// F17 core: PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// F14 derived tables (`FROM (SELECT ...) alias`) flatten into this node.
/// WITH RECURSIVE UNION/UNION ALL iterates in CTEExec. `AS NOT MATERIALIZED`
/// inlines the seed plan. Completeness wave / not this feature:
/// CTETableReader shared storage, UNION in non-recursive CTE, CTE in JOIN/DML.
pub struct LogicalCTE;

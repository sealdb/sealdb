/// F17 core: PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// F14 derived tables (`FROM (SELECT ...) alias`) flatten into this node.
/// WITH RECURSIVE UNION/UNION ALL iterates in CTEExec. `AS NOT MATERIALIZED`
/// inlines the seed plan. Non-recursive UNION/EXCEPT/INTERSECT is a Union
/// seed. Completeness wave / not this feature: CTETableReader shared
/// storage, CTE in JOIN/DML.
pub struct LogicalCTE;

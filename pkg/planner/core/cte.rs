/// F17 core: PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// F14 derived tables (`FROM (SELECT ...) alias`) flatten into this node.
/// Completeness wave / not this feature: recursive CTE, CTETableReader shared
/// storage, MATERIALIZED hint, UNION in CTE, CTE in JOIN/DML.
pub struct LogicalCTE;

/// F17 core: PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// Completeness wave / not this feature: recursive CTE, CTETableReader shared
/// storage, MATERIALIZED hint, UNION in CTE, CTE in JOIN/DML.
pub struct LogicalCTE;

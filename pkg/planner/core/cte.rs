/// PhysicalCTE (TiDB `LogicalCTE` / `LogicalCTETable`).
/// derived tables (`FROM (SELECT ...) alias`) flatten into this node.
/// WITH RECURSIVE UNION/UNION ALL iterates in CTEExec. `AS NOT MATERIALIZED`
/// inlines the seed plan. Non-recursive UNION/EXCEPT/INTERSECT is a Union
/// seed. Shared CTETableReader storage reuses one materialization per
/// statement. Completeness wave / not this feature: nested WITH.
pub struct LogicalCTE;

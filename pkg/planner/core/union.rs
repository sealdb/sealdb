/// F15 core: PhysicalUnion (TiDB `LogicalUnionAll` / set operations).
/// Completeness wave / not this feature: partition union, parallel union
/// workers, INTERSECT-tighter-than-UNION precedence.
pub struct LogicalUnionAll;

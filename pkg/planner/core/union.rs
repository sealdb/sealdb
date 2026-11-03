/// PhysicalUnion (TiDB `LogicalUnionAll` / set operations).
/// Parallel child drain is `SEALDB_UNION_CONCURRENCY` on UnionExec.
/// Completeness wave / not this feature: partition union,
/// INTERSECT-tighter-than-UNION precedence.
pub struct LogicalUnionAll;

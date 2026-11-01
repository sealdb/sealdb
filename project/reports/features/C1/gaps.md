# C1 gaps

C1 freezes the leftover denominator. It does not implement leftover SQL
and does not claim pass rates.

These stay out of the C1 door:

- Unmapped PlanBuilder kinds (`AdminStmt`, `TruncateTableStmt`, views,
  sequences, flashback, resource groups, and the rest listed in
  `project/features/leftover.md`)
- Stale gap labels kept as frozen ids (`F02:indexes`, `F12:pessimistic`)
  because C1 records the core-ported `gaps[]` as they were, not a rewrite
- Executing leftover integrationtest suites
- Cross-cluster TiDB write / SealDB read (still after a later C3 cut)
- C6 items remain deferred: Cascades, TiFlash/MPP, BR/Lightning, plugins

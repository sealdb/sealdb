# C2 gaps

C2 ports EXPLAIN row/json/dot/ANALYZE, DistSQL cop-task rows
(`TableFullScan` / `IndexRangeScan` under readers), costed access-path
skyline, two-table Inner JoinReorder, left/right-deep n-table DP
JoinReorder for Inner equality graphs, IndexMerge for OR of indexed
equalities (union partial index scans + table lookup), and Cost Model
v2 MVB (`CostVer2` + CPU/mem/net/scan factors driving JoinReorder/DP and
EXPLAIN `estCost`). These stay out of this door:

- `EXPLAIN FOR CONNECTION`
- FORMAT=brief/verbose extra columns and per-operator RuntimeStats
- Full bushy subset DP, outer-join assoc
- IndexJoin / MergeJoin skyline; IndexMerge intersection / cost vs TableReader
- Full Cost Model v2 per-operator GetPlanCostVer2, concurrency, TiFlash/MPP
- Running leftover `explain*` / `index_merge` / `index_join` integrationtest suites

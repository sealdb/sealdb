# C2 gaps

C2 ports EXPLAIN row/json/dot/ANALYZE, DistSQL cop-task rows
(`TableFullScan` / `IndexRangeScan` under readers), costed access-path
skyline, two-table Inner JoinReorder, and left/right-deep n-table DP
JoinReorder for Inner equality graphs. These stay out of this door:

- `EXPLAIN FOR CONNECTION`
- FORMAT=brief/verbose extra columns and per-operator RuntimeStats
- Full bushy subset DP, outer-join assoc
- IndexMerge / IndexJoin / MergeJoin skyline
- Cost model v1/v2 full factors, network, memory, concurrency
- Running leftover `explain*` integrationtest suites

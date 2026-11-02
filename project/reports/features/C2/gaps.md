# C2 gaps

C2 ports EXPLAIN row/json/dot/ANALYZE, DistSQL cop-task rows
(`TableFullScan` / `IndexRangeScan` under readers), costed access-path
skyline, and two-table Inner JoinReorder. These stay out of this door:

- `EXPLAIN FOR CONNECTION`
- FORMAT=brief/verbose extra columns and per-operator RuntimeStats
- n-table DP / greedy JoinReorder, bushy trees, outer-join assoc
- IndexMerge / IndexJoin / MergeJoin skyline
- Cost model v1/v2 full factors, network, memory, concurrency
- Running leftover `explain*` integrationtest suites

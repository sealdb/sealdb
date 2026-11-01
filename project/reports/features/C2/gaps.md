# C2 gaps

C2 ports EXPLAIN row/json/dot, costed access-path skyline, and
two-table Inner JoinReorder. These stay out of this door:

- `EXPLAIN ANALYZE` and `EXPLAIN FOR CONNECTION`
- Coprocessor task rows (`TableFullScan` under `TableReader`)
- FORMAT=brief/verbose extra columns and `hint` / `actRows`
- n-table DP / greedy JoinReorder, bushy trees, outer-join assoc
- IndexMerge / IndexJoin / MergeJoin skyline
- Cost model v1/v2 full factors, network, memory, concurrency
- Running leftover `explain*` integrationtest suites

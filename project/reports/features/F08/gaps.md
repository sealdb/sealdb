# F08 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.
HAVING / ROLLUP / GROUP BY ALL are rejected in
`pkg/parser/sqlparser_bridge/lower.rs`. HashAgg limits are marked on
`pkg/executor/aggregate/agg_hash_executor.rs`.

- HAVING
- COUNT(DISTINCT) / SUM(DISTINCT)
- StreamAgg
- HashAgg spill / parallel workers
- GROUP BY expressions beyond simple columns
- ROLLUP / CUBE / GROUPING SETS
- AVG decimal scale matching MySQL

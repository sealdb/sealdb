# F08 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.
ROLLUP / GROUP BY ALL stay rejected. HashAgg limits are marked on
`pkg/executor/aggregate/agg_hash_executor.rs`.

- SUM/AVG/MIN/MAX DISTINCT (COUNT(DISTINCT) is in)
- StreamAgg
- HashAgg spill / parallel workers
- GROUP BY expressions beyond simple columns
- ROLLUP / CUBE / GROUPING SETS
- AVG decimal scale matching MySQL

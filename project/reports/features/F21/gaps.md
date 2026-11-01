# F21 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/statistics/table.rs` / `pkg/executor/analyze.rs`.

- Histograms / TopN / CMSketch / correlation
- `ANALYZE TABLE t INDEX` / `ANALYZE TABLE t (cols)`
- Partition analyze / incremental / sampling
- Persist `mysql.stats_meta` / `mysql.stats_histograms`
- Auto-analyze
- Join / Sort / Limit cost model beyond IndexLookUp vs TableReader
- Pseudo-stats versioning (`PseudoRowCount` is documented, not wired)

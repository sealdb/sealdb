# F19 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/ddl/partition.rs` / `pkg/planner/core/rule_partition_processor.rs`.

- RANGE COLUMNS partition types
- Partition expression keys (only a single column name)
- Subpartition
- `ADD` / `DROP` / `REORGANIZE PARTITION`
- Dynamic prune / partition-name FROM (`t PARTITION (p0)`)
- First-column-as-handle still means duplicate ids overwrite within one physical id
- sqlparser-rs MySQL dialect cannot parse MySQL `PARTITION BY`; F19 strips it before lower

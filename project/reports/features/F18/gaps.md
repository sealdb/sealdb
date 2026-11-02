# F18 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/ddl/index.rs` / `pkg/ddl/alter.rs`.

- Async DDL job queue / coprocessor reorg workers / concurrent ingest
- `FULLTEXT` / prefix indexes
- Generated columns, partitions, multi-table ALTER
- Transactional DDL

# F18 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/ddl/index.rs` / `pkg/ddl/alter.rs`.

- TiDB DDL job queue / schema-state machine (`none` → `delete-only` → `write-only` → `public`)
- Reorg / concurrent ingest / `ALGORITHM` / `LOCK`
- `UNIQUE` / `PRIMARY KEY` / `FULLTEXT` / prefix indexes
- `ALTER TABLE CHANGE` / `MODIFY` / `RENAME`
- `ADD COLUMN AFTER` / `FIRST`
- Generated columns, partitions, multi-table ALTER
- Transactional DDL

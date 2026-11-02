# F10 gaps

These stay out of the core-ported door. Pick them up in the completeness wave
or F18. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/distsql.rs` / `pkg/meta/model/index.rs` /
`pkg/planner/core/planbuilder.rs`.

- Standalone `CREATE INDEX` / `DROP INDEX` / `ALTER TABLE ADD INDEX` (F18)
- FOREIGN KEY / CHECK / FULLTEXT / SPATIAL
- Composite lookup beyond first-column equality
- Prefix-length / functional indexes
- IndexMerge / lookup push-down / parallel workers
- Costed index-path enumeration and skyline
- Multiple NULLs in UNIQUE (MySQL allows them; current unique keys collide)

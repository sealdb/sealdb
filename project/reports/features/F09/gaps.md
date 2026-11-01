# F09 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.
Rejected at parse/lower time in `pkg/parser/sqlparser_bridge/lower.rs`
(`lower_from`). Runtime limits are marked on
`pkg/executor/join/hash_join_v1.rs`.

- Semi / anti / full outer join
- CROSS JOIN / comma joins
- Three or more tables
- USING / NATURAL
- Hash join spill and parallel workers
- Merge join / index join
- Non-equality ON predicates

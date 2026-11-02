# F09 gaps

Two-table Inner/Left/Right HashJoin and CROSS JOIN (including two-table comma
joins) are leftover-closed. CROSS JOIN materializes both sides in memory.

Rejected at parse/lower time in `pkg/parser/sqlparser_bridge/lower.rs`
(`lower_from` / `lower_table_with_joins`). Runtime limits are marked on
`pkg/executor/join/hash_join_v1.rs`.

- Semi / anti / full outer join
- Three or more tables
- USING / NATURAL
- Hash join recursive spill and parallel workers
- Merge join / index join
- Non-equality ON predicates
- CROSS JOIN disk spill (no join key to partition)

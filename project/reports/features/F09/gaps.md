# F09 gaps

Left-deep n-table Inner/Left/Right HashJoin, CROSS JOIN, and comma joins are
leftover-closed. CROSS JOIN materializes both sides in memory.

Rejected at parse/lower time in `pkg/parser/sqlparser_bridge/lower.rs`
(`lower_table_with_joins`). Runtime limits are marked on
`pkg/executor/join/hash_join_v1.rs`.

- Semi / anti / full outer join
- USING / NATURAL
- Hash join recursive spill and parallel workers
- Merge join / index join
- Non-equality ON predicates
- CROSS JOIN disk spill (no join key to partition)
- Bushy join trees / join reorder across more than two base tables

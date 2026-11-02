# F09 gaps

Left-deep n-table Inner/Left/Right/Semi/Anti HashJoin, CROSS JOIN, comma joins,
and simple EXISTS/IN/NOT EXISTS decorrelation are leftover-closed. CROSS JOIN
materializes both sides in memory. Semi/Anti emit the left schema only.

Rejected at parse/lower time in `pkg/parser/sqlparser_bridge/lower.rs`
(`lower_table_with_joins`). Runtime limits are marked on
`pkg/executor/join/hash_join_v1.rs`. NOT IN stays NestedLoopApply (null-aware).
F14 still owns `semi-anti-rewrite` (TiDB SemiJoinRewriter: semi → inner + agg).

- Full outer join
- Right semi / right anti
- USING / NATURAL
- Hash join recursive spill and parallel workers
- Merge join / index join
- Non-equality ON predicates
- CROSS JOIN disk spill (no join key to partition)
- Bushy join trees / join reorder across more than two base tables
- NOT IN null-aware anti join
- Composite join keys on Semi/Anti

# F15 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/unionexec/union.rs` / `pkg/planner/core/union.rs`.

- `EXCEPT` / `INTERSECT` / `MINUS`
- Parenthesized nested set operations as a single operand
- `UNION` inside EXISTS / IN / scalar subquery
- Parallel union workers
- `UNION DISTINCT BY NAME`

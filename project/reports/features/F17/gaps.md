# F17 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/cte.rs` / `pkg/planner/core/cte.rs`.

- `UNION` / `UNION ALL` inside a non-recursive CTE body
- Nested `WITH` inside a CTE query
- CTE referenced from `JOIN` / `INSERT` / `UPDATE` / `DELETE`
- Shared `CTETableReader` storage and multi-consumer reuse
- Qualified CTE names (`db.cte`)
- CTE `FROM` clause (PostgreSQL-style)

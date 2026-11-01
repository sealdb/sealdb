# F17 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/cte.rs` / `pkg/planner/core/cte.rs`.

- Recursive CTE (`WITH RECURSIVE`)
- `AS MATERIALIZED` / `AS NOT MATERIALIZED`
- `UNION` / `UNION ALL` inside a CTE body
- Nested `WITH` inside a CTE query
- CTE referenced from `JOIN` / `INSERT` / `UPDATE` / `DELETE`
- Shared `CTETableReader` storage and multi-consumer reuse
- Qualified CTE names (`db.cte`)
- CTE `FROM` clause (PostgreSQL-style)

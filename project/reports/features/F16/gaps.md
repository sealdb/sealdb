# F16 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/window.rs` / `pkg/planner/core/window.rs`.

- Named windows (`WINDOW w AS ...`)
- `LAG` / `LEAD` / `FIRST_VALUE` / `LAST_VALUE` / `NTH_VALUE` / `NTILE`
- `IGNORE NULLS` / `RESPECT NULLS`
- Pipelined window
- `GROUP BY` together with window functions
- Derive TopN from window

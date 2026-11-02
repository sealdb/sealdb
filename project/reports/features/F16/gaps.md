# F16 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/window.rs` / `pkg/planner/core/window.rs`.

- `GROUP BY` together with window functions
- Derive TopN from window

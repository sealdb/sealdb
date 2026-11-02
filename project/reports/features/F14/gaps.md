# F14 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/executor/join/apply.rs` / `pkg/planner/core/apply.rs`.

- Subquery `JOIN`
- Parallel apply
- Semi / anti join rewrite
- Decorrelated HashJoin rewrite of Apply

# F07 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.
Rejected at parse/lower time via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` (`lower_update` / `lower_delete`).
Also marked on `pkg/executor/update.rs` and `pkg/executor/delete.rs`.

- Multi-table UPDATE / DELETE
- UPDATE JOIN / UPDATE ... FROM
- ORDER BY / LIMIT on UPDATE / DELETE
- RETURNING
- Index maintenance after mutating the handle column (F10 writes index keys on the core single-table path; handle rewrite still completeness)
- Affected-row counts on the MySQL protocol

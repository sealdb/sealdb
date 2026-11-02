# F07 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.
ORDER BY / LIMIT on single-table UPDATE / DELETE is leftover-closed.
Multi-table / FROM / RETURNING stay rejected at parse/lower time via
`Error::unsupported` in `pkg/parser/sqlparser_bridge/lower.rs`.

- Multi-table UPDATE / DELETE
- UPDATE JOIN / UPDATE ... FROM
- RETURNING
- Index maintenance after mutating the handle column (F10 writes index keys on the core single-table path; handle rewrite still completeness)
- Affected-row counts on the MySQL protocol

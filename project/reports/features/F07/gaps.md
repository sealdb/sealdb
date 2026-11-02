# F07 gaps

Two-table UPDATE JOIN and multi-table DELETE (including USING) are leftover-closed.
ORDER BY / LIMIT on single-table UPDATE / DELETE remains supported; MySQL forbids
those clauses on multi-table DML and SealDB rejects them at parse/lower time.

Out of scope (not in TiDB v8.5.8 / MySQL 8.0):

- UPDATE / DELETE RETURNING (rejected at parse/lower via `Error::unsupported`)
- PostgreSQL-style UPDATE ... FROM

Still completeness:

- n-table / comma-join DML (F09 `n-table` / `cross-join`)
- Index maintenance after mutating the handle column
- Affected-row counts on the MySQL protocol

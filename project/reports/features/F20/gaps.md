# F20 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/privilege/privilege.rs` / `pkg/executor/grant.rs`.

- Roles (`GRANT ROLE` / `REVOKE ROLE` / `SET ROLE`)
- `WITH GRANT OPTION` / `GRANT OPTION FOR`
- `CREATE` / `ALTER` / `DROP USER`, `IDENTIFIED BY`
- Column-level grants, dynamic privileges, SEM
- Persist `mysql.user` / `mysql.db` / `mysql.tables_priv` to KV
- LDAP / auth plugins / resource groups
- Process-wide `privileges.SkipWithGrant` config (F20 uses Manager constructors)

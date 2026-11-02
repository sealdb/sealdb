# C4 gaps

C4 ports text PREPARE / EXECUTE / DEALLOCATE, a session plan cache
keyed by restored SQL + schema version, COM_STMT_PREPARE /
COM_STMT_EXECUTE / COM_STMT_CLOSE, session user variables
(`SET @a` / `SELECT @a` / `EXECUTE … USING @a`), and an in-memory
`CREATE` / `DROP` / `SHOW BINDING` path that rewrites matching SELECT
SQL via Domain-shared `sealdb-bindinfo`.
These stay open:

- Cross-session (shared) plan cache and cache invalidation by stats
- Plan cache parameter types and `?` typed placeholders
- Prepared statement max count / DEALLOCATE ALL
- Evolving bindings (capture, fuzzy digest, GLOBAL vs SESSION split, hint parse)

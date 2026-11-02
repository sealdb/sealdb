# C4 gaps

C4 ports text PREPARE / EXECUTE / DEALLOCATE, a Domain-shared
instance plan cache (`sealdb-planner` `InstancePlanCache` on Domain,
keyed by restored SQL + schema version, cross-session hits),
COM_STMT_PREPARE / COM_STMT_EXECUTE / COM_STMT_CLOSE, session user
variables (`SET @a` / `SELECT @a` / `EXECUTE … USING @a`), and an
in-memory `CREATE` / `DROP` / `SHOW BINDING` path that rewrites
matching SELECT SQL via Domain-shared `sealdb-bindinfo`.
These stay open:

- Plan cache parameter types and `?` typed placeholders
- Stats-driven plan cache invalidation / capacity tuning beyond MVB LRU
- Prepared statement max count / DEALLOCATE ALL
- Evolving bindings (capture, fuzzy digest, GLOBAL vs SESSION split, hint parse)

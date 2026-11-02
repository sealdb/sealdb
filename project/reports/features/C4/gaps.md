# C4 gaps

C4 ports text PREPARE / EXECUTE / DEALLOCATE, a session plan cache
keyed by restored SQL + schema version, COM_STMT_PREPARE /
COM_STMT_EXECUTE / COM_STMT_CLOSE, and session user variables
(`SET @a` / `SELECT @a` / `EXECUTE … USING @a`).
These stay open:

- CREATE / DROP / SHOW BINDING and evolving bindings
- Cross-session (shared) plan cache and cache invalidation by stats
- Plan cache parameter types and `?` typed placeholders
- Prepared statement max count / DEALLOCATE ALL

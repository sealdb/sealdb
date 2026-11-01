# C4 gaps

C4 ports text PREPARE / EXECUTE / DEALLOCATE and a session plan cache
keyed by restored SQL + schema version. These stay out of this door:

- COM_STMT_PREPARE / COM_STMT_EXECUTE binary protocol
- User variables (`USING @a`)
- CREATE / DROP / SHOW BINDING and evolving bindings
- Prepared statement max count / DEALLOCATE ALL
- Cross-session plan cache and cache invalidation by stats
- Plan cache parameter types and `?` typed placeholders

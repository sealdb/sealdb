# F02 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- Online DDL job queue, owner, SchemaDiff, MDL
- `CREATE TABLE LIKE` / `AS SELECT`
- Temporary, partitioned, and view tables
- Indexes, table-level constraints, foreign keys
- Database/table charset and collation options
- `USE` (unqualified `CREATE TABLE t` needs a current database)
- Persist meta to a real TiKV cluster
- Bootstrap `mysql` / `test` system schemas
- Exact FieldType JSON extras (`Elems`, `Array`) vs TiDB

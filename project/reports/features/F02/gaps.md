# F02 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- Online DDL job queue, owner, SchemaDiff, MDL
- `CREATE TABLE LIKE` / `AS SELECT`
- Temporary, partitioned, and view tables
- Indexes via standalone `CREATE/DROP INDEX` (F18); F10 only accepts `CREATE TABLE ... INDEX/KEY`
- Table-level UNIQUE / PRIMARY KEY / FOREIGN KEY / CHECK (F10 rejects these)
- Database/table charset and collation options
- `USE` (unqualified `CREATE TABLE t` needs a current database)
- Persist meta to a real TiKV cluster
- Bootstrap `mysql` / `test` system schemas
- Exact FieldType JSON extras (`Elems`, `Array`) vs TiDB

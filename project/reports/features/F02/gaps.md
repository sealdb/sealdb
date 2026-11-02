# F02 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- Async online DDL job queue, owner, SchemaDiff, MDL (`ADMIN SHOW DDL JOBS` leftover-closed)
- Temporary and view tables (`CREATE TABLE LIKE` / `AS SELECT` leftover-closed; LIKE copies columns/indexes/partitions without data; AS SELECT infers columns and inserts)
- Prefix / FULLTEXT indexes (`CREATE/DROP INDEX` leftover-closed)
- Table-level FOREIGN KEY / CHECK (UNIQUE / PRIMARY KEY are F10 leftover-closed)
- Database/table charset and collation options
- `USE` (unqualified `CREATE TABLE t` needs a current database)
- Bootstrap `mysql` / `test` system schemas
- Exact FieldType JSON extras (`Elems`, `Array`) vs TiDB

# F02 gaps

These stay out of the core-ported door. Pick them up in the completeness wave.

- Online DDL job queue, owner, SchemaDiff, MDL
- Temporary and view tables (`CREATE TABLE LIKE` / `AS SELECT` leftover-closed; LIKE copies columns/indexes/partitions without data; AS SELECT infers columns and inserts)
- Indexes via standalone `CREATE/DROP INDEX` (F18); F10 only accepts `CREATE TABLE ... INDEX/KEY`
- Table-level FOREIGN KEY / CHECK (UNIQUE / PRIMARY KEY are F10 leftover-closed)
- Database/table charset and collation options
- `USE` (unqualified `CREATE TABLE t` needs a current database)
- Bootstrap `mysql` / `test` system schemas
- Exact FieldType JSON extras (`Elems`, `Array`) vs TiDB

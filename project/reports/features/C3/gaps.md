# C3 gaps

C3 ports SchemaChecker (8028), READ COMMITTED statement snapshots,
and same-Domain cross-session read/write. These stay out of this door:

- TiKV / PD cross-cluster write then SealDB read (needs live cluster)
- Metadata lock (MDL) wait instead of immediate 8028
- `ErrInfoSchemaExpired` (8027) / schema lease
- SERIALIZABLE / READ UNCOMMITTED / SET TRANSACTION SNAPSHOT
- SAVEPOINT and transactional DDL
- Running leftover `db_integration` integrationtest suites

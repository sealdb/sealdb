# C3 gaps

C3 ports SchemaChecker (8028), READ COMMITTED statement snapshots,
same-Domain cross-session read/write, in-memory MDL, schema lease /
reload (8027), SERIALIZABLE (mapped to RR), and SAVEPOINT (F12).

Closed in this wave:

- `tikv-cross-cluster` → same-cluster multi-session on live TiKV when
  `SEALDB_TIKV=1` (skips otherwise). True multi-PD / cross-cluster stays out.
- `mdl` → Domain in-memory metadata lock: DML shared vs DDL exclusive
  on the same table key (1205 on conflict).
- `schema-lease` → Domain schema lease + `reload_schema_from_storage`
  hook; expired lease returns 8027.
- `serializable` → `SET` / `START TRANSACTION ISOLATION LEVEL SERIALIZABLE`
  accepted; MVB maps to RR snapshot semantics.
- `savepoint` → already core-ported in F12 (MemStorage + session); gated here.

These stay out:

- True multi-PD / cross-cluster TiKV (no fake PD clusters)
- MDL wait / queue (MVB fails immediately with 1205)
- TiDB oracle-like pessimistic SERIALIZABLE / SI
- TiKV SAVEPOINT (MemStorage only; trait default still unsupported)
- SAVEPOINT does not undo DDL; CREATE INDEX / ALTER backfill KV rollback (F18)
- Running leftover `db_integration` integrationtest suites

# TiKV client spike

Purpose: validate the Rust `tikv-client` 0.4.x surface before committing the
`pkg/store/driver` adapter design.

## Run

```bash
export PD_ENDPOINTS=127.0.0.1:2379
cargo run --manifest-path tools/spikes/tikv-client/Cargo.toml
```

A pinned local cluster can be started with `make docker-up` after Docker is
available in WSL.

## Checklist

- [ ] `TransactionClient::new` connects to PD
- [ ] `current_timestamp` returns a PD timestamp
- [ ] optimistic `begin` / `put` / `get` / `commit`
- [ ] pessimistic `begin_pessimistic` / `get_for_update`
- [ ] `Snapshot` read at a timestamp
- [ ] `RawClient::coprocessor` with a minimal DAG
- [ ] error mapping for region epoch / lock conflict
- [ ] TLS and API v2 / keyspace configuration

Record pass/fail and gaps in `project/decisions/0004-tikv-client-strategy.md`.

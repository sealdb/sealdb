# SealDB

SealDB is a Rust implementation of a TiDB-compatible SQL compute layer. It keeps PD and TiKV unchanged and focuses on the compute path: protocol, parsing, planning, execution, metadata, and transaction orchestration.

## Status

**Phase 0 — foundation.** The workspace, governance documents, parity checker, and parser oracle skeleton are in place. The legacy Rust scaffold has been archived under `legacy-rust-scaffold/` and is not part of the build.

## Unified baseline

| Component | Version |
|---|---|
| Compute-layer reference | TiDB `v8.5.8` |
| PD | `v8.5.8` |
| TiKV | `v8.5.8` |
| Rust | `1.99.1` (edition 2024) |
| SQL parser | `sqlparser` 0.63.x |
| TiKV client | `tikv-client` 0.4.x |

Local PD/TiKV checkouts may be on a newer development branch. Tests and integration runs must use the pinned `v8.5.8` line (for example, `pingcap/pd:v8.5.8` and `pingcap/tikv:v8.5.8`).

## Repository layout

```text
cmd/tidb-server/        process entry point
pkg/                    compute-layer crates (top-level package granularity)
tools/oracle/           Go oracle for AST/plan/result comparison
tests/                  Rust integration and conformance tests (to be populated)
project/                requirements, architecture, roadmap, progress, ADRs, skills
legacy-rust-scaffold/   archived pre-rewrite code (not built)
docker/                 local PD/TiKV environment
monitoring/             Prometheus/Grafana assets
config/                 configuration templates
```

Each top-level directory under `pkg/` is a Cargo crate. Subpackages remain Rust modules inside that crate, so directory names continue to follow the reference layout.

## Quick start

```bash
rustup toolchain install 1.99.1
cargo check --workspace
cargo test --workspace
cargo run --bin sealdb-server
```

Oracle smoke test:

```bash
cd tools/oracle
GOPROXY=https://goproxy.cn,direct go run ./cmd/tidb-oracle -sql "select 1" -pretty
```

Parity audit:

```bash
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root .
```

## Engineering rules

- `project/` is the source of truth for scope, phases, conventions, and progress.
- Source files do not carry upstream provenance markers. Port status is tracked centrally in `project/parity/status.json`.
- Every phase must pass its Gate before the next phase starts.
- PD and TiKV are consumed as external services; their source is not modified.

## Documentation

- [Engineering handbook](project/README.md)
- [Requirements](project/requirements.md)
- [Architecture](project/architecture.md)
- [Roadmap](project/roadmap.md)
- [Progress](project/progress.md)
- [Testing](project/testing.md)
- [Debugging](project/debugging.md)
- [ADRs](project/decisions/README.md)

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

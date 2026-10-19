# Archived Rust scaffold (pre-rewrite)

This directory contains the original `src/`, `tests/`, `docs/`, `Cargo.toml`, and `Makefile` from before the compute-layer rewrite.

Status: **archived, not built, not part of the workspace.**

Reasons:

- The parser was a keyword matcher, not a SQL parser.
- The storage transaction path was unimplemented.
- The planner/executor shape did not match the reference compute-layer architecture.
- The root manifest required an edition newer than the toolchain available at the time.

The active implementation lives in the repository root (`pkg/`, `cmd/`, `tools/`).  
This directory is kept only for historical reference; do not extend it.

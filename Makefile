SHELL := /bin/bash
TIDB_ROOT ?= /home/wslu/work/github/db/pingcap/tidb

.PHONY: help check build test fmt clippy parity oracle clean docker-up docker-down

help:
	@echo "check     cargo check --workspace"
	@echo "build     cargo build --workspace"
	@echo "test      cargo test --workspace"
	@echo "fmt       cargo fmt --all --check"
	@echo "clippy    cargo clippy --workspace --all-targets -- -D warnings"
	@echo "parity    run project/scripts/parity_check.py"
	@echo "oracle    parser oracle smoke test"
	@echo "clean     cargo clean"
	@echo "docker-up start pinned PD/TiKV"
	@echo "docker-down stop local cluster"

check:
	cargo check --workspace

build:
	cargo build --workspace

test:
	cargo test --workspace

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

parity:
	python3 project/scripts/parity_check.py --tidb-root "$(TIDB_ROOT)" --rust-root . --strict

oracle:
	cd tools/oracle && GOPROXY=https://goproxy.cn,direct go run ./cmd/tidb-oracle -sql "select 1" -pretty

clean:
	cargo clean

docker-up:
	docker compose -f docker/dev/docker-compose.yml up -d

docker-down:
	docker compose -f docker/dev/docker-compose.yml down

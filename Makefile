SHELL := /bin/bash
TIDB_ROOT ?= $(shell if [ -d /Users/wslu/work/github/db/pingcap/tidb ]; then echo /Users/wslu/work/github/db/pingcap/tidb; else echo /home/wslu/work/github/db/pingcap/tidb; fi)
CARGO ?= $(HOME)/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/cargo

.PHONY: help check build test fmt clippy parity oracle clean docker-up docker-down feature-gate leftover

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
	$(CARGO) check --workspace

build:
	$(CARGO) build --workspace

test:
	$(CARGO) test --workspace

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

feature-gate:
	./project/scripts/feature_gate.sh $(FEATURE)

leftover:
	python3 project/scripts/leftover_inventory.py --tidb-root "$(TIDB_ROOT)" --rust-root . --out project/features/leftover.md

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

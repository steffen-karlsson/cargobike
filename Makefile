.PHONY: build build-release test lint fmt fmt-check clippy coverage audit docs-check ci clean

build:
	cargo build

build-release:
	cargo build --release

test:
	cargo test --all-targets

lint: fmt-check clippy

docs-check:
	./scripts/docs-check.sh

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

coverage:
	cargo llvm-cov --workspace

audit:
	cargo audit

ci: fmt-check clippy docs-check test audit

clean:
	cargo clean

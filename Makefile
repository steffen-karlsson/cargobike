.PHONY: build build-release test lint fmt fmt-check clippy coverage audit ci clean

build:
	cargo build

build-release:
	cargo build --release

test:
	cargo test --all-targets

lint: fmt-check clippy

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

ci: fmt-check clippy test audit

clean:
	cargo clean

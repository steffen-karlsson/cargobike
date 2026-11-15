.PHONY: build build-release test lint fmt fmt-check clippy deny coverage audit docs-check hooks ci clean

build:
	cargo build

build-release:
	cargo build --release

# The crash-hooks feature matches CI's test job exactly: without it
# the engine's integration suite (incl. the crash harness) compiles
# EMPTY locally -- every run green for the wrong reason -- and the
# harness spawns stale featureless binaries. The bin pre-build
# refreshes `target/debug/engine-harness` -- what the crash harness
# spawns -- under the same feature (cargo test builds its own deps/
# artifact but never refreshes the root copy).
test:
	cargo build --bins --features cargobike-engine/crash-hooks
	cargo test --all-targets --features cargobike-engine/crash-hooks

lint: fmt-check clippy deny

docs-check:
	./scripts/docs-check.sh

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

hooks:
	git config core.hooksPath .githooks

clippy:
	cargo clippy --all-targets -- -D warnings

# The supply-chain gate CI's Deny policy job runs (advisories, licence
# policy, duplicate bans). The advisory DB fetches on first run;
# `cargo install cargo-deny --locked` installs the binary.
deny:
	cargo deny check

coverage:
	cargo llvm-cov --workspace

audit:
	cargo audit

ci: fmt-check clippy docs-check test audit deny

clean:
	cargo clean

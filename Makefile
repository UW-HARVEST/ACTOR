.PHONY: test
test:
	RUSTFLAGS="-D warnings" cargo build --all-targets
	RUSTFLAGS="-D warnings" cargo test
	RUSTFLAGS="-D warnings" cargo clippy --all-targets
	cargo fmt --check
	cd nightly && \
		RUSTFLAGS="-D warnings" cargo miri test --manifest-path=../Cargo.toml

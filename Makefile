TOOLCHAIN := $(shell awk -F'"' '/^channel[[:space:]]*=/ { print $$2; exit }' rust-toolchain.toml)
CARGO := rustup run $(TOOLCHAIN) cargo

test:
	$(CARGO) test --all-targets

format:
	$(CARGO) fmt --all

lint:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --all-targets --all-features -- --deny warnings

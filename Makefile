# Convenience targets. The Debian package has its own Makefile in dpkg/.

.PHONY: check test deploy-and-test

# What CI runs, so that a push does not discover something this could have
check:
	cargo fmt --all --check
	cargo clippy --locked --all-targets --all-features -- --deny warnings
	cargo clippy --locked --all-targets -- --deny warnings
	cargo test --locked --all-features

test:
	cargo test --all-features

# Build for the Raspberry Pi, copy to it, and run the checks there. Configure with PI_HOST and
# PI_USER; see scripts/device-test.sh for the rest. Run this from a Linux shell (WSL on Windows).
deploy-and-test:
	@bash scripts/device-test.sh

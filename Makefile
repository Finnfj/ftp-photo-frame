# Convenience targets. The Debian package has its own Makefile in dpkg/.

.PHONY: check test deploy-and-test soak

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

# Watch a running frame for hours and report when it stops. Read-only: it asks for /api/status and
# nothing else, so it is safe to point at the frame you actually use. Configure with BASE and AUTH.
soak:
	@bash scripts/soak.sh

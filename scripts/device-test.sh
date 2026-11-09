#!/usr/bin/env bash
# Builds the frame for the Raspberry Pi, copies it and a test harness to the device, and runs the
# checks there. Nothing is installed on the device and a frame already running on it is left alone.
#
#   PI_HOST   where to connect           (default raspberrypi.local)
#   PI_USER   user to connect as         (default pi)
#   PI_DIR    where to put the payload   (default ~/photo-frame-device-test)
#   BINARY    skip the build and use this binary instead
#   TARGET    rust target                (default aarch64-unknown-linux-gnu)
#
# Run it from a Linux shell. On Windows that means WSL: `cross` installs a Linux toolchain into
# rustup, which Windows rustup refuses.
set -uo pipefail

PI_HOST="${PI_HOST:-raspberrypi.local}"
PI_USER="${PI_USER:-pi}"
PI_DIR="${PI_DIR:-photo-frame-device-test}"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="$PI_USER@$PI_HOST"

say() { echo "==> $*"; }
die() { echo "error: $*" >&2; exit 1; }

say "checking the device is reachable"
ssh -o ConnectTimeout=10 -o BatchMode=yes "$REMOTE" true 2>/dev/null \
  || die "cannot ssh to $REMOTE without a password. Set up key authentication, or set PI_HOST/PI_USER."
ARCH=$(ssh "$REMOTE" uname -m)
say "  $REMOTE is up, architecture $ARCH"

BINARY="${BINARY:-}"
if [ -z "$BINARY" ]; then
  command -v cross > /dev/null 2>&1 \
    || die "cross is not installed (cargo install cross), or pass BINARY=path to skip the build"
  say "cross-building for $TARGET (this takes a few minutes the first time)"
  (cd "$REPO" && cross build --release --all-features --target "$TARGET") \
    || die "the cross build failed. If it complains about exec format, register the QEMU handlers:
  docker run --privileged --rm multiarch/qemu-user-static --reset -p yes"
  BINARY="$REPO/target/$TARGET/release/syno-photo-frame"
fi
[ -f "$BINARY" ] || die "no binary at $BINARY"
say "using $BINARY"
case "$ARCH" in
  aarch64|arm64) ;;
  *) echo "warning: the device reports $ARCH; make sure TARGET matches it" >&2 ;;
esac

say "assembling the payload"
PAYLOAD=$(mktemp -d)
trap 'rm -rf "$PAYLOAD"' EXIT
mkdir -p "$PAYLOAD/fixtures"
cp "$BINARY" "$PAYLOAD/syno-photo-frame"
cp "$REPO/scripts/device/run-checks.sh" "$PAYLOAD/"
cp "$REPO/scripts/device/test-ftp-server.py" "$PAYLOAD/"
cp "$REPO/scripts/web-app-checks.sh" "$PAYLOAD/"
# Real photos, so that thumbnail rendering and EXIF handling are exercised rather than stubbed
cp "$REPO/assets/test_loading.jpeg" "$PAYLOAD/fixtures/one.jpeg"
cp "$REPO/assets/Error.jpeg" "$PAYLOAD/fixtures/two.jpeg"
chmod +x "$PAYLOAD/syno-photo-frame" "$PAYLOAD/run-checks.sh" "$PAYLOAD/web-app-checks.sh"

say "copying it to $REMOTE:$PI_DIR"
ssh "$REMOTE" "rm -rf ~/$PI_DIR && mkdir -p ~/$PI_DIR" || die "could not prepare $PI_DIR on the device"
if command -v rsync > /dev/null 2>&1 && ssh "$REMOTE" "command -v rsync > /dev/null 2>&1"; then
  rsync -a "$PAYLOAD/" "$REMOTE:$PI_DIR/" || die "rsync failed"
else
  scp -q -r "$PAYLOAD/." "$REMOTE:$PI_DIR/" || die "scp failed"
fi

say "running the checks on the device"
echo
ssh "$REMOTE" "cd ~/$PI_DIR && RUST_LOG=${RUST_LOG:-info} bash run-checks.sh"
result=$?

echo
if [ "$result" -eq 0 ]; then
  say "the device passed"
else
  say "the device FAILED. The payload is left at $REMOTE:~/$PI_DIR for poking at,"
  say "and the frame's own log is at ~/.cache/photo-frame-device-test/frame.log there."
fi
exit "$result"

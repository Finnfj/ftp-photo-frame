#!/usr/bin/env bash
# Cross-compiles the frame for 64-bit Raspberry Pi OS inside docker/Dockerfile's image.
#
# The binary lands where a cross build used to put it:
#   target/aarch64-unknown-linux-gnu/release/syno-photo-frame
#
#   ENGINE     docker or podman   (default: whichever is installed, docker first)
#   PROFILE    release or debug   (default: release)
#   FEATURES   passed to cargo    (default: --all-features)
#   MAX_GLIBC  the oldest glibc the result must run on (default: 2.36, Raspberry Pi OS bookworm)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${IMAGE:-syno-photo-frame-aarch64-bookworm}"
PROFILE="${PROFILE:-release}"
FEATURES="${FEATURES:---all-features}"
MAX_GLIBC="${MAX_GLIBC:-2.36}"
TARGET=aarch64-unknown-linux-gnu

ENGINE="${ENGINE:-}"
if [ -z "$ENGINE" ]; then
  if command -v docker > /dev/null 2>&1; then
    ENGINE=docker
  elif command -v podman > /dev/null 2>&1; then
    ENGINE=podman
  else
    echo "neither docker nor podman is installed" >&2
    exit 1
  fi
fi
echo "==> using $ENGINE"

echo "==> building the image (slow the first time, cached afterwards)"
"$ENGINE" build -t "$IMAGE" -f "$REPO/docker/Dockerfile" "$REPO/docker"

echo "==> the glibc this will link against"
"$ENGINE" run --rm "$IMAGE" bash -c 'dpkg -s libc6:arm64 | sed -n "s/^Version: /    /p"'

echo "==> cross-compiling"
profile_flag=""
[ "$PROFILE" = "release" ] && profile_flag="--release"
# :Z is ignored by docker and is what podman needs under SELinux
"$ENGINE" run --rm \
  -v "$REPO":/work:Z \
  -w /work \
  "$IMAGE" \
  cargo build --locked $profile_flag $FEATURES --target "$TARGET"

binary="$REPO/target/$TARGET/$PROFILE/syno-photo-frame"
echo "==> $binary"
file "$binary" | sed 's/^/    /'

# A binary that builds and then cannot start is the failure this image exists to prevent, and the
# only reason it went unnoticed was that nothing ever checked: CI built the artifact and never ran
# it. Checking here turns that into a build failure instead of a discovery on the device.
if ! command -v objdump > /dev/null 2>&1; then
  echo "==> objdump not available, skipping the glibc check" >&2
  exit 0
fi

highest=$(objdump -T "$binary" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sed 's/GLIBC_//' | sort -Vu | tail -1)
echo "==> highest glibc symbol required: ${highest:-none} (the device provides $MAX_GLIBC)"

if [ -n "$highest" ]; then
  newest=$(sort -V <<EOF | tail -1
$MAX_GLIBC
$highest
EOF
  )
  if [ "$newest" != "$MAX_GLIBC" ]; then
    echo >&2
    echo "ERROR: this binary needs glibc $highest and will not start on Raspberry Pi OS." >&2
    echo "       Something moved the base image forward. docker/Dockerfile has to stay on a" >&2
    echo "       distribution whose glibc is no newer than $MAX_GLIBC." >&2
    exit 1
  fi
fi

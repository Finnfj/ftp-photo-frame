# Cross-compilation image for 64-bit Raspberry Pi OS. Built and used by scripts/build-aarch64.sh.
#
# Based on Debian bookworm because that is what Raspberry Pi OS is, and a binary cannot start on a
# system whose glibc is older than the one it was linked against. Taking the target libraries from
# bookworm's own arm64 packages pins that floor to the device's 2.36 by construction.
#
# This replaced an image based on the stock cross-rs one. That image is tracked by a floating
# "main" tag, which moved to Ubuntu 24.04 and glibc 2.39, and the artifact silently became
# undeployable: it built and uploaded perfectly and then refused to start on the Pi with
#   error while loading shared libraries: version `GLIBC_2.38' not found
# Nothing in CI noticed, because CI only ever built the binary and never ran it.
#
# Installing the arm64 libraries here needs no QEMU: they are plain library packages whose
# maintainer scripts do not execute target binaries.
FROM debian:bookworm

RUN dpkg --add-architecture arm64 \
    && apt-get update \
    && apt-get install --assume-yes --no-install-recommends \
    ca-certificates \
    curl \
    build-essential \
    # aws-lc-sys, pulled in by reqwest, compiles C code with cmake
    cmake \
    pkg-config \
    # the cross toolchain
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    # the libraries the sdl2 crate links against, for the target architecture
    libsdl2-dev:arm64 \
    libsdl2-gfx-dev:arm64 \
    libsdl2-ttf-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustup target add aarch64-unknown-linux-gnu

# Which compiler to use for the target, for cargo, cc-rs and cmake-rs in turn. pkg-config refuses
# to report another architecture's libraries unless told that this is intentional, and has to be
# pointed at the multiarch directory holding them.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR=/

WORKDIR /work

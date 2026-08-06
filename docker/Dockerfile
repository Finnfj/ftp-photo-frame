# Cross-compilation image for aarch64 (64bit Raspberry Pi OS), used by cross-rs. See BUILD.md.
#
# Based on the stock cross-rs image, which already provides the Rust toolchain, the
# aarch64-linux-gnu cross toolchain and a matching sysroot, so only the libraries this project links
# against have to be added.
#
# Requires the host to be able to execute aarch64 binaries, because installing the target
# architecture's packages below runs their maintainer scripts. Register the handlers once with:
#
#     docker run --privileged --rm multiarch/qemu-user-static --reset -p yes
#
# The build itself is not emulated. Note that the tag is pinned to the image built from cross's main
# branch rather than taken from the CROSS_BASE_IMAGE build argument: the image matching the released
# cross 0.2.5 is based on Ubuntu 16.04, whose package archive no longer exists.
FROM ghcr.io/cross-rs/aarch64-unknown-linux-gnu:main

RUN dpkg --add-architecture arm64 \
    && apt-get update \
    && apt-get install --assume-yes --no-install-recommends \
    # aws-lc-sys, pulled in by reqwest, compiles C code with cmake
    cmake \
    pkg-config \
    # The sdl2 crate is built with the gfx and ttf features
    libsdl2-dev:arm64 \
    libsdl2-gfx-dev:arm64 \
    libsdl2-ttf-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

# pkg-config refuses to report libraries of a foreign architecture unless it is told that this is
# intentional, and has to be pointed at the multiarch directory holding them
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
ENV PKG_CONFIG_SYSROOT_DIR=/

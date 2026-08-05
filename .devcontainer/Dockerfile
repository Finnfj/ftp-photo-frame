# Development container: builds and tests for the host architecture, and can cross-compile for the
# Raspberry Pi through cross-rs. See BUILD.md.
FROM debian:trixie

RUN apt-get update \
    && apt-get install --assume-yes --no-install-recommends \
    ca-certificates \
    curl \
    git \
    build-essential \
    cmake \
    pkg-config \
    # Needed to link the test binaries for the host
    libsdl2-dev \
    libsdl2-gfx-dev \
    libsdl2-ttf-dev \
    # Needed to link when cross-compiling outside of cross-rs
    gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --component clippy rustfmt \
    && rustup target add aarch64-unknown-linux-gnu \
    && cargo install cross --git https://github.com/cross-rs/cross \
    && chmod -R a+w $CARGO_HOME $RUSTUP_HOME

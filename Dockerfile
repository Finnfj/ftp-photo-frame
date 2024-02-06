##
# This file can be used to cross-compile e.g. from an amd64 machine to
# arm64 (Raspberry Pi), or to build the app without installing Rust
# locally.
#
# To cross-compile the Debian package, you need a builder supporting
# the target platform. E.g. to build for arm64 run the following
# command once to create the builder:
#
#     docker buildx create --name cross --bootstrap --platform linux/arm64
#
# Build Debian package and copy it to current directory:
#
#     docker build . --builder cross --platform linux/arm64 --target=dpkg --output type=local,dest=.
#
# Build just the binary (note that in order to execute it afterwards,
# libsdl2-2.0.0 and libsdl2-ttf-2.0.0 dependency packages need to be
# installed):
#
#     docker build . --builder cross --platform linux/arm64 --target=bin --output type=local,dest=.
#
# To build for current architecture (not cross-compile) you can use
# the default builder (skip the `docker buildx` command above) and
# omit the `--builder` and `--platform` options.
#
# To build for Debian bullseye change the build stage base image
# (`FROM ...`) to rust:bullseye.
#
# To cross compile for 32bit Raspberry Pi use the `--platform
# linux/arm/v7` option in the commands above.
##

FROM rust:bookworm as build

RUN apt update && \
    apt install -y libsdl2-dev libsdl2-ttf-dev libssl-dev lintian && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
# Cache dependencies build
RUN cargo init --name syno-photo-frame --vcs none .
COPY Cargo.toml Cargo.lock .
RUN cargo build --release
# Build the binary and Debian package
COPY . .
WORKDIR /workspace/dpkg
RUN make

# The following two stages can be used as --target options for `docker
# build` so it is possible to extract the artifacts from Docker images
# to local file-system
FROM scratch as dpkg
COPY --from=build /workspace/dpkg/syno-photo-frame_*.deb .

FROM scratch as bin
COPY --from=build /workspace/target/release/syno-photo-frame .

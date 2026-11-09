# Build Instructions

This describes building and testing this fork. For installing the app on a
Raspberry Pi, see [README.md](README.md).

## Host build and tests

The tests link against SDL2, so its development packages have to be installed
even though no window is opened:

```bash
sudo apt-get install --assume-yes build-essential cmake pkg-config libsdl2-dev libsdl2-gfx-dev libsdl2-ttf-dev
```

`libsdl2-gfx-dev` and `libsdl2-ttf-dev` are needed because the `sdl2` crate is
built with its `gfx` and `ttf` features, and `cmake` because `aws-lc-sys`, which
`reqwest` depends on, compiles C code.

Then:

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- --deny warnings
cargo fmt --all --check
```

`--all-features` also covers `ftps` (FTPS support), `motion-sensor` (GPIO
support) and `web` (the web app for managing photos, see
[Web App](README.md#web-app)), none of which are enabled by default.

To look at the web app's page while working on it, without an FTP server:

```bash
cargo test --all-features -- --ignored --nocapture serve_a_fake_album
```

It serves a made-up album on <http://127.0.0.1:8099/> until interrupted, and
prints the credentials to log in with.

## Cross-compiling for the Raspberry Pi

The target is a 64bit Raspberry Pi OS, i.e. `aarch64-unknown-linux-gnu`. Note
that `.cargo/config.toml` deliberately does *not* set a default target, so plain
`cargo` commands keep building for the host.

[cross](https://github.com/cross-rs/cross) and a container engine (Docker or
Podman) are needed:

```bash
bash scripts/build-aarch64.sh
```

The binary ends up in `target/aarch64-unknown-linux-gnu/release/syno-photo-frame`.
It needs Docker or Podman; the script uses whichever is installed and takes
`ENGINE=podman` to be told explicitly.

`docker/Dockerfile` describes the image: Debian bookworm, the aarch64 toolchain,
and the SDL2 libraries for the target architecture. Bookworm specifically,
because that is what Raspberry Pi OS is. A binary cannot start on a system whose
glibc is older than the one it was linked against, so the base image sets the
floor, and the script refuses to finish if the result needs anything newer than
glibc 2.36:

```
ERROR: this binary needs glibc 2.39 and will not start on Raspberry Pi OS.
```

That check exists because the opposite happened. The image used to be based on
the stock cross-rs one, tracked by a floating `main` tag; when that moved to
Ubuntu 24.04 the artifact kept building and uploading and silently stopped being
able to run on the device. CI could not catch it, because building a binary says
nothing about whether it starts.

The first build is slow, as the image has to be made once. No QEMU registration
is needed: the arm64 packages are libraries whose maintainer scripts do not run
target binaries.

Copy the binary to the Pi, for example:

```bash
scp target/aarch64-unknown-linux-gnu/release/syno-photo-frame pi@raspberrypi.local:
```

The Pi needs the runtime libraries, not the development ones:

```bash
sudo apt-get install --assume-yes libsdl2-2.0-0 libsdl2-gfx-1.0-0 libsdl2-ttf-2.0-0
```

## Dev container

`.devcontainer/` describes a container with everything above already installed,
including `cross`, and is picked up automatically by VS Code.

## Alternative: the upstream Dockerfile

The [Dockerfile](Dockerfile) inherited from upstream builds a Debian package, and
does so by compiling *natively* under QEMU emulation rather than
cross-compiling. It is slower, but a useful fallback whenever cross-compilation
runs into trouble linking SDL2 or `aws-lc-sys`:

```bash
docker buildx create --name cross --bootstrap --platform linux/arm64
docker build . --builder cross --platform linux/arm64 --target=dpkg --output type=local,dest=.
```

See the comments at the top of that file for details.

## Continuous integration

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs the formatting, lint
and test checks for the host on every push and pull request, and builds a release
binary for aarch64 which it uploads as a build artifact.

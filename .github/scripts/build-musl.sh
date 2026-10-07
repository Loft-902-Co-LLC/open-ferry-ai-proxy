#!/bin/sh
# Builds open-ferry for a musl target, fully static.
#
# Usage, from the repository root, in the Rust Alpine image the release
# workflow pins (see RELEASING.md):
#
#   TARGET=x86_64-unknown-linux-musl sh .github/scripts/build-musl.sh
#
# Alpine's C toolchain (GCC and musl-dev, both in the image) targets musl,
# which the C code of rustls's crypto library, aws-lc-rs, and of SQLite is
# compiled for. aws-lc-rs builds with its cc builder on these targets, with
# pregenerated bindings: no CMake, Go, NASM or bindgen.
#
# The binary is target/$TARGET/release/open-ferry. check-static.sh checks
# that it is static.
set -eu

if [ -z "${TARGET:-}" ]; then
  echo "build-musl.sh: set TARGET to the musl target to build" >&2
  exit 2
fi
case $TARGET in
  *-unknown-linux-musl) ;;
  *)
    echo "build-musl.sh: $TARGET isn't a musl target" >&2
    exit 2
    ;;
esac

# The toolchain rust-toolchain.toml names, as on the other runners.
rustup toolchain install --profile minimal

# The build is native: Alpine's host target is the musl target itself.
host=$(rustc -vV | sed -n 's/^host: //p')
if [ "$host" != "$TARGET" ]; then
  echo "::error::The build's host is $host, not $TARGET"
  exit 1
fi

# Static linking (crt-static) is the musl targets' default; the release
# workflow also sets it in CARGO_TARGET_<TARGET>_RUSTFLAGS. With --target,
# those flags reach only the target's code, not build scripts or proc
# macros, which must stay dynamic.
cargo build --release --locked -p open-ferry --target "$TARGET"

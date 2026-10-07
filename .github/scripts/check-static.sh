#!/bin/sh
# Checks that a Linux binary is statically linked: no program interpreter
# (dynamic loader) and no shared libraries it needs.
#
# Usage: sh .github/scripts/check-static.sh BINARY
#
# Needs readelf (binutils). Also prints what file says of the binary, when
# file is installed; that's for the log only, as file before 5.40 calls a
# static PIE "dynamically linked".
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: check-static.sh BINARY" >&2
  exit 2
fi
binary=$1
if [ ! -f "$binary" ]; then
  echo "::error::$binary doesn't exist"
  exit 1
fi

if command -v file >/dev/null 2>&1; then
  echo "$binary: $(file -b "$binary")"
fi

headers=$(readelf -hlW "$binary")
if ! printf '%s\n' "$headers" | grep -Eq 'Type:[[:space:]]+(EXEC|DYN)'; then
  echo "::error::$binary isn't an ELF executable"
  exit 1
fi
# A dynamic binary names its loader in an INTERP program header, and each
# library it needs in a NEEDED dynamic entry. A static PIE has a dynamic
# section, for its own relocations, but neither.
if printf '%s\n' "$headers" | grep -q 'INTERP'; then
  echo "::error::$binary names a program interpreter, so it isn't static:"
  printf '%s\n' "$headers" | grep 'interpreter' || true
  exit 1
fi
needed=$(readelf -dW "$binary" | grep '(NEEDED)' || true)
if [ -n "$needed" ]; then
  echo "::error::$binary needs shared libraries:"
  printf '%s\n' "$needed"
  exit 1
fi
echo "$binary is static: no program interpreter, no shared libraries"

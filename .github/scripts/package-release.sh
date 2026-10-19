#!/usr/bin/env bash
# The release and the throwaway qualification use exactly the same archive layout.
set -euo pipefail

name="open-ferry-$VERSION-$TARGET"
exe=open-ferry
if [ "$RUNNER_OS" = Windows ]; then exe=open-ferry.exe; fi
mkdir -p "dist/$name"
cp "target/$TARGET/release/$exe" LICENSE README.md config.example.yaml "dist/$name/"
if [ "$RUNNER_OS" != Windows ]; then chmod 755 "dist/$name/$exe"; fi
cp -R licenses "dist/$name/"
cp dashboard/dist/third-party-licenses.txt "dist/$name/licenses/dashboard-third-party-licenses.txt"
if [ ! -s "rust-licenses/$TARGET.txt" ]; then
  echo "::error::The rust-licenses artifact has no $TARGET.txt: add the target to .github/scripts/rust-licenses.py"
  exit 1
fi
cp "rust-licenses/$TARGET.txt" "dist/$name/licenses/rust-third-party-licenses.txt"
cd dist
if [ "$RUNNER_OS" = Windows ]; then
  # Windows' bsdtar, as in the original release Package step.
  "$(cygpath "$SYSTEMROOT")/System32/tar.exe" -a -cf "$name.zip" "$name"
  archive="$name.zip"
else
  tar -czf "$name.tar.gz" "$name"
  archive="$name.tar.gz"
fi
rm -rf "$name"
echo "archive=dist/$archive" >> "$GITHUB_OUTPUT"

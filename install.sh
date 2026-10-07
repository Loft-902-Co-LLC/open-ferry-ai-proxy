#!/bin/sh
# Installs open-ferry on Linux or macOS from a GitHub release.
#
#   curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh
#
# It downloads the release's archive for this system and checks it against
# the release's SHA256SUMS, and, when the GitHub CLI (gh) is installed,
# against the archive's build provenance attestation. It installs the
# binary as ~/.local/bin/open-ferry, and, when there is no config at
# $XDG_CONFIG_HOME/open-ferry/config.yaml (~/.config/open-ferry/config.yaml
# when XDG_CONFIG_HOME isn't an absolute path), writes one with
# `open-ferry init`. It edits no shell profile and no PATH.
#
# Run `sh install.sh --help` for the options. Environment:
#   OPEN_FERRY_INSTALL_BASE_URL  where releases are downloaded from, in
#                                place of the GitHub repository's releases
#                                URL (for a mirror, or a test server)
#   OPEN_FERRY_INSTALL_GH        the GitHub CLI command (default: gh)
#
# See https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy#install
set -eu

REPO=Loft-902-Co-LLC/open-ferry-ai-proxy
GLIBC_FLOOR=2.31

usage() {
  cat <<'EOF'
Installs open-ferry from a GitHub release.

Usage: install.sh [options]

Options:
  --version VERSION  install this version (such as 0.1.0) rather than the
                     latest release
  --target TARGET    install the build for this target rather than this
                     system's, such as x86_64-unknown-linux-musl
  --bin-dir DIR      install the binary in DIR (default: ~/.local/bin)
  --config PATH      the config to keep, or to write when there is none
                     (default: $XDG_CONFIG_HOME/open-ferry/config.yaml, or
                     ~/.config/open-ferry/config.yaml when XDG_CONFIG_HOME
                     isn't an absolute path)
  --no-attestation   don't check the build provenance attestation, even
                     when gh is installed; the SHA256SUMS check still runs
  -h, --help         show this help
EOF
}

say() {
  printf '%s\n' "$*"
}

die() {
  printf 'install.sh: error: %s\n' "$*" >&2
  exit 1
}

usage_error() {
  printf 'install.sh: %s\n' "$*" >&2
  printf 'Run install.sh --help for the options.\n' >&2
  exit 2
}

has() {
  command -v "$1" >/dev/null 2>&1
}

# --- Options -----------------------------------------------------------------

version=
target=
bin_dir=
config=
attestation=1
while [ "$#" -gt 0 ]; do
  case $1 in
    --version | --target | --bin-dir | --config)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        usage_error "$1 needs a value"
      fi
      case $1 in
        --version) version=$2 ;;
        --target) target=$2 ;;
        --bin-dir) bin_dir=$2 ;;
        --config) config=$2 ;;
      esac
      shift 2
      ;;
    --version=* | --target=* | --bin-dir=* | --config=*)
      value=${1#*=}
      if [ -z "$value" ]; then
        usage_error "${1%%=*} needs a value"
      fi
      case $1 in
        --version=*) version=$value ;;
        --target=*) target=$value ;;
        --bin-dir=*) bin_dir=$value ;;
        --config=*) config=$value ;;
      esac
      shift
      ;;
    --no-attestation)
      attestation=0
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage_error "unknown option: $1"
      ;;
  esac
done

if [ -z "$bin_dir" ] || [ -z "$config" ]; then
  if [ -z "${HOME:-}" ]; then
    die "HOME isn't set: pass --bin-dir and --config"
  fi
fi
if [ -z "$bin_dir" ]; then
  bin_dir=$HOME/.local/bin
fi
if [ -z "$config" ]; then
  # Where open-ferry init writes by default: XDG_CONFIG_HOME counts only
  # when it's an absolute path, as the XDG spec says.
  case ${XDG_CONFIG_HOME:-} in
    /*) config=$XDG_CONFIG_HOME/open-ferry/config.yaml ;;
    *) config=$HOME/.config/open-ferry/config.yaml ;;
  esac
fi
# Absolute, as they're printed in commands to run from anywhere.
case $bin_dir in
  /*) ;;
  *) bin_dir=$(pwd)/$bin_dir ;;
esac
case $config in
  /*) ;;
  *) config=$(pwd)/$config ;;
esac

# A version is MAJOR.MINOR.PATCH with an optional pre-release part, as the
# release workflow requires; a leading v is allowed.
valid_version() {
  printf '%s\n' "$1" | grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?'
}

if [ -n "$version" ]; then
  version=${version#v}
  valid_version "$version" || usage_error "not a version: $version (such as 0.1.0)"
fi

# --- The target --------------------------------------------------------------

# Whether version $1 is at least $2, comparing up to three numeric parts.
version_at_least() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    split(a, x, "."); split(b, y, ".")
    for (i = 1; i <= 3; i++) {
      if (x[i] + 0 > y[i] + 0) exit 0
      if (x[i] + 0 < y[i] + 0) exit 1
    }
    exit 0
  }'
}

target_note=
detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case $arch in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "there's no open-ferry build for $arch processors; build it from source" ;;
  esac
  case $os in
    Linux)
      # glibc reports its version through getconf; musl doesn't. The glibc
      # builds need glibc $GLIBC_FLOOR or newer; the musl builds are static
      # and run on any Linux.
      libc_version=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
      case $libc_version in
        "glibc "*)
          libc_version=${libc_version#glibc }
          if version_at_least "$libc_version" "$GLIBC_FLOOR"; then
            target=$arch-unknown-linux-gnu
          else
            target=$arch-unknown-linux-musl
            target_note="This system's glibc, $libc_version, is older than $GLIBC_FLOOR, so this is the static (musl) build."
          fi
          ;;
        *)
          target=$arch-unknown-linux-musl
          target_note="This system has no glibc, so this is the static (musl) build."
          ;;
      esac
      ;;
    Darwin)
      # A shell running under Rosetta reports x86_64 on Apple silicon.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
        target_note="This shell runs under Rosetta, so this is the Apple silicon build."
      fi
      target=$arch-apple-darwin
      ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT)
      die "on Windows, use install.ps1: irm https://github.com/$REPO/releases/latest/download/install.ps1 | iex"
      ;;
    *)
      die "there's no open-ferry build for $os; build it from source"
      ;;
  esac
}

if [ -z "$target" ]; then
  detect_target
fi
case $target in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | \
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl | \
    x86_64-apple-darwin | aarch64-apple-darwin) ;;
  *-windows-*) usage_error "use install.ps1 for $target" ;;
  *) usage_error "there's no open-ferry build for $target" ;;
esac

# --- Tools -------------------------------------------------------------------

base_url=${OPEN_FERRY_INSTALL_BASE_URL:-https://github.com/$REPO/releases}
base_url=${base_url%/}

download() {
  if has curl; then
    case $1 in
      # Never follow a redirect off HTTPS.
      https://*) curl --fail --silent --show-error --location --retry 3 --proto =https --proto-redir =https --output "$2" "$1" ;;
      *) curl --fail --silent --show-error --location --retry 3 --output "$2" "$1" ;;
    esac
  elif has wget; then
    wget -q -O "$2" "$1"
  else
    die "downloading needs curl or wget; install one of them"
  fi
}

sha256_of() {
  if has sha256sum; then
    sha256sum "$1" | awk '{ print tolower($1) }'
  elif has shasum; then
    shasum -a 256 "$1" | awk '{ print tolower($1) }'
  elif has openssl; then
    openssl dgst -sha256 -r "$1" | awk '{ print tolower($1) }'
  else
    die "checking the download needs sha256sum, shasum or openssl"
  fi
}

has tar || die "unpacking the download needs tar"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t open-ferry-install)
[ -n "$tmp" ] && [ -d "$tmp" ] || die "couldn't make a temporary directory"
cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# --- The release -------------------------------------------------------------

# SHA256SUMS lines are "<hash>  <name>", or "<hash> *<name>" in binary mode.
sums_hash() {
  awk -v want="$2" '{ name = $2; sub(/^\*/, "", name); if (name == want) { print tolower($1); exit } }' "$1"
}

suffix=-$target.tar.gz
if [ -z "$version" ]; then
  say "Finding the latest release..."
  download "$base_url/latest/download/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "couldn't download the latest release's SHA256SUMS from $base_url"
  # The archive's name carries the version: open-ferry-<version>-<target>.tar.gz.
  archive=$(awk -v suffix="$suffix" '{
    name = $2; sub(/^\*/, "", name)
    n = length(name) - length(suffix)
    if (index(name, "open-ferry-") == 1 && n > 11 && substr(name, n + 1) == suffix) { print name; exit }
  }' "$tmp/SHA256SUMS")
  [ -n "$archive" ] || die "the latest release has no archive for $target"
  version=${archive#open-ferry-}
  version=${version%"$suffix"}
  valid_version "$version" || die "the latest release's archive has an unexpected name: $archive"
else
  download "$base_url/download/v$version/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "couldn't download SHA256SUMS for open-ferry $version: is there a release v$version?"
fi
archive=open-ferry-$version$suffix
expected=$(sums_hash "$tmp/SHA256SUMS" "$archive")
printf '%s\n' "$expected" | grep -Eqx '[0-9a-f]{64}' ||
  die "open-ferry $version's SHA256SUMS lists no archive for $target"

say "Downloading open-ferry $version for $target..."
if [ -n "$target_note" ]; then
  say "$target_note"
fi
download "$base_url/download/v$version/$archive" "$tmp/$archive" ||
  die "couldn't download $archive"

actual=$(sha256_of "$tmp/$archive")
if [ "$actual" != "$expected" ]; then
  die "$archive doesn't match SHA256SUMS (expected $expected, got $actual): not installing it"
fi
say "Checked $archive against SHA256SUMS."

gh=${OPEN_FERRY_INSTALL_GH:-gh}
if [ "$attestation" = 0 ]; then
  say "Not checking the build provenance attestation (--no-attestation)."
elif has "$gh"; then
  say "Checking the build provenance attestation with gh..."
  if ! "$gh" attestation verify "$tmp/$archive" --repo "$REPO"; then
    die "gh couldn't verify $archive's attestation: not installing it. If gh isn't signed in, run 'gh auth login' and try again, or pass --no-attestation to rely on the SHA256SUMS check alone."
  fi
else
  say "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed. The SHA256SUMS check passed."
fi

# --- Install -----------------------------------------------------------------

(cd "$tmp" && tar -xzf "$archive") || die "couldn't unpack $archive"
binary=$tmp/open-ferry-$version-$target/open-ferry
[ -f "$binary" ] || die "$archive holds no open-ferry binary"

mkdir -p "$bin_dir" || die "couldn't make $bin_dir"
# Copied next to the old binary, then renamed over it in one step, so a
# running open-ferry keeps its file and a failed copy leaves the old one.
staged=$bin_dir/.open-ferry.new.$$
cp "$binary" "$staged" || die "couldn't write to $bin_dir"
chmod 755 "$staged"
if ! mv -f "$staged" "$bin_dir/open-ferry"; then
  rm -f "$staged"
  die "couldn't install open-ferry in $bin_dir"
fi
installed=$bin_dir/open-ferry
say "Installed open-ferry $version as $installed."

if [ -e "$config" ]; then
  wrote_config=0
  say "Keeping your config at $config."
else
  wrote_config=1
  say "Writing a starting config with open-ferry init..."
  say ""
  "$installed" init -config "$config" || die "open-ferry init couldn't write $config"
  say ""
fi

# --- Next steps --------------------------------------------------------------

command=open-ferry
on_path=0
case ":${PATH:-}:" in
  *":$bin_dir:"* | *":$bin_dir/:"*) on_path=1 ;;
esac
if [ "$on_path" = 0 ]; then
  case $installed in
    *[!A-Za-z0-9_./-]*) command="\"$installed\"" ;;
    *) command=$installed ;;
  esac
fi

say "open-ferry $version is installed."
if [ "$on_path" = 0 ]; then
  say ""
  say "$bin_dir isn't on your PATH. To run open-ferry by name, add this line to your shell's profile (such as ~/.profile, ~/.bashrc or ~/.zshrc), then open a new terminal:"
  say ""
  say "  export PATH=\"$bin_dir:\$PATH\""
else
  found=$(command -v open-ferry 2>/dev/null || true)
  if [ -n "$found" ] && [ "$found" != "$installed" ]; then
    say ""
    say "Note: open-ferry on your PATH is $found, not this one."
  fi
fi
say ""
say "Next steps:"
say "  Start it:            $command -config \"$config\""
say "  Or run it at login:  $command service install -config \"$config\""
if [ "$wrote_config" = 1 ]; then
  say "  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above"
else
  say "  Open the dashboard:  http://127.0.0.1:<port>/dashboard/, with your config's port (8317 by default)"
fi
say "  Check the setup:     $command check -config \"$config\""

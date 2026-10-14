#!/bin/sh
# Installs open-ferry on Linux or macOS from a GitHub release.
#
#   curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh
#
# It downloads the release's archive for this system and checks it against
# the release's SHA256SUMS, and, when the GitHub CLI (gh) is installed,
# against the archive's build provenance attestation. It installs the
# binary as ~/.local/bin/open-ferry. Then it looks for CLIProxyAPI with
# `open-ferry migrate -json`, which changes nothing. When it finds it, it
# asks whether to switch it to open-ferry, which then runs on CLIProxyAPI's
# config and credentials (`open-ferry migrate -yes`; see
# docs/migrating-from-cliproxyapi.md). When it doesn't, and there is no
# config at $XDG_CONFIG_HOME/open-ferry/config.yaml
# (~/.config/open-ferry/config.yaml when XDG_CONFIG_HOME isn't an absolute
# path), it writes one with `open-ferry init`. It edits no shell profile
# and no saved PATH.
# With --system (Linux, as root), the binary goes to /usr/local/bin and
# init -system writes /etc/open-ferry/config.yaml with the machine auth dir.
# It skips migration; account installs must run without sudo, except for
# root installing its own account inside a Linux container.
# Real root ignores GH, BASE_URL, TEST_PRIVILEGED and every FAKE_* setting,
# drops tool configuration settings, runs tools with a clean allow-listed
# environment and a PATH excluding /usr/local. It checks the install directory
# and its ancestors
# for root ownership, no links and no group/other write before downloading.
# Download and unpacking use a checked root-owned directory with mode 0700.
#
# Run `sh install.sh --help` for the options. Environment:
#   OPEN_FERRY_INSTALL_BASE_URL  where releases are downloaded from, in
#                                place of the GitHub repository's releases
#                                URL (for a mirror, or a test server)
#   OPEN_FERRY_INSTALL_GH        the GitHub CLI command (default: gh)
#   OPEN_FERRY_INSTALL_SELF_UPDATE  off, notify or auto: set self-update.mode
#                                in the config (--no-auto-update is off)
#   OPEN_FERRY_INSTALL_MIGRATE   yes: as --migrate; no: as --no-migrate
#   OPEN_FERRY_INSTALL_SYSTEM    yes: as --system; no: an account install
#   OPEN_FERRY_INSTALL_TEST_PRIVILEGED  0 or 1: fake privilege only with a
#                                loopback base URL and FAKE_OPEN_FERRY_INSTALL_LOG
#
# The new binary installs itself (`open-ferry install-self`): it waits for
# any other install or update, refuses to replace a binary a background
# service runs, replaces the old one with a way back, and writes
# install-receipt.json in open-ferry's data directory ($XDG_DATA_HOME/open-ferry,
# or ~/.local/share/open-ferry), which lets the binary update itself; see
# docs/updates.md for turning that off.
#
# See https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy#install
set -eu

REPO=Loft-902-Co-LLC/open-ferry-ai-proxy
GLIBC_FLOOR=2.31

usage() {
  run cat <<'EOF'
Installs open-ferry from a GitHub release.

Usage: install.sh [options]

Options:
  --version VERSION  install this version (such as 0.1.0) rather than the
                     latest release
  --target TARGET    install the build for this target rather than this
                     system's, such as x86_64-unknown-linux-musl
  --system           install for the whole machine on Linux; needs root
  --bin-dir DIR      install the binary in DIR (default: ~/.local/bin;
                     with --system: /usr/local/bin)
  --config PATH      the config to keep, or to write when there is none
                     (default: $XDG_CONFIG_HOME/open-ferry/config.yaml, or
                     ~/.config/open-ferry/config.yaml when XDG_CONFIG_HOME
                     isn't an absolute path; with --system:
                     /etc/open-ferry/config.yaml)
  --no-attestation   don't check the build provenance attestation, even
                     when gh is installed; the SHA256SUMS check still runs
  --no-auto-update   turn automatic updates off (self-update.mode: off in
                     the config); open-ferry update -mode auto turns them on
  --migrate          when CLIProxyAPI is found, switch it to open-ferry
                     without asking, as for an unattended install; exits
                     with 1 when it can't be switched
  --no-migrate       don't look for CLIProxyAPI
  -h, --help         show this help

Environment: OPEN_FERRY_INSTALL_MIGRATE=yes is --migrate, and
OPEN_FERRY_INSTALL_MIGRATE=no is --no-migrate; the options win over it.
OPEN_FERRY_INSTALL_SYSTEM=yes is --system; no is an account install.
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

# Discover privilege in a subshell before any external command sees loader hooks.
caller_path=${PATH:-}
PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH
real_uid=$(
  unset LD_PRELOAD LD_LIBRARY_PATH LD_AUDIT BASH_ENV ENV CDPATH GLOBIGNORE
  IFS=$(printf ' \t\nx')
  IFS=${IFS%x}
  /usr/bin/env -i PATH="$PATH" LC_ALL=C id -u
) || die "couldn't tell whether this process is privileged"

# Every privileged external command shares this allow-list, including children
# tar starts to decompress an archive. Never pass caller HOME to a root tool.
run() (
  if [ "$real_uid" != 0 ]; then
    "$@"
    exit $?
  fi
  for proxy in http_proxy https_proxy HTTPS_PROXY no_proxy NO_PROXY all_proxy ALL_PROXY; do
    present=
    eval "present=\${$proxy+x}"
    if [ "$present" = x ]; then
      eval "value=\${$proxy}"
      set -- "$proxy=$value" "$@"
    fi
  done
  /usr/bin/env -i PATH="$PATH" HOME="$HOME" LC_ALL=C "$@"
)
if [ "$real_uid" = 0 ]; then
  unset LD_PRELOAD LD_LIBRARY_PATH LD_AUDIT BASH_ENV ENV CDPATH GLOBIGNORE
  IFS=$(printf ' \t\nx')
  IFS=${IFS%x}
  # Resolve root's home from the machine account database, not caller HOME.
  HOME=/root
  HOME=$(run awk -F: '$3 == 0 { print $6; exit }' /etc/passwd)
  [ -n "$HOME" ] || die "couldn't find root's home in /etc/passwd"
  export HOME
  # Only names are printed; values may contain credentials or newlines.
  names=$(export -p | run sed -n "s/^export \([A-Za-z_][A-Za-z0-9_]*\).*$/\1/p")
  for name in $names; do
    case $name in
      OPEN_FERRY_INSTALL_SELF_UPDATE | OPEN_FERRY_INSTALL_SYSTEM | OPEN_FERRY_INSTALL_MIGRATE) continue ;;
      OPEN_FERRY_INSTALL_* | FAKE_* | TAR_OPTIONS | CURL_HOME | WGETRC | SSL_CERT_FILE | SSL_CERT_DIR | CURL_CA_BUNDLE | PERL5OPT | PERL5LIB | OPENSSL_CONF | GH_*)
        say "Ignoring $name when running as root."
        unset "$name"
        ;;
    esac
  done
else
  PATH=$caller_path
  export PATH
fi
uid=$real_uid
test_mode=0

# --- Options -----------------------------------------------------------------

version=
target=
bin_dir=
config=
attestation=1
self_update=${OPEN_FERRY_INSTALL_SELF_UPDATE:-}
migrate=
migrate_why=--migrate
system=
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
    --system)
      system=1
      shift
      ;;
    --no-attestation)
      attestation=0
      shift
      ;;
    --no-auto-update)
      self_update=off
      shift
      ;;
    --migrate)
      migrate=yes
      shift
      ;;
    --no-migrate)
      migrate=no
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

case $self_update in
  '' | off | notify | auto) ;;
  *) usage_error "OPEN_FERRY_INSTALL_SELF_UPDATE is \"$self_update\": use off, notify or auto" ;;
esac
if [ -z "$migrate" ]; then
  case ${OPEN_FERRY_INSTALL_MIGRATE:-} in
    '') migrate=ask ;;
    1 | yes | true)
      migrate=yes
      migrate_why=OPEN_FERRY_INSTALL_MIGRATE=$OPEN_FERRY_INSTALL_MIGRATE
      ;;
    0 | no | false) migrate=no ;;
    *) usage_error "OPEN_FERRY_INSTALL_MIGRATE is yes or no, not $OPEN_FERRY_INSTALL_MIGRATE" ;;
  esac
fi

if [ -z "$system" ]; then
  case ${OPEN_FERRY_INSTALL_SYSTEM:-} in
    '' | 0 | no | false) system=0 ;;
    1 | yes | true) system=1 ;;
    *) usage_error "OPEN_FERRY_INSTALL_SYSTEM is yes or no, not $OPEN_FERRY_INSTALL_SYSTEM" ;;
  esac
fi

# Refuse before downloading or creating any directory.
os=$(run uname -s)
in_container=0
if [ -e /.dockerenv ] || [ -e /run/.containerenv ] || [ -e /var/run/secrets/kubernetes.io ]; then
  in_container=1
fi
# Only the fake-release suite's existing loopback and recording seam
# may pretend to be root; a normal run uses the effective uid.
case ${OPEN_FERRY_INSTALL_BASE_URL:-} in
  http://127.0.0.1:*)
    if [ "$real_uid" != 0 ] && [ -n "${FAKE_OPEN_FERRY_INSTALL_LOG:-}" ]; then
      test_mode=1
      case ${FAKE_CONTAINER_MARKER:-} in
        dockerenv | containerenv | kubernetes) in_container=1 ;;
        *) in_container=0 ;;
      esac
      case ${OPEN_FERRY_INSTALL_TEST_PRIVILEGED:-} in
        0) uid=1 ;;
        1) uid=0 ;;
      esac
    fi
    ;;
esac
if [ "$os" = Darwin ] && { [ "$system" = 1 ] || [ "$uid" = 0 ]; }; then
  die "open-ferry can't yet check who may change its files on macOS, so it doesn't use them as root. Run it without sudo; installs for the whole machine come to macOS in a later version."
fi
if [ "$system" = 1 ]; then
  [ "$uid" = 0 ] || die "installing open-ferry for the whole machine needs root. Run it with sudo."
  [ "$os" = Linux ] || die "--system is supported only on Linux."
  [ -n "$bin_dir" ] || bin_dir=/usr/local/bin
  [ -n "$config" ] || config=/etc/open-ferry/config.yaml
  migrate=no
elif [ "$uid" = 0 ]; then
  if [ "$os" = Linux ] && [ "$in_container" = 1 ]; then
    say "Running as root inside a container: installing for root's own account."
  else
    die "an account install must not run as root. Run it without sudo, or install open-ferry for the whole machine with \`install.sh --system\`."
  fi
fi

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

# Walk from / before creating anything, so even an intermediate link is refused.
# In the loopback suite, the test account stands in for root.
check_directory() (
  directory=$1
  [ ! -L "$directory" ] && [ -d "$directory" ] ||
    die "refusing $directory: use a real directory, not a link or file"
  metadata=$(LC_ALL=C run ls -ldn "$directory") || die "couldn't examine $directory"
  read -r mode _links owner _rest <<EOF
$metadata
EOF
  case $owner in
    0) ;;
    *)
      [ "$test_mode" = 1 ] && [ "$owner" = "$real_uid" ] ||
        die "refusing $directory: make it owned by root, or choose a root-owned --bin-dir"
      ;;
  esac
  case $mode in
    d????????*) ;;
    *) die "refusing $directory: couldn't read its directory permissions" ;;
  esac
  case $mode in
    ?????w????* | ????????w?*)
      die "refusing $directory: remove group and other write permission (chmod go-w), or choose a protected --bin-dir"
      ;;
  esac
)

prepare_directory() (
  directory=$1
  if [ "$directory" != / ]; then
    parent=${directory%/*}
    [ -n "$parent" ] || parent=/
    prepare_directory "$parent" || exit 1
  fi
  if [ ! -e "$directory" ] && [ ! -L "$directory" ]; then
    (umask 022 && run mkdir -m 755 "$directory") || die "couldn't make $directory"
  fi
  check_directory "$directory"
)

if [ "$uid" = 0 ]; then
  case $bin_dir/ in
    */../* | */./* | *//*) die "choose an absolute --bin-dir without . or .. components or repeated slashes: $bin_dir" ;;
  esac
  bin_dir=${bin_dir%/}
  [ -n "$bin_dir" ] || bin_dir=/
  prepare_directory "$bin_dir" || exit 1
fi

# A version is MAJOR.MINOR.PATCH with an optional pre-release part, as the
# release workflow requires; a leading v is allowed.
valid_version() {
  printf '%s\n' "$1" | run grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?'
}

if [ -n "$version" ]; then
  version=${version#v}
  valid_version "$version" || usage_error "not a version: $version (such as 0.1.0)"
fi

# --- The target --------------------------------------------------------------

# Whether version $1 is at least $2, comparing up to three numeric parts.
version_at_least() {
  run awk -v a="$1" -v b="$2" 'BEGIN {
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
  os=$(run uname -s)
  arch=$(run uname -m)
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
      libc_version=$(run getconf GNU_LIBC_VERSION 2>/dev/null || true)
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
      if [ "$arch" = x86_64 ] && [ "$(run sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
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

run_curl() {
  if [ "$real_uid" = 0 ]; then run curl -q "$@"; else run curl "$@"; fi
}

download() {
  if has curl; then
    case $1 in
      # Never follow a redirect off HTTPS.
      https://*) run_curl --fail --silent --show-error --location --retry 3 --proto =https --proto-redir =https --output "$2" "$1" ;;
      *) run_curl --fail --silent --show-error --location --retry 3 --output "$2" "$1" ;;
    esac
  elif has wget; then
    if [ "$real_uid" = 0 ] && run wget --help 2>&1 | run grep -q -- --no-config; then
      run wget --no-config -q -O "$2" "$1"
    else
      run wget -q -O "$2" "$1"
    fi
  else
    die "downloading needs curl or wget; install one of them"
  fi
}

sha256_of() {
  if has sha256sum; then
    run sha256sum "$1" | run awk '{ print tolower($1) }'
  elif has shasum; then
    run shasum -a 256 "$1" | run awk '{ print tolower($1) }'
  elif has openssl; then
    run openssl dgst -sha256 -r "$1" | run awk '{ print tolower($1) }'
  else
    die "checking the download needs sha256sum, shasum or openssl"
  fi
}

has tar || die "unpacking the download needs tar"

if [ "$uid" = 0 ]; then
  tmp=$(umask 077 && run mktemp -d "$bin_dir/.open-ferry-install.XXXXXXXX")
else
  tmp=$(run mktemp -d 2>/dev/null || run mktemp -d -t open-ferry-install)
fi
[ -n "$tmp" ] && [ -d "$tmp" ] || die "couldn't make a temporary directory"
cleanup() {
  run rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if [ "$uid" = 0 ]; then
  run chmod 700 "$tmp" || die "couldn't protect $tmp"
  check_directory "$tmp" || exit 1
fi

# --- The release -------------------------------------------------------------

# SHA256SUMS lines are "<hash>  <name>", or "<hash> *<name>" in binary mode.
sums_hash() {
  run awk -v want="$2" '{ name = $2; sub(/^\*/, "", name); if (name == want) { print tolower($1); exit } }' "$1"
}

suffix=-$target.tar.gz
if [ -z "$version" ]; then
  say "Finding the latest release..."
  download "$base_url/latest/download/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "couldn't download the latest release's SHA256SUMS from $base_url"
  # The archive's name carries the version: open-ferry-<version>-<target>.tar.gz.
  archive=$(run awk -v suffix="$suffix" '{
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
printf '%s\n' "$expected" | run grep -Eqx '[0-9a-f]{64}' ||
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
  if ! run "$gh" attestation verify "$tmp/$archive" --repo "$REPO"; then
    die "gh couldn't verify $archive's attestation: not installing it. If gh isn't signed in, run 'gh auth login' and try again, or pass --no-attestation to rely on the SHA256SUMS check alone."
  fi
else
  say "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed. The SHA256SUMS check passed."
fi

# --- Install -----------------------------------------------------------------

(cd "$tmp" && run tar -xzf "$archive") || die "couldn't unpack $archive"
binary=$tmp/open-ferry-$version-$target/open-ferry
[ -f "$binary" ] || die "$archive holds no open-ferry binary"

run mkdir -p "$bin_dir" || die "couldn't make $bin_dir"
# Copied next to the old binary, then the new binary moves itself into place
# (`install-self`): a running open-ferry keeps its file, and a failed install
# leaves the old one. The staged file has a name of its own, so two installs
# never share it.
staged=$(run mktemp "$bin_dir/.open-ferry.$$.XXXXXXXX") || die "couldn't write to $bin_dir"
installed=$bin_dir/open-ferry
if ! run cp "$binary" "$staged" || ! run chmod 755 "$staged"; then
  run rm -f "$staged"
  die "couldn't write to $bin_dir"
fi
status=0
# `install-self` is in every release (0.1.0 has it, and no release is older),
# so a version this script can name has it: no fallback, no probe.
set -- install-self -from "$staged" -to "$installed" -installer install.sh
if [ "$system" = 1 ]; then set -- "$@" -system; fi
(cd / && run "$staged" "$@" </dev/null) || status=$?
case $status in
  0) ;;
  3)
    run rm -f "$staged"
    die "open-ferry runs as a background service: install-self said how to stop it, above. Nothing was changed."
    ;;
  4)
    run rm -f "$staged"
    die "couldn't tell whether open-ferry is running from $installed: see above. Nothing was changed."
    ;;
  5)
    run rm -f "$staged"
    die "gave up waiting for another open-ferry install or update to finish. Nothing was changed; run the installer again."
    ;;
  6)
    # The new binary and the files it names may be all that is left: keep them.
    die "the install didn't finish and open-ferry couldn't put things back: see above for the files to restore by hand."
    ;;
  *)
    run rm -f "$staged"
    die "couldn't install open-ferry in $bin_dir: see above."
    ;;
esac
say "Installed open-ferry $version as $installed."

# How to run open-ferry in the commands printed from here on.
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

# --- CLIProxyAPI -------------------------------------------------------------

# The string under the key $1 in $search, the one line of JSON that
# `open-ferry migrate -json` prints, or nothing when it's null or missing.
# The keys read here appear once, and a string's own quotes are escaped,
# so a key can't be found inside a string.
json_string() {
  printf '%s\n' "$search" |
    run sed -n -E 's/^.*"'"$1"'":"(([^"\\]|\\.)*)".*$/\1/p' |
    run sed -e 's/\\"/"/g' -e 's/\\\\/\\/g'
}

# Whether someone at a terminal can answer: the output is a terminal and
# /dev/tty opens. (Under `curl | sh`, the standard input is the script.)
is_terminal() {
  [ -t 1 ] && (: </dev/tty) 2>/dev/null
}

# `open-ferry migrate -json` only looks, and changes nothing. It answers
# {"found":false} when there is no CLIProxyAPI.
found_cpa=0
switched=0
switch_failed=
if [ "$system" = 1 ]; then
  say "Skipping the migrate offer for a machine install. To migrate CLIProxyAPI, run: sudo open-ferry migrate"
fi
if [ "$migrate" != no ]; then
  search=$(cd / && run "$installed" migrate -json </dev/null 2>/dev/null || true)
  case $search in
    '{"found":true,'*) found_cpa=1 ;;
    '{"found":false}')
      if [ "$migrate" = yes ]; then
        say "CLIProxyAPI wasn't found, so there is nothing to switch ($migrate_why)."
      fi
      ;;
    *)
      error=$(json_string error)
      say "open-ferry couldn't look for CLIProxyAPI${error:+: $error}."
      if [ "$migrate" = yes ]; then
        switch_failed="$migrate_why was given, but open-ferry couldn't look for CLIProxyAPI. To look again: $command migrate -dry-run"
      fi
      ;;
  esac
fi
if [ "$found_cpa" = 1 ]; then
  cpa_config=$(json_string config)
  cpa_host=$(json_string host)
  case $cpa_host in
    '' | 0.0.0.0 | ::) cpa_host=127.0.0.1 ;;
    *:*) cpa_host="[$cpa_host]" ;;
  esac
  cpa_port=$(printf '%s\n' "$search" | run sed -n -E 's/^.*"port":([0-9]+).*$/\1/p')
  cpa_scheme=http
  case $search in
    *'"tls":true'*) cpa_scheme=https ;;
  esac
  say ""
  say "Found $(json_string summary)."
  switch_now=0
  case $search in
    *'"can_switch":true'*)
      say "open-ferry can take its place, on its config and credentials. To see how first, run: $command migrate -dry-run"
      if [ "$migrate" = yes ]; then
        say "Switching to open-ferry ($migrate_why)..."
        switch_now=1
      elif is_terminal; then
        printf '%s' "Switch to open-ferry now? [y/N] "
        answer=
        read -r answer </dev/tty || answer=
        case $answer in
          y | Y | yes | Yes | YES) switch_now=1 ;;
          *) say "Not switching." ;;
        esac
      else
        say "There is no one at a terminal to ask, so it isn't switched. To switch to open-ferry, run: $command migrate"
      fi
      ;;
    *)
      say "open-ferry can't switch it as things are. To see why, and what to do, run: $command migrate -dry-run"
      if [ "$migrate" = yes ]; then
        switch_failed="$migrate_why was given, but CLIProxyAPI can't be switched as things are. To see why: $command migrate -dry-run"
      fi
      ;;
  esac
  if [ "$switch_now" = 1 ]; then
    say ""
    if (cd / && run "$installed" migrate -yes </dev/null); then
      switched=1
    else
      switch_failed="the switch from CLIProxyAPI didn't finish: see what open-ferry migrate said above"
    fi
  fi
  say ""
fi

# --- The config --------------------------------------------------------------

wrote_config=0
skipped_init=0
if [ -e "$config" ]; then
  say "Keeping your config at $config."
elif [ "$found_cpa" = 1 ]; then
  # A switch runs open-ferry on CLIProxyAPI's config: a starting config
  # here would compete with it, on the same port.
  skipped_init=1
  if [ "$switched" = 1 ]; then
    say "Not writing a starting config: open-ferry runs on CLIProxyAPI's."
  else
    say "Not writing a starting config: switching keeps CLIProxyAPI's."
  fi
else
  wrote_config=1
  say "Writing a starting config with open-ferry init..."
  say ""
  set -- init -config "$config"
  if [ "$system" = 1 ]; then set -- init -system -config "$config"; fi
  (cd / && run "$installed" "$@") || die "open-ferry init couldn't write $config"
  say ""
fi

# The config the update mode goes in: CLIProxyAPI's once open-ferry runs on
# it, else open-ferry's own, unless there is none yet. The server's first
# update check comes minutes after it starts, and it follows a change to its
# config at once.
mode_config=$config
if [ "$switched" = 1 ]; then
  mode_config=$cpa_config
  case $mode_config in
    /*) ;;
    *) die "CLIProxyAPI returned a config path that isn't absolute: $mode_config" ;;
  esac
elif [ "$skipped_init" = 1 ]; then
  mode_config=
fi
if [ -n "$self_update" ]; then
  if [ -n "$mode_config" ]; then
    (cd / && run "$installed" update -mode "$self_update" -config "$mode_config") ||
      die "open-ferry update couldn't set self-update.mode to $self_update in $mode_config"
  else
    say "There's no config yet to set self-update.mode to $self_update in. Once open-ferry runs on one, run: $command update -mode $self_update -config <config>"
  fi
fi

# --- Next steps --------------------------------------------------------------

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
if [ "$switched" = 1 ]; then
  if [ -n "$cpa_config" ]; then
    say "  Check the setup:     $command check -config \"$cpa_config\""
  fi
  if [ -n "$cpa_port" ]; then
    say "  Open the dashboard:  $cpa_scheme://$cpa_host:$cpa_port/dashboard/ and sign in with CLIProxyAPI's management key"
  fi
  say "  Switch back:         $command migrate -undo"
else
  if [ "$found_cpa" = 1 ]; then
    say "  Switch over:         $command migrate"
  fi
  if [ "$skipped_init" = 0 ]; then
    start_command=$command
    if [ "$system" = 1 ]; then start_command="sudo $command"; fi
    say "  Start it:            $start_command -config \"$config\""
    if [ "$system" = 1 ]; then
      say "  Run it at boot:      $start_command service install -system -config \"$config\""
    else
      say "  Or run it at login:  $command service install -config \"$config\""
    fi
    if [ "$wrote_config" = 1 ]; then
      say "  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above"
    else
      say "  Open the dashboard:  http://127.0.0.1:<port>/dashboard/, with your config's port (8317 by default)"
    fi
    say "  Check the setup:     $command check -config \"$config\""
  else
    say "  Or start afresh:     $command init -config \"$config\""
  fi
fi
if [ -n "$mode_config" ]; then
  say ""
  off="To turn that off: $command update -mode off -config \"$mode_config\""
  case $self_update:$wrote_config in
    off:*) say "Automatic updates are off. To turn them on: $command update -mode auto -config \"$mode_config\"" ;;
    notify:*) say "open-ferry says when a release is out, but doesn't install it. $off" ;;
    auto:* | *:1) say "open-ferry keeps itself up to date. $off" ;;
    *) say "open-ferry keeps itself up to date, unless your config says otherwise. $off" ;;
  esac
fi
if [ -n "$switch_failed" ]; then
  die "$switch_failed"
fi

#!/usr/bin/env bash
# Runs tools/bench on an Ubuntu 24.04 machine made for it, such as a cloud VM:
# installs the build tools, Rust (the version rust-toolchain.toml names) and
# Go 1.26.4, clones open-ferry and CLIProxyAPI, builds both as
# docs/benchmarks.md says, waits for the machine to be quiet, and runs the
# benchmark. It leaves the updated docs/benchmarks.md, the results and a log
# in <dir>/results/<time>/.
#
# Run it by hand, as root or as a user with sudo, on a machine running
# nothing else; CI never runs it. Running it again is safe: it installs only
# what's missing, fetches the refs again, and builds again only what changed.
#
#   bash run-ubuntu.sh --note "Azure Standard_D8as_v5, East US"
#
# See docs/benchmarks.md, "Running it", and `bash run-ubuntu.sh --help`.
set -euo pipefail

GO_VERSION=1.26.4
# The SHA-256 of go.dev's archives of Go 1.26.4 for Linux.
GO_SHA256_AMD64=1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f
GO_SHA256_ARM64=ef758ae7c6cf9267c9c0ef080b8965f453d89ab2d25d9eb22de4405925238768

usage() {
  cat <<'EOF'
Runs open-ferry's benchmark against CLIProxyAPI on Ubuntu 24.04.

Usage: run-ubuntu.sh --note <text> [options] [-- <benchmark options>]

Options:
  --note <text>              what the machine is, for the report: its VM size
                             and region, say (required)
  --ref <ref>                open-ferry's branch, tag or commit (default: main)
  --tag <tag>                CLIProxyAPI's tag (default: v8.0.20)
  --dir <dir>                where everything goes (default: ~/ofp-bench)
  --repo <url>               open-ferry's repository (default: its GitHub one)
  --cliproxyapi-repo <url>   CLIProxyAPI's repository (default: its GitHub one)
  --max-load <n>             the 1-minute load average to wait for before
                             measuring (default: 0.5)
  --wait <minutes>           how long to wait for it before giving up
                             (default: 30)
  -h, --help                 show this

Options after -- go to the benchmark, such as `-- --duration 2` for a quick
run. See docs/benchmarks.md for them.
EOF
}

note=""
ref="main"
tag="v8.0.20"
dir="$HOME/ofp-bench"
repo="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy.git"
cliproxyapi_repo="https://github.com/router-for-me/CLIProxyAPI.git"
max_load="0.5"
wait_minutes=30
bench_args=()

need() {
  if [ $# -lt 2 ] || [ -z "$2" ]; then
    echo "$1 needs a value" >&2
    exit 2
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --note) need "$@"; note="$2"; shift 2 ;;
    --ref) need "$@"; ref="$2"; shift 2 ;;
    --tag) need "$@"; tag="$2"; shift 2 ;;
    --dir) need "$@"; dir="$2"; shift 2 ;;
    --repo) need "$@"; repo="$2"; shift 2 ;;
    --cliproxyapi-repo) need "$@"; cliproxyapi_repo="$2"; shift 2 ;;
    --max-load) need "$@"; max_load="$2"; shift 2 ;;
    --wait) need "$@"; wait_minutes="$2"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    --) shift; bench_args=("$@"); break ;;
    *) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
  esac
done
if [ -z "$note" ]; then
  echo "--note is required: say what the machine is, such as its VM size and region" >&2
  exit 2
fi
case "$max_load" in
  '' | *[!0-9.]* | *.*.*) echo "--max-load takes a number, not $max_load" >&2; exit 2 ;;
esac
case "$wait_minutes" in
  '' | *[!0-9]*) echo "--wait takes whole minutes, not $wait_minutes" >&2; exit 2 ;;
esac

mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
results="$dir/results/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$results"
exec > >(tee -a "$results/run.log") 2>&1
echo "== $(date -u '+%Y-%m-%d %H:%M:%S UTC'): benchmark run in $dir"
echo "Machine: $note"
uname -a
if [ -r /etc/os-release ]; then
  . /etc/os-release
  echo "OS: ${PRETTY_NAME:-unknown}"
  if [ "${ID:-}" != ubuntu ] || [ "${VERSION_ID:-}" != 24.04 ]; then
    echo "warning: this script is for Ubuntu 24.04; carrying on anyway"
  fi
fi
echo "CPUs: $(nproc); load average: $(cut -d' ' -f1-3 /proc/loadavg)"
grep -m1 'model name' /proc/cpuinfo || true
grep MemTotal /proc/meminfo || true
echo "Ports for outgoing connections: $(cat /proc/sys/net/ipv4/ip_local_port_range)"

as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  else
    sudo "$@"
  fi
}

echo "== Build tools"
packages=(build-essential pkg-config cmake git curl ca-certificates)
missing=()
for package in "${packages[@]}"; do
  if ! dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed'; then
    missing+=("$package")
  fi
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "Installing ${missing[*]}"
  as_root env DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=600 update -q
  as_root env DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=600 install -y -q \
    --no-install-recommends "${missing[@]}"
else
  echo "All installed"
fi

echo "== Rust"
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$dir/rustup-init.sh"
  sh "$dir/rustup-init.sh" -y -q --profile minimal --default-toolchain none --no-modify-path
  rm -f "$dir/rustup-init.sh"
fi
rustup --version

echo "== Go $GO_VERSION"
case "$(uname -m)" in
  x86_64 | amd64) go_arch=amd64; go_sha256=$GO_SHA256_AMD64 ;;
  aarch64 | arm64) go_arch=arm64; go_sha256=$GO_SHA256_ARM64 ;;
  *) echo "no Go $GO_VERSION download for $(uname -m)" >&2; exit 1 ;;
esac
go_root="$dir/sdk/go$GO_VERSION"
go="$go_root/bin/go"
if [ ! -x "$go" ] || [ "$(GOTOOLCHAIN=local "$go" env GOVERSION 2>/dev/null)" != "go$GO_VERSION" ]; then
  mkdir -p "$dir/sdk"
  archive="$dir/sdk/go$GO_VERSION.linux-$go_arch.tar.gz"
  curl --proto '=https' --tlsv1.2 -fsSL "https://go.dev/dl/go$GO_VERSION.linux-$go_arch.tar.gz" \
    -o "$archive"
  echo "$go_sha256  $archive" | sha256sum -c -
  rm -rf "$go_root" "$dir/sdk/go"
  tar -C "$dir/sdk" -xzf "$archive"
  mv "$dir/sdk/go" "$go_root"
  rm -f "$archive"
fi
GOTOOLCHAIN=local "$go" version

# fetch <repository> <ref> <dir>: makes <dir> a checkout of <ref>, with
# nothing changed in it.
fetch() {
  local url=$1 want=$2 into=$3
  if [ ! -d "$into/.git" ]; then
    git init -q "$into"
  fi
  git -C "$into" fetch -q --depth 1 --force "$url" "$want"
  git -C "$into" checkout -q --force --detach FETCH_HEAD
  git -C "$into" clean -q -f -d
}

echo "== open-ferry at $ref"
src="$dir/open-ferry"
fetch "$repo" "$ref" "$src"
git -C "$src" log -1 --format='%H %s'

echo "== CLIProxyAPI at $tag"
upstream="$dir/CLIProxyAPI"
# The tag itself, so that the benchmark can name it.
fetch "$cliproxyapi_repo" "refs/tags/$tag:refs/tags/$tag" "$upstream"
git -C "$upstream" describe --tags --always --dirty

echo "== Building"
cd "$src"
# The toolchain rust-toolchain.toml names.
rustup toolchain install
rustc -V
cargo build --release --locked -p open-ferry -p open-ferry-bench
bench="$src/target/release/open-ferry-bench"
"$bench" --upstream "$upstream" --go "$go" --prepare

echo "== Waiting for a 1-minute load average of $max_load or less"
deadline=$(($(date +%s) + wait_minutes * 60))
while :; do
  load=$(cut -d' ' -f1 /proc/loadavg)
  # gawk, the awk of Azure's Ubuntu images, refuses `load` as a variable name.
  if awk -v now="$load" -v max="$max_load" 'BEGIN { exit !(now <= max) }'; then
    echo "The load average is $load"
    break
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "The load average is still $load after $wait_minutes minutes: giving up" >&2
    exit 1
  fi
  echo "The load average is $load; waiting"
  sleep 15
done

echo "== Running the benchmark"
"$bench" --upstream "$upstream" --go "$go" --machine-note "$note" \
  --out "$src/docs/benchmarks.md" ${bench_args[@]+"${bench_args[@]}"} | tee "$results/results.md"
cp "$src/docs/benchmarks.md" "$results/benchmarks.md"
echo "== Done: $results holds benchmarks.md (the updated docs/benchmarks.md), results.md and run.log"

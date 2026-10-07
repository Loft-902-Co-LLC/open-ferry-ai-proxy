#!/bin/sh
# Smoke-tests open-ferry's container image (docker/Dockerfile) for the
# platform it was loaded for.
#
# Usage: sh .github/scripts/test-image.sh IMAGE
#
# Needs docker and curl. It checks:
# - the image's layout: the binary, upstream's paths, the template, the
#   licenses, the zone database and the CA bundle;
# - a server started with a config mounted at /CLIProxyAPI/config.yaml, an
#   auth directory at /root/.cli-proxy-api holding a dummy Codex credential,
#   and a logs directory at /CLIProxyAPI/logs: it answers on 8317, serves
#   the credential's models to a client key, lists the credential through
#   the management API, and writes its log to the logs directory. It runs on
#   an internal Docker network, which reaches nothing outside, and is asked
#   from a second container on that network;
# - a server with its port published on the host's 127.0.0.1: it answers
#   there, and turns away management requests while management.allow-remote
#   is false, as they come through Docker's gateway, not the loopback.
#
# The credential is a dummy: an access token of no account, and no refresh
# token. Nothing is sent to any provider.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: test-image.sh IMAGE" >&2
  exit 2
fi
image=$1
name=open-ferry-image-test-$$
work=$(mktemp -d)
cleanup() {
  docker rm -f "$name-internal" "$name-client" "$name-published" >/dev/null 2>&1 || true
  docker network rm "$name" >/dev/null 2>&1 || true
  # The servers ran as root, so what they wrote is root's.
  docker run --rm --network none -v "$work:/work" "$image" sh -c 'rm -rf /work/internal /work/published' >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
  echo "::error::$*"
  for container in "$name-internal" "$name-published"; do
    if docker inspect "$container" >/dev/null 2>&1; then
      echo "--- docker logs $container"
      docker logs "$container" 2>&1 | tail -n 40 || true
    fi
  done
  if [ -f "$work/internal/logs/main.log" ]; then
    echo "--- $name-internal's main.log"
    tail -n 40 "$work/internal/logs/main.log" || true
  fi
  exit 1
}

pass() {
  echo "ok: $*"
}

# The config the servers run with: the template's empty host, so the server
# listens on every interface of the container.
write_config() { # DIR ALLOW_REMOTE LOGGING_TO_FILE
  mkdir -p "$1/auths"
  cat > "$1/config.yaml" <<EOF
config-version: 8
server:
  host: ""
  port: 8317
management:
  allow-remote: $2
  secret-key: "test-management-key"
access:
  api-keys:
    - "test-client-key"
oauth:
  auth-dir: "~/.cli-proxy-api"
observability:
  logs:
    logging-to-file: $3
EOF
}

# --- The layout --------------------------------------------------------------

docker run --rm --network none "$image" sh -eu -c '
  test "$(pwd)" = /CLIProxyAPI
  test "$(command -v open-ferry)" = /usr/local/bin/open-ferry
  open-ferry -h >/dev/null 2>&1
  /CLIProxyAPI/CLIProxyAPI -h >/dev/null 2>&1
  test -s /CLIProxyAPI/config.example.yaml
  test "$(stat -c %u:%g /usr/local/bin/open-ferry /CLIProxyAPI/config.example.yaml | sort -u)" = 0:0
  test ! -e /CLIProxyAPI/config.yaml
  test ! -e /CLIProxyAPI/logs
  test -s /usr/share/doc/open-ferry/LICENSE
  test -s /usr/share/doc/open-ferry/licenses/rust-third-party-licenses.txt
  test -s /usr/share/doc/open-ferry/licenses/dashboard-third-party-licenses.txt
  test -s /etc/ssl/certs/ca-certificates.crt
  test "$(date +%Z)" = UTC
  case $(TZ=America/New_York date +%Z) in EST | EDT) ;; *) exit 1 ;; esac
' || fail "the image's layout isn't as expected"
pass "the layout: /usr/local/bin/open-ferry, /CLIProxyAPI, the template, the licenses, UTC with TZ working, the CA bundle"

# --- A server on an internal network -----------------------------------------

internal=$work/internal
write_config "$internal" true true
mkdir -p "$internal/logs"
cat > "$internal/auths/codex-dummy.json" <<'EOF'
{"type":"codex","email":"dummy@example.com","access_token":"dummy-not-a-token"}
EOF

docker network create --internal "$name" >/dev/null
docker run -d --name "$name-internal" --network "$name" --network-alias open-ferry \
  -v "$internal/config.yaml:/CLIProxyAPI/config.yaml:ro" \
  -v "$internal/auths:/root/.cli-proxy-api" \
  -v "$internal/logs:/CLIProxyAPI/logs" \
  "$image" >/dev/null
docker run -d --name "$name-client" --network "$name" "$image" sleep 600 >/dev/null

# Prints the HTTP status of a GET of $1 from the client container, with the
# header $2 if given, and leaves the body in the client's /tmp/body.
get() {
  docker exec "$name-client" sh -c '
    if [ -n "$2" ]; then set -- "$1" --header "$2"; else set -- "$1"; fi
    wget -S -O /tmp/body "$@" 2>&1 | grep -o "HTTP/1\.[01] [0-9][0-9][0-9]" | tail -n 1 | cut -d" " -f2
  ' sh "http://open-ferry:8317$1" "${2:-}"
}
body() {
  docker exec "$name-client" cat /tmp/body
}

i=0
until [ "$(get /healthz)" = 200 ]; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    fail "the server didn't answer /healthz on 8317 within 30 seconds"
  fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$name-internal")" != true ]; then
    fail "the server stopped"
  fi
  sleep 0.5
done
pass "the server answers /healthz on 8317"

status=$(get /v1/models)
[ "$status" = 401 ] || fail "/v1/models without a client key answered $status, not 401"
status=$(get /v1/models "Authorization: Bearer test-client-key")
[ "$status" = 200 ] || fail "/v1/models with the client key answered $status, not 200"
body | grep -q '"id":"gpt-' || fail "/v1/models lists no Codex model, so the credential in /root/.cli-proxy-api wasn't read: $(body | head -c 300)"
pass "/v1/models serves the models of the credential in /root/.cli-proxy-api"

status=$(get /v0/management/auth-files "Authorization: Bearer test-management-key")
[ "$status" = 200 ] || fail "the management API's auth-files answered $status, not 200"
body | grep -q 'codex-dummy\.json' || fail "the management API doesn't list codex-dummy.json: $(body | head -c 300)"
pass "the management API lists the credential in /root/.cli-proxy-api"

[ -s "$internal/logs/main.log" ] || fail "the server wrote no main.log to /CLIProxyAPI/logs"
pass "the server logs to /CLIProxyAPI/logs"

docker rm -f "$name-internal" "$name-client" >/dev/null

# --- A server with its port published on the host's loopback -----------------

published=$work/published
write_config "$published" false false
docker run -d --name "$name-published" -p 127.0.0.1::8317 \
  -v "$published/config.yaml:/CLIProxyAPI/config.yaml:ro" \
  -v "$published/auths:/root/.cli-proxy-api" \
  "$image" >/dev/null
port=$(docker port "$name-published" 8317/tcp | sed -n 's/^127\.0\.0\.1://p' | head -n 1)
[ -n "$port" ] || fail "docker published no port for 8317 on 127.0.0.1"
base=http://127.0.0.1:$port

i=0
until curl -fsS -o /dev/null "$base/healthz" 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    fail "the server didn't answer on 127.0.0.1:$port within 30 seconds"
  fi
  sleep 0.5
done
pass "the server answers on the published 127.0.0.1:$port"

status=$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer test-client-key" "$base/v1/models")
[ "$status" = 200 ] || fail "/v1/models on the published port answered $status, not 200"
status=$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer test-management-key" "$base/v0/management/auth-files")
[ "$status" = 403 ] || fail "the management API on the published port answered $status, not 403, with allow-remote false"
pass "with allow-remote false, the management API turns away the host (403): requests arrive through Docker's gateway"

echo "The image passed."

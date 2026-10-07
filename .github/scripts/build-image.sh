#!/bin/sh
# Builds open-ferry's container image, docker/Dockerfile, with buildx.
#
# Usage, from the repository root:
#
#   VERSION=0.1.0 sh .github/scripts/build-image.sh CONTEXT PLATFORMS [OPTION...]
#
# CONTEXT holds SHA256SUMS and the static (musl) archives of VERSION;
# PLATFORMS is buildx's --platform, such as linux/amd64,linux/arm64. The
# options go to `docker buildx build` as they are: an --output, --load or
# --push, and tags.
#
# Environment:
#   VERSION   the release's version (required)
#   REVISION  the commit, for the revision label
#   CREATED   the build date, RFC 3339, for the created label and the
#             image's timestamps (SOURCE_DATE_EPOCH), so that every build
#             of one release makes the same image
#   SOURCE    the repository's URL, for the index's source annotation
#
# A multi-platform build's index carries the labels' annotations too, as
# GHCR shows those. There's no buildx provenance or SBOM: the release
# workflow attests the image's provenance itself.
set -eu

if [ "$#" -lt 2 ]; then
  echo "usage: build-image.sh CONTEXT PLATFORMS [OPTION...]" >&2
  exit 2
fi
context=$1
platforms=$2
shift 2
if [ -z "${VERSION:-}" ]; then
  echo "build-image.sh: set VERSION to the release's version" >&2
  exit 2
fi
revision=${REVISION:-unknown}
created=${CREATED:-unknown}
source=${SOURCE:-https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy}
description="A Rust port of CLIProxyAPI with a built-in UI and first-class WebSocket support"

if [ "$created" != unknown ]; then
  SOURCE_DATE_EPOCH=$(date -u -d "$created" +%s)
  export SOURCE_DATE_EPOCH
fi

case $platforms in
  *,*)
    set -- \
      --annotation "index:org.opencontainers.image.source=$source" \
      --annotation "index:org.opencontainers.image.description=$description" \
      --annotation "index:org.opencontainers.image.licenses=MIT" \
      --annotation "index:org.opencontainers.image.version=$VERSION" \
      --annotation "index:org.opencontainers.image.revision=$revision" \
      "$@"
    ;;
esac

exec docker buildx build \
  --file docker/Dockerfile \
  --platform "$platforms" \
  --build-arg "VERSION=$VERSION" \
  --build-arg "REVISION=$revision" \
  --build-arg "CREATED=$created" \
  --provenance=false --sbom=false \
  "$@" \
  "$context"

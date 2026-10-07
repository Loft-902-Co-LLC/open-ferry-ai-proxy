# Ported from CLIProxyAPI Dockerfile (v8.0.15, MIT).
# https://github.com/router-for-me/CLIProxyAPI
#
# open-ferry's container image, ghcr.io/loft-902-co-llc/open-ferry, for
# linux/amd64 and linux/arm64. It's built from a release's static (musl)
# Linux archives, not from source: the build context is a directory that
# holds SHA256SUMS and the two archives, open-ferry-VERSION-x86_64-unknown-
# linux-musl.tar.gz and open-ferry-VERSION-aarch64-unknown-linux-musl.tar.gz.
# The release workflow builds it so; to build it from a published release:
#
#   docker buildx build -f docker/Dockerfile --build-arg VERSION=0.1.0 \
#     --platform linux/amd64,linux/arm64 DIR
#
# Each archive is checked against SHA256SUMS before it is unpacked. No stage
# runs code for the target platform, so an arm64 image builds on an amd64
# machine without emulation, and nothing is downloaded but the pinned images
# below.
#
# The paths are upstream's, so a Compose file written for CLIProxyAPI's
# image switches by changing the image name: the config is
# /CLIProxyAPI/config.yaml (the working directory's config.yaml), the auth
# directory /root/.cli-proxy-api (the template's ~/.cli-proxy-api), the
# logs /CLIProxyAPI/logs when that directory exists (a mount), and the port
# 8317. The template's empty server.host, every interface, is what a
# container needs: Docker forwards the published port to the container's
# own address, not its loopback.
#
# The base is Alpine, pinned by digest: a small image (about 8 MB) with the
# Mozilla CA bundle (/etc/ssl/certs/ca-certificates.crt), which rustls
# reads, and a shell, for `docker exec` and the sign-in commands. scratch or
# distroless would save those few megabytes but leave no shell. The binary
# is static, so it needs nothing else from the base.
#
# Deviations from upstream:
# - Built from the release's static binaries, on Alpine; upstream compiles
#   the source in the image build and runs it on Debian (bookworm).
# - The time zone is UTC: there's no TZ=Asia/Shanghai. Set TZ at run time
#   (such as TZ=Europe/London) for local times in the logs; the zone
#   database is in /usr/share/zoneinfo.
# - The binary is /usr/local/bin/open-ferry, on PATH. /CLIProxyAPI/
#   CLIProxyAPI is a link to it, for commands written for upstream's image.
# - The licenses are in /usr/share/doc/open-ferry.
# - The image has OCI labels: its source, version, revision and license.

# The Alpine release the image is based on, pinned by digest: a
# multi-platform index, so each platform gets its own.
ARG ALPINE=docker.io/library/alpine:3.24@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
# The zone database comes from Google's distroless static image, pinned by
# digest, rather than from Alpine's tzdata package, which would be
# downloaded at build time at whatever version the repository holds.
ARG DISTROLESS=gcr.io/distroless/static-debian12@sha256:d75cdd72874d4790092fcb1b058493ecf6bb5bf2b2b897045b00ff01d91843f2

# The archives are checked and unpacked on the build machine's own platform;
# TARGETARCH picks the archive of the image's.
FROM --platform=$BUILDPLATFORM ${ALPINE} AS unpack
ARG VERSION
ARG TARGETARCH
WORKDIR /dist
COPY SHA256SUMS open-ferry-${VERSION}-*-unknown-linux-musl.tar.gz ./
RUN set -eu; \
    case "${VERSION:-}" in \
      "") echo "Set the VERSION build argument to the release's version, such as 0.1.0" >&2; exit 1 ;; \
    esac; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64 ;; \
      arm64) arch=aarch64 ;; \
      *) echo "There's no open-ferry build for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    name="open-ferry-$VERSION-$arch-unknown-linux-musl"; \
    expected=$(awk -v want="$name.tar.gz" '{ n = $2; sub(/^\*/, "", n); if (n == want) print $1 }' SHA256SUMS); \
    actual=$(sha256sum "$name.tar.gz" | cut -d' ' -f1); \
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then \
      echo "$name.tar.gz doesn't match SHA256SUMS" >&2; exit 1; \
    fi; \
    tar -xzf "$name.tar.gz"; \
    mkdir -p /root-fs/usr/local/bin /root-fs/CLIProxyAPI /root-fs/usr/share/doc/open-ferry; \
    mv "$name/open-ferry" /root-fs/usr/local/bin/open-ferry; \
    chmod 755 /root-fs/usr/local/bin/open-ferry; \
    ln -s /usr/local/bin/open-ferry /root-fs/CLIProxyAPI/CLIProxyAPI; \
    mv "$name/config.example.yaml" /root-fs/CLIProxyAPI/config.example.yaml; \
    mv "$name/LICENSE" "$name/README.md" "$name/licenses" /root-fs/usr/share/doc/open-ferry/; \
    chown -R 0:0 /root-fs

FROM --platform=$BUILDPLATFORM ${DISTROLESS} AS zoneinfo

FROM ${ALPINE}
ARG VERSION
ARG REVISION=unknown
ARG CREATED=unknown
LABEL org.opencontainers.image.title="open-ferry" \
      org.opencontainers.image.description="A Rust port of CLIProxyAPI with a built-in UI and first-class WebSocket support" \
      org.opencontainers.image.source="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy" \
      org.opencontainers.image.url="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy" \
      org.opencontainers.image.documentation="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy#readme" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"
COPY --from=zoneinfo /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=unpack /root-fs/ /
WORKDIR /CLIProxyAPI
EXPOSE 8317
CMD ["open-ferry"]

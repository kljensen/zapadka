#!/usr/bin/env bash
#
# Builds a static Zapadka binary inside a pinned musl container.
#
# Used by the release workflow and callable by hand, so a developer can
# reproduce exactly what a release does without reading the workflow.
#
# The image is pinned to a *per-platform* manifest digest rather than to a tag
# or to a multi-arch index digest. A tag can be repointed at a different build,
# and an index leaves which platform image was used implicit. A release that
# cannot be rebuilt byte for byte is not reproducible however carefully
# everything else is pinned.
#
# Usage: build-musl.sh <target-triple> <docker-platform> [image-digest]

set -euo pipefail

TARGET="${1:?usage: build-musl.sh <target-triple> <docker-platform> [image-digest]}"
PLATFORM="${2:?usage: build-musl.sh <target-triple> <docker-platform> [image-digest]}"
DIGEST="${3:-}"

# rust:1.89-alpine, resolved per platform. Alpine's Rust is musl-native, so
# this produces a genuinely static binary rather than a glibc build
# cross-compiled and hoped about.
if [ -z "$DIGEST" ]; then
  case "$PLATFORM" in
    linux/amd64)
      DIGEST="sha256:f9617326395be71547efa3c8b8d58b89ff958afc594596713bceb585aabc067e" ;;
    linux/arm64)
      DIGEST="sha256:375c19a34ded78a3541ea2fe20f46cd494ee20f737d259bde4aed8ae340fdc10" ;;
    *)
      echo "no pinned image for platform $PLATFORM" >&2
      exit 1 ;;
  esac
fi

exec docker run --rm --platform "$PLATFORM" \
  -v "$PWD:/src" -w /src \
  "rust@$DIGEST" \
  sh -euc "
    apk add --no-cache musl-dev build-base >/dev/null
    cargo build --release --locked --target '$TARGET' -p zapadka
  "

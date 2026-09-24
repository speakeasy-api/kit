#!/usr/bin/env bash
# usage: docker-build.sh [slim|bookworm|alpine] [docker buildx build arguments...]
#
# Environment:
#   KIT_IMAGE  image name for the version tags (default: kit). Set it to an empty
#              string to build without tags, e.g. when pushing by digest.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
flavor=${1:-slim}
if [[ $# -gt 0 ]]; then shift; fi
case $flavor in
  slim | bookworm | alpine) ;;
  *)
    echo "unknown container flavor: $flavor (expected slim, bookworm, or alpine)" >&2
    exit 2
    ;;
esac

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)
revision=$(git -C "$root" rev-parse HEAD)
image=${KIT_IMAGE-kit}
tags=()
if [[ -n $image ]]; then
  tags+=(--tag "$image:$version-$flavor")
  if [[ $flavor == slim ]]; then
    tags+=(--tag "$image:$version")
  fi
fi

exec docker buildx build \
  --file "$root/Dockerfile" \
  --target "$flavor" \
  --build-arg "VERSION=$version" \
  --build-arg "REVISION=$revision" \
  ${tags[@]+"${tags[@]}"} \
  "$@" \
  "$root"

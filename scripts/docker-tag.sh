#!/usr/bin/env bash
# usage: docker-tag.sh slim|bookworm|alpine IMAGE@sha256:... [IMAGE@sha256:...]
#
# Creates one multi-platform manifest per release tag from the pushed per-platform
# digests. Every flavor gets IMAGE:TAG-FLAVOR; slim also gets IMAGE:TAG. When the
# release is the newest stable one, the floating IMAGE:FLAVOR tags (and IMAGE:latest
# for slim) move as well.
#
# Environment:
#   KIT_IMAGE          image name (default: the name of the first source digest)
#   KIT_RELEASE_TAG    release tag (default: v<Cargo.toml version>)
#   KIT_FLOATING_TAGS  true to also move the floating tags (default: false)
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 slim|bookworm|alpine IMAGE@sha256:... [IMAGE@sha256:...]" >&2
  exit 2
fi
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
flavor=$1
shift
case $flavor in
  slim | bookworm | alpine) ;;
  *)
    echo "unknown container flavor: $flavor (expected slim, bookworm, or alpine)" >&2
    exit 2
    ;;
esac

image=${KIT_IMAGE:-${1%%@*}}
release_tag=${KIT_RELEASE_TAG:-v$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)}
floating=${KIT_FLOATING_TAGS:-false}

tags=("$image:$release_tag-$flavor")
if [[ $flavor == slim ]]; then
  tags+=("$image:$release_tag")
fi
if [[ $floating == true ]]; then
  tags+=("$image:$flavor")
  if [[ $flavor == slim ]]; then
    tags+=("$image:latest")
  fi
fi

args=()
for tag in "${tags[@]}"; do
  args+=(--tag "$tag")
done
exec docker buildx imagetools create "${args[@]}" "$@"

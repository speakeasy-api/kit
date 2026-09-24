#!/usr/bin/env bash
# usage: smoke-linux-no-audio.sh PATH/kit
# Requires a native Linux GNU binary, readelf, and a working Docker daemon.
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
"$root/scripts/check-linux-audio-linkage.sh" "$@"
binary=$(CDPATH= cd -- "$(dirname "$1")" && pwd)/$(basename "$1")
# No audio libraries or devices; never invoke voice or consume model quota.
printf '%s\n' 'FROM ubuntu:24.04' \
  'RUN apt-get update && apt-get install -y --no-install-recommends libstdc++6 && rm -rf /var/lib/apt/lists/*' \
  | docker build -t kit-no-audio-smoke -
docker run --rm --network none -v "$binary:/kit:ro" kit-no-audio-smoke \
  sh -ec 'if ldconfig -p | grep -E "lib(asound|pulse).*\.so"; then exit 1; fi; /kit --version'

#!/usr/bin/env bash
# usage: check-linux-audio-linkage.sh PATH/kit
# Inspect ELF metadata without executing the binary or initializing audio.
set -euo pipefail
if [[ $# != 1 || ! -f $1 ]]; then
  echo "usage: $0 PATH/kit" >&2
  exit 2
fi
dynamic=$(readelf -d "$1")
if grep -E '\(NEEDED\).*lib(asound|pulse[^]]*)\.so' <<< "$dynamic"; then
  echo 'Audio libraries must not be startup dependencies.' >&2
  exit 1
fi

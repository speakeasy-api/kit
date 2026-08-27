#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
KIT_CODESIGN_IDENTITY="${KIT_CODESIGN_IDENTITY:-}"
if [ -z "$KIT_CODESIGN_IDENTITY" ] && [ -r "$root/.kit-codesign-identity" ]; then
  IFS= read -r KIT_CODESIGN_IDENTITY < "$root/.kit-codesign-identity"
fi
: "${KIT_CODESIGN_IDENTITY:?set KIT_CODESIGN_IDENTITY or create .kit-codesign-identity}"
KIT_CODESIGN_IDENTIFIER="${KIT_CODESIGN_IDENTIFIER:-com.speakeasy.kit}"

if [ "$(uname -s)" != Darwin ]; then
  echo "codesigning is only supported on macOS" >&2
  exit 1
fi

cargo build --locked --release
binary="${KIT_CODESIGN_BINARY:-target/release/kit}"
codesign \
  --force \
  --options runtime \
  --timestamp \
  --identifier "$KIT_CODESIGN_IDENTIFIER" \
  --sign "$KIT_CODESIGN_IDENTITY" \
  "$binary"
codesign --verify --strict --verbose=2 "$binary"
codesign --display --requirements - "$binary" 2>&1

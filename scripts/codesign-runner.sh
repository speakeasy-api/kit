#!/bin/sh
set -eu

binary=$1
shift

# Cargo also uses runners for tests and benchmarks. Only the `kit` binary needs
# the stable identity used by its Keychain entries.
if [ "$(basename "$binary")" = kit ]; then
  root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
  identity="${KIT_CODESIGN_IDENTITY:-}"
  if [ -z "$identity" ] && [ -r "$root/.kit-codesign-identity" ]; then
    IFS= read -r identity < "$root/.kit-codesign-identity"
  fi
  identifier="${KIT_CODESIGN_IDENTIFIER:-com.speakeasy.kit}"
  if [ -n "$identity" ] && security find-identity -v -p codesigning 2>/dev/null | grep -Fq "\"$identity\""; then
    codesign --force --options runtime --identifier "$identifier" --sign "$identity" "$binary"
  else
    echo "warning: no configured Apple signing identity is available; running Kit without a stable Keychain signature" >&2
  fi
fi

exec "$binary" "$@"

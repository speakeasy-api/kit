#!/usr/bin/env bash
set -euo pipefail

version=2.45.4
sha256=090ec29491aad50aec10631bf6e62253fed733c50f3aab0f5ffc86bc170bdbef
binary_sha256=6aa2b4da95304b343bea12890c59f9655aa428c08b351d57d592cfab4e88a9f1
destination=${1:-"${HOME}/.local/bin"}
install_root=$destination/xcodegen-$version
binary=$install_root/bin/xcodegen
mkdir -p "$destination"
if [[ ! -x $binary ]]; then
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/xcodegen.XXXXXX")
  trap 'rm -rf "$temporary"' EXIT
  archive=$temporary/xcodegen.zip
  curl -fsSL --retry 3 --retry-all-errors \
    "https://github.com/yonaskolb/XcodeGen/releases/download/$version/xcodegen.zip" \
    -o "$archive"
  actual=$(shasum -a 256 "$archive" | awk '{ print $1 }')
  if [[ $actual != "$sha256" ]]; then
    echo "XcodeGen archive checksum mismatch: $actual" >&2
    exit 1
  fi
  unzip -q "$archive" -d "$temporary/unpacked"
  rm -rf "$install_root"
  mv "$temporary/unpacked/xcodegen" "$install_root"
fi
actual_binary=$(shasum -a 256 "$binary" | awk '{ print $1 }')
if [[ $actual_binary != "$binary_sha256" ]]; then
  echo "XcodeGen binary checksum mismatch: $actual_binary" >&2
  exit 1
fi
printf '%s\n' "$sha256" > "$destination/.xcodegen-$version-sha256"
launcher=$destination/xcodegen
rm -f "$launcher"
cat > "$launcher" <<EOF
#!/bin/sh
root=\$(CDPATH= cd -- "\$(dirname "\$0")" && pwd)
exec "\$root/xcodegen-$version/bin/xcodegen" "\$@"
EOF
chmod +x "$launcher"
"$launcher" --version

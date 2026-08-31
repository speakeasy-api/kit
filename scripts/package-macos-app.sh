#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 PATH/Kit.app DIST_DIR VERSION" >&2
  exit 2
fi
if [[ $(uname -s) != Darwin ]]; then
  echo "Kit.app packaging requires macOS" >&2
  exit 1
fi
for command in codesign ditto lipo shasum; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
app=$1
dist=$2
version=$3
helper=$app/Contents/Helpers/kit
executable=$app/Contents/MacOS/Kit
source_helper=${KIT_RELEASE_HELPER:-$root/target/aarch64-apple-darwin/release/kit}
signed=${SIGN_RELEASE:-false}

[[ -d $app && -x $helper && -x $executable ]] || { echo "invalid Kit.app archive product: $app" >&2; exit 1; }
for binary in "$executable" "$helper"; do
  arches=$(lipo -archs "$binary")
  [[ $arches == arm64 ]] || { echo "expected thin arm64 binary at $binary; found: $arches" >&2; exit 1; }
done
cmp -s "$source_helper" "$helper" || { echo "archived helper does not match the release build" >&2; exit 1; }

mkdir -p "$dist"
if [[ $signed == true ]]; then
  : "${MACOS_SIGNING_IDENTITY:?MACOS_SIGNING_IDENTITY must be set}"
  : "${APPLE_API_KEY_P8_BASE64:?APPLE_API_KEY_P8_BASE64 must be set}"
  : "${APPLE_API_KEY_ID:?APPLE_API_KEY_ID must be set}"
  : "${APPLE_API_ISSUER_ID:?APPLE_API_ISSUER_ID must be set}"
  temporary=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kit-app-notary.XXXXXX")
  trap 'rm -rf "$temporary"' EXIT

  # Nested code must be signed before the enclosing bundle. Neither executable needs
  # an exception to the Hardened Runtime, so both use the explicit empty entitlement set.
  codesign --force --options runtime --timestamp \
    --identifier com.speakeasy.kit \
    --entitlements "$root/macos/Config/KitDesktop.entitlements" \
    --sign "$MACOS_SIGNING_IDENTITY" "$helper"
  codesign --verify --strict --verbose=2 "$helper"
  codesign --force --options runtime --timestamp \
    --identifier com.speakeasy.kit.desktop \
    --entitlements "$root/macos/Config/KitDesktop.entitlements" \
    --sign "$MACOS_SIGNING_IDENTITY" "$app"
  codesign --verify --deep --strict --verbose=2 "$app"
  for signed_code in "$helper" "$app"; do
    signature=$(codesign --display --verbose=4 "$signed_code" 2>&1)
    grep -Eq '^Authority=Developer ID Application:' <<<"$signature" || {
      echo "$signed_code is not signed by a Developer ID Application identity" >&2
      exit 1
    }
    grep -Eq '^CodeDirectory .*flags=.*\(runtime\)' <<<"$signature" || {
      echo "$signed_code does not enable the Hardened Runtime" >&2
      exit 1
    }
  done

  submission=$temporary/Kit-app-notarization.zip
  ditto -c -k --sequesterRsrc --keepParent "$app" "$submission"
  api_key=$temporary/AuthKey_${APPLE_API_KEY_ID}.p8
  printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$api_key"
  chmod 600 "$api_key"
  notary_log=$temporary/notarytool.log
  xcrun notarytool submit "$submission" \
    --key "$api_key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" \
    --wait 2>&1 | tee "$notary_log"
  grep -Eq '(^|[[:space:]])status: Accepted([[:space:]]|$)' "$notary_log" || {
    echo "Apple did not accept Kit.app notarization" >&2
    exit 1
  }
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  codesign --verify --strict --verbose=2 "$helper"
  codesign --verify --deep --strict --verbose=2 "$app"
  spctl --assess --type execute --verbose=2 "$app"
fi

archive=$dist/Kit-v$version-aarch64-apple-darwin.zip
rm -f "$archive" "$archive.sha256"
ditto -c -k --sequesterRsrc --keepParent "$app" "$archive"
shasum -a 256 "$archive" > "$archive.sha256"
printf '%s\n' "$archive"

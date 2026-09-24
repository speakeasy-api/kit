#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 Debug|Release build|test|archive" >&2
  exit 2
fi
configuration=$1
action=$2
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
derived=${KIT_DERIVED_DATA:-$root/macos/.build}

args=(
  -project "$root/macos/KitDesktop.xcodeproj"
  -scheme KitDesktop
  -configuration "$configuration"
  -derivedDataPath "$derived"
  CODE_SIGNING_ALLOWED=NO
)
case $configuration in
  Debug)
    args+=(-destination 'platform=macOS,arch=arm64' "KIT_BINARY=${KIT_BINARY:-$root/target/debug/kit}")
    ;;
  Release)
    args+=(-destination 'generic/platform=macOS')
    ;;
  *)
    echo "unknown configuration: $configuration" >&2
    exit 2
    ;;
esac
if [[ $action == archive ]]; then
  args+=(-archivePath "${KIT_ARCHIVE_PATH:-$derived/Kit.xcarchive}")
fi
if [[ -n ${CI:-} ]]; then
  args+=(-quiet)
fi

exec xcodebuild "$action" "${args[@]}"

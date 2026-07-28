#!/bin/sh
set -eu

version=0.2.89
root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
tools_dir="$root/.tools"
act_path="$tools_dir/bin/act"

os=$(uname -s)
case $(uname -m) in
  arm64|aarch64) arch=arm64 ;;
  x86_64) arch=x86_64 ;;
  *) echo "unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
esac

case "$os/$arch" in
  Darwin/arm64) sha256=48ae218af96725f7635a66de2b87e1e346893b02add0f16b92f560296b2151fc ;;
  Darwin/x86_64) sha256=41b31488e7c254baec31cce12c7dade3e35973b8a31b9486206ad43f233d814e ;;
  Linux/arm64) sha256=daa8679ba9615a74d2d0cec321dc593f21948a2a11bb65862b063d8b930f4bcb ;;
  Linux/x86_64) sha256=0191d6f1f3b716b5c55820032605d05fc3c1cdbf581ebeff655019e5dd1524c0 ;;
  *) echo "unsupported host operating system: $os" >&2; exit 1 ;;
esac

installed_version=$("$act_path" --version 2>/dev/null || :)
if [ "$installed_version" = "act version $version" ]; then
  echo "act v$version is already installed at $act_path"
  exit 0
fi

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "tar is required" >&2; exit 1; }

asset="act_${os}_${arch}.tar.gz"
url="https://github.com/nektos/act/releases/download/v${version}/${asset}"
archive="$tools_dir/.${asset}.$$"
extract_dir="$tools_dir/.act-v${version}.$$"
trap 'rm -rf "$archive" "$extract_dir"' EXIT HUP INT TERM
mkdir -p "$tools_dir/bin" "$extract_dir"

curl --fail --location --silent --show-error --output "$archive" "$url"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$archive")
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$archive")
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
actual=${actual%% *}
[ "$actual" = "$sha256" ] || {
  echo "checksum mismatch for $asset: expected $sha256, got $actual" >&2
  exit 1
}

tar -xzf "$archive" -C "$extract_dir"
[ -x "$extract_dir/act" ] || { echo "$asset did not contain an executable act" >&2; exit 1; }
mv "$extract_dir/act" "$act_path"
echo "installed act v$version for $os/$arch at $act_path"

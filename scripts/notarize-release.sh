#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/notarize-release.sh vVERSION

Builds, Developer ID signs, and notarizes the macOS ARM64 release binary on
this Mac. The exact signed binary is preserved under dist/notarize/vVERSION/.

Required configuration (`.env` or environment variables):
  KIT_NOTARY_API_KEY_DOCUMENT
  KIT_NOTARY_API_KEY_VAULT
  KIT_NOTARY_API_KEY_ID
  KIT_NOTARY_API_ISSUER_ID

Optional overrides:
  KIT_CODESIGN_IDENTITY
  KIT_CODESIGN_IDENTIFIER
EOF
}

if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  usage
  exit 0
fi
if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

for command in cargo codesign ditto git op security tar xcrun; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done
if [[ $(uname -s) != Darwin ]]; then
  echo "notarization must run on macOS" >&2
  exit 1
fi

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"

if [[ -f $root/.env ]]; then
  # shellcheck source=/dev/null
  source "$root/.env"
fi

tag=$1
version=${tag#v}
cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
if [[ $tag != "v$cargo_version" ]]; then
  echo "tag $tag does not match Cargo version $cargo_version (expected v$cargo_version)" >&2
  exit 1
fi
if [[ -n $(git status --porcelain) ]]; then
  echo "working tree must be clean before preparing a signed release" >&2
  exit 1
fi

identity=${KIT_CODESIGN_IDENTITY:-Developer ID Application: Inlucent Limited (TAMRUK8SL6)}
identifier=${KIT_CODESIGN_IDENTIFIER:-com.danielkov.kit}
api_key_document=${KIT_NOTARY_API_KEY_DOCUMENT:?KIT_NOTARY_API_KEY_DOCUMENT must be set}
api_key_vault=${KIT_NOTARY_API_KEY_VAULT:?KIT_NOTARY_API_KEY_VAULT must be set}
api_key_id=${KIT_NOTARY_API_KEY_ID:?KIT_NOTARY_API_KEY_ID must be set}
api_issuer_id=${KIT_NOTARY_API_ISSUER_ID:?KIT_NOTARY_API_ISSUER_ID must be set}
target=aarch64-apple-darwin
commit=$(git rev-parse HEAD)
out_dir="$root/dist/notarize/$tag"
target_dir="$root/target/notarize/$tag"
source_binary="$target_dir/$target/release/kit"
binary="$out_dir/kit"
submission_zip="$out_dir/kit-notarization.zip"
archive="$out_dir/kit-$tag-$target.tar.gz"
log="$out_dir/notarytool.log"

if [[ -e $out_dir ]]; then
  echo "output already exists: $out_dir" >&2
  echo "remove it explicitly before starting a new submission" >&2
  exit 1
fi
if ! security find-identity -v -p codesigning | grep -Fq "\"$identity\""; then
  echo "Developer ID signing identity is not available: $identity" >&2
  exit 1
fi
op account get >/dev/null

umask 077
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/kit-notary.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT
api_key="$tmp_dir/AuthKey_${api_key_id}.p8"
op document get "$api_key_document" --vault "$api_key_vault" --output "$api_key" >/dev/null
chmod 600 "$api_key"
source_dir="$tmp_dir/source"
mkdir -p "$source_dir"
git archive --format=tar "$commit" | tar -xf - -C "$source_dir"

mkdir -p "$out_dir"
printf '%s\n' "$commit" > "$out_dir/source-commit.txt"
echo "Building Kit $version from commit $commit for $target..."
(
  cd "$source_dir"
  CARGO_TARGET_DIR="$target_dir" cargo build --locked --release --target "$target"
)
cp "$source_binary" "$binary"

echo "Signing with $identity..."
codesign --force --options runtime --timestamp \
  --identifier "$identifier" \
  --sign "$identity" \
  "$binary"
codesign --verify --strict --verbose=2 "$binary"
"$binary" --version

ditto -c -k --keepParent "$binary" "$submission_zip"

echo "Submitting to Apple and waiting for completion..."
echo "The signed binary and submission log will remain in $out_dir"
xcrun notarytool submit "$submission_zip" \
  --key "$api_key" \
  --key-id "$api_key_id" \
  --issuer "$api_issuer_id" \
  --wait 2>&1 | tee "$log"
if ! grep -Eq '(^|[[:space:]])status: Accepted([[:space:]]|$)' "$log"; then
  echo "Apple did not report an accepted submission; preserving $out_dir" >&2
  exit 1
fi

tar -C "$out_dir" -czf "$archive" kit
shasum -a 256 "$archive" > "$archive.sha256"

echo
echo "Notarization accepted. Release archive:"
echo "  $archive"
echo "Checksum:"
cat "$archive.sha256"

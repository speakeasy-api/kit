#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 0 ]]; then
  echo "Usage: scripts/release-local.sh" >&2
  exit 2
fi

for command in cargo gh git mise shasum; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
[[ -n $version ]] || { echo "could not read version from Cargo.toml" >&2; exit 1; }
tag="v$version"
branch=$(git symbolic-ref --quiet --short HEAD) || { echo "release from a branch, not detached HEAD" >&2; exit 1; }
commit=$(git rev-parse HEAD)
releases_repo=${KIT_RELEASES_REPO:-danielkov/kit-releases}
remote=${KIT_RELEASE_REMOTE:-origin}
out_dir="$root/dist/notarize/$tag"
macos="$out_dir/kit-$tag-aarch64-apple-darwin.tar.gz"
linux="$out_dir/kit-$tag-x86_64-unknown-linux-gnu.tar.gz"
sums="$out_dir/SHA256SUMS"

[[ -z $(git status --porcelain) ]] || { echo "working tree must be clean" >&2; exit 1; }
! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "local tag already exists: $tag" >&2; exit 1; }
! git ls-remote --exit-code --tags "$remote" "refs/tags/$tag" >/dev/null 2>&1 || { echo "remote tag already exists: $tag" >&2; exit 1; }
! gh release view "$tag" --repo "$releases_repo" >/dev/null 2>&1 || { echo "release already exists: $releases_repo $tag" >&2; exit 1; }
gh auth status >/dev/null

if command -v caffeinate >/dev/null; then
  caffeinate -i -w $$ &
fi

echo "Running release checks..."
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked

"$root/scripts/notarize-release.sh" "$tag" &
macos_pid=$!
"$root/scripts/build-linux-release.sh" "$tag" &
linux_pid=$!
cleanup_builds() {
  kill "$macos_pid" "$linux_pid" 2>/dev/null || true
}
trap cleanup_builds EXIT

wait "$macos_pid"
test "$(cat "$out_dir/source-commit.txt")" = "$commit"
wait "$linux_pid"
trap - EXIT

(
  cd "$out_dir"
  shasum -a 256 "$(basename "$macos")" "$(basename "$linux")" > SHA256SUMS
  shasum -a 256 -c SHA256SUMS
)

echo "Creating hidden draft release..."
gh release create "$tag" "$macos" "$linux" "$sums" \
  --repo "$releases_repo" \
  --target main \
  --draft \
  --title "Kit $version" \
  --notes "Prebuilt Kit binaries. Source code is maintained separately."

git tag "$tag" "$commit"
git push "$remote" "$branch"
git push "$remote" "$tag"

gh release edit "$tag" --repo "$releases_repo" --draft=false --latest

echo "Testing the published mise installation..."
mise_dir=$(mktemp -d "${TMPDIR:-/tmp}/kit-mise-release.XXXXXX")
MISE_DATA_DIR="$mise_dir/data" \
MISE_CONFIG_DIR="$mise_dir/config" \
MISE_CACHE_DIR="$mise_dir/cache" \
MISE_STATE_DIR="$mise_dir/state" \
  mise exec "github:$releases_repo@$version" -- kit --version | grep -Fx "kit $version"
rm -rf "$mise_dir"

echo "Published https://github.com/$releases_repo/releases/tag/$tag"

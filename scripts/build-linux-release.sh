#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: scripts/build-linux-release.sh vVERSION" >&2
  exit 2
fi

for command in docker file git shasum tar; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"
tag=$1
target=x86_64-unknown-linux-gnu
out_dir="$root/dist/notarize/$tag"
commit_file="$out_dir/source-commit.txt"
archive="$out_dir/kit-$tag-$target.tar.gz"
builder=localhost/kit-linux-builder:rust-1.94-bookworm

test -f "$commit_file" || { echo "missing notarized release state: $commit_file" >&2; exit 1; }
commit=$(cat "$commit_file")
test "$(git rev-parse "$commit")" = "$commit"
test ! -e "$archive" || { echo "refusing to overwrite $archive" >&2; exit 1; }

if ! docker image inspect "$builder" >/dev/null 2>&1; then
  echo "Creating reusable native ARM64 cross-build image..."
  docker build --platform linux/arm64 -t "$builder" - <<'EOF'
FROM rust:1.94.0-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
 && rustup target add x86_64-unknown-linux-gnu \
 && rm -rf /var/lib/apt/lists/*
EOF
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/kit-linux-release.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT
source_dir="$tmp_dir/source"
target_dir="$root/target/local-linux/$tag"
mkdir -p "$source_dir" "$target_dir"
git archive --format=tar "$commit" | tar -xf - -C "$source_dir"

echo "Building Linux x86-64 from commit $commit..."
docker run --rm --platform linux/arm64 \
  -v "$source_dir:/work" \
  -v "$target_dir:/cargo-target" \
  -v kit-cargo-registry:/usr/local/cargo/registry \
  -w /work \
  "$builder" \
  bash -c 'set -euo pipefail
    export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH
    CARGO_TARGET_DIR=/cargo-target \
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
      cargo build --locked --release --target x86_64-unknown-linux-gnu
    mkdir -p stage
    cp /cargo-target/x86_64-unknown-linux-gnu/release/kit stage/kit
    tar -C stage -czf kit-'"$tag"'-x86_64-unknown-linux-gnu.tar.gz kit'

cp "$source_dir/kit-$tag-$target.tar.gz" "$archive"
shasum -a 256 "$archive" > "$archive.sha256"
shasum -a 256 -c "$archive.sha256"

tar -xOf "$archive" kit > "$tmp_dir/kit"
chmod +x "$tmp_dir/kit"
file "$tmp_dir/kit" | grep -q 'ELF 64-bit.*x86-64'
docker run --rm --platform linux/amd64 \
  -v "$tmp_dir/kit:/kit:ro" \
  debian:bookworm-slim /kit --version | grep -Fx "kit ${tag#v}"

echo "Linux release archive: $archive"

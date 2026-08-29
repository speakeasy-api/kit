#!/bin/sh
# Install the latest Kit release binary.
#
#   curl -fsSL https://raw.githubusercontent.com/speakeasy-api/kit/main/install.sh | sh
#
# Environment:
#   KIT_VERSION      release to install, e.g. v0.1.108 (default: latest)
#   KIT_INSTALL_DIR  destination directory (default: ~/.local/bin)
#   GITHUB_TOKEN     optional, raises the GitHub API rate limit
set -eu

REPO="speakeasy-api/kit"
INSTALL_DIR="${KIT_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${KIT_VERSION:-latest}"

log() { printf '%s\n' "$*" >&2; }
die() { log "install.sh: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

need curl
need tar

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
  Darwin/x86_64) die "no prebuilt Intel macOS binary yet; use 'cargo install --git https://github.com/$REPO' or Rosetta is not supported" ;;
  Linux/aarch64|Linux/arm64) die "no prebuilt linux/arm64 tarball yet; use the ghcr.io/$REPO container image (linux/arm64 is published) or 'cargo install --git https://github.com/$REPO'" ;;
  *) die "unsupported platform: $os/$arch" ;;
esac

auth_header=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
  auth_header="Authorization: Bearer $GITHUB_TOKEN"
fi

if [ "$VERSION" = "latest" ]; then
  api="https://api.github.com/repos/$REPO/releases/latest"
  VERSION=$(curl -fsSL ${auth_header:+-H "$auth_header"} "$api" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$VERSION" ] || die "could not resolve the latest release tag"
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac

asset="kit-$VERSION-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t kit-install)
trap 'rm -rf "$tmp"' EXIT

log "Downloading $asset"
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | cut -d ' ' -f 1)
else
  actual=$(shasum -a 256 "$tmp/$asset" | cut -d ' ' -f 1)
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/kit" "$INSTALL_DIR/kit"

log "Installed kit $VERSION to $INSTALL_DIR/kit"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) log "Add $INSTALL_DIR to your PATH, for example: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
log "Next: kit init && kit auth login openai"

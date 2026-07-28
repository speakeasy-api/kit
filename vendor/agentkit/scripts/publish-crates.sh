#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.toml
}

ROOT_VERSION="$(workspace_version)"
if [[ -z "$ROOT_VERSION" ]]; then
  echo "Failed to determine workspace version from Cargo.toml" >&2
  exit 1
fi

VERSION="${VERSION:-$ROOT_VERSION}"
WAIT_SECONDS="${WAIT_SECONDS:-10}"

CRATES=(
  agentkit-core
  agentkit-http
  agentkit-capabilities
  agentkit-context
  agentkit-tools-core
  agentkit-tools-derive
  agentkit-task-manager
  agentkit-loop
  agentkit-acp
  agentkit-compaction
  agentkit-adapter-completions
  agentkit-reporting
  agentkit-mcp
  agentkit-tool-fs
  agentkit-tool-shell
  agentkit-tool-compose
  agentkit-tool-skills
  agentkit-provider-openrouter
  agentkit-provider-openai
  agentkit-provider-ollama
  agentkit-provider-vllm
  agentkit-provider-groq
  agentkit-provider-mistral
  agentkit-provider-anthropic
  agentkit-provider-cerebras
  agentkit
  how-cli
)

crate_exists() {
  local crate="$1"
  python - "$crate" "$VERSION" <<'PY' >/dev/null 2>&1
import sys
import urllib.error
import urllib.request

crate, version = sys.argv[1], sys.argv[2]
url = f"https://crates.io/api/v1/crates/{crate}/{version}"

try:
    with urllib.request.urlopen(url):
        pass
except urllib.error.HTTPError as exc:
    if exc.code == 404:
        raise SystemExit(1)
    raise
PY
}

publish_and_wait() {
  local crate="$1"

  if crate_exists "$crate"; then
    echo "Skipping ${crate}@${VERSION}; already present on crates.io."
    return 0
  fi

  echo "Publishing ${crate}@${VERSION}..."
  cargo publish -p "$crate" --locked --no-verify

  echo "Waiting for ${crate}@${VERSION} to appear on crates.io..."
  until crate_exists "$crate"; do
    sleep "$WAIT_SECONDS"
  done
}

main() {
  cargo check --workspace

  for crate in "${CRATES[@]}"; do
    publish_and_wait "$crate"
  done
}

main "$@"

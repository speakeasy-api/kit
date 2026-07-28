#!/bin/sh
set -eu

if [ "${KIT_ALLOW_BILLING:-}" != 1 ]; then
    echo "KIT_ALLOW_BILLING=1 is required; no provider request was made" >&2
    exit 2
fi

cargo build --locked --bin kit
exec cargo test --locked --manifest-path dogfood-harness/Cargo.toml real_provider_billing_smoke -- --ignored --exact

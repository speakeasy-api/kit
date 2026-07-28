# Repository API and dogfood operation

The daemon opens one `ManagedWorkspace` from the trusted `KIT_PROJECT_ROOT` (the current directory
when unset). It reads at most 256 KiB from the regular file `.kit/native.json`. Version 1 declares
one to 64 sealed verification checks, their exact argv, immutable image/tool/config digests,
resource ceilings, affected paths, requirement class, and diagnostic adapter. Invalid trusted
configuration prevents readiness. The same trusted file sets
`edit_validation_wall_time_millis`; it must be in `1..=300000`, while the default for projects
without native configuration remains 10 seconds. A model request cannot raise this project policy.
Native workspace reconciliation uses at least its 10-second default and at most 20 seconds of this
trusted allowance; longer edit-validation policies do not widen repository reads.
It is never replaced by a daemon-host command fallback.

The authenticated `/v1/projects/{project_id}/repository/*` API and `kit repo` CLI expose revision,
capability listing, discover, search, read, edit, run, and check. The CLI sends the same HTTP routes
as other clients. Tool inputs are read from `--input-file PATH`, `--input-file -`, or stdin; JSON,
environment values, and other potentially sensitive bytes are not accepted as argv payloads.
`edit`, `run`, and `check` require an explicit `--idempotency-key` which callers retain until the
durable result is reconciled.

All six calls pass through the M001 capability kernel and then the M004 native dispatcher. Direct
edit therefore uses the same baseline, validation, COW stage, syntax/formatter adapters, fast
verification, feedback, recovery materialization, and actual-diff artifact path as an agent tool
call. Run and check use the registered M003 attempt executor. Results, intent/outcome events, costs,
and authorized artifact references survive daemon restart. Artifact bytes are available only from
the authenticated repository artifact endpoint and are checked against principal and project
ownership.

The repository result/event schema is not yet shipped. Startup therefore performs a one-time
mutating terminal-event migration (`migration_version: 1`) rather than treating pre-release rows as
immutable history. For every committed kernel outcome it rebuilds any missing or contradictory
result and terminal event from that outcome, including exact cost and artifact projections and the
event payload digest. The result row and event are replaced in one SQLite transaction; an existing
terminal event keeps its earliest sequence/cursor, duplicate terminal events are removed, and an
already matching version-1 event is left byte-for-byte unchanged. Repeating startup is idempotent.

Artifact responses also carry the verified source-manifest digest, media type, class, principal,
and project as `X-Kit-Artifact-*` headers; the CLI preserves those fields alongside the bytes.
The revision response includes both the epoch-scoped `revision` token used for mutation fencing and
the canonical `digest` of checkout bytes. A daemon restart may rotate the token for replay defense;
the digest remains unchanged when the checkout is unchanged.

## Platform blockers

Missing native isolation, sealed helper, syntax worker, formatter image, verification registry, or
diagnostic adapter makes only the dependent tool unavailable. Responses use RFC 9457 problem
details or a failed durable result with the native failure code. Kit does not execute an equivalent
host command and does not claim successful release evidence. Unsupported local isolation remains
tracked by `EXT-22` on Linux and `EXT-19` on Windows; production formatter and syntax images require
their separately attested external artifacts.

## Manual real-provider smoke

This command is intentionally not a CI job and can incur provider billing:

```sh
KIT_PROJECT_ROOT="$PWD" \
KIT_PROVIDER=openai \
OPENAI_API_KEY="$(<"$HOME/.config/kit/openai-key")" \
KIT_ALLOW_BILLING=1 \
KIT_DOGFOOD_CHECK_IMAGE="$EXTERNALLY_PINNED_CHECK_IMAGE" \
KIT_NATIVE_CONTAINER_IMAGE="$EXTERNALLY_PINNED_RUN_IMAGE" \
KIT_DOGFOOD_TOOL_DIGEST="$EXTERNALLY_PINNED_TOOL_DIGEST" \
KIT_DOGFOOD_CONFIG_DIGEST="$EXTERNALLY_PINNED_CONFIG_DIGEST" \
KIT_CONTAINER_HELPER_SHA256="$EXTERNALLY_PINNED_HELPER_DIGEST" \
scripts/dogfood_real_provider.sh
```

Use the equivalent documented credential-file setting for Anthropic or OpenRouter. Do not place a
credential in shell history or a repository file. The deterministic dogfood test is the non-billed
CI path. `real_provider_preflight_without_network_or_billing` is separately named and is not billing
or production evidence. The billing script exits before Cargo unless `KIT_ALLOW_BILLING=1` is set;
with opt-in it runs the ignored smoke scenario and makes the provider request.

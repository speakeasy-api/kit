# Agentkit 0.10.2 Compatibility Audit

## Pin

Kit targets only the repository-vendored snapshot in `vendor/agentkit/`:

| Identity | Pinned value |
| --- | --- |
| Source commit | `c3926f1c4f3c945d400c8b6ef039da1f84826fcd` |
| Source tree | `5befb5676ea31703f4485e2d4b5869c39a39cb0f` |
| Dirty overlay SHA-256 | `92178443493858a217a04387442b56ecd2499e86b05b699aa76b685be146abd1` |
| Excluded generated-path list SHA-256 | `6013053000cc27b0e77ed61964266ff38e246bee41fee9ef829c0d8763ecd3ae` |
| Agentkit aggregate SHA-256 | `7a04d34e1509a0325bba5bd804f4d76afb6662ee7754d4ee903aa59b51867d0a` |
| Runlet aggregate SHA-256 | `fef525f0008de628b1aff655d2e5685d2c826c76c8517c50e1ce8a88cfcbb8ef` |
| Agentkit version | `0.10.2` |
| Runlet version/source | `0.1.0`, repository path `vendor/runlet` |

These values equal `docs/compatibility/pins/agentkit-snapshot.yaml`,
`vendor/agentkit/SNAPSHOT-METADATA.yaml`, and the closed `0.02` preflight in
`docs/decisions/PRE-0002-agentkit-pin.md`. The conformance test also compares
the bridge constants to both preflight records. `scripts/verify_pins.sh`
recomputes the payload digests and verifies the manifests.

The four post-capture changes are rustfmt-only. Formatting the reconstructed
recorded files with the pinned Rust 1.94.0 rustfmt produces the payload files
byte-for-byte. The deterministic Agentkit and Runlet formatting patch digests
are `6e598fadf82872029ac172008e03f7d9d2795e54d985d545509ea55cbaea5ef4`
and `da354fef9418f6f54bcb95349e7f5da6da92d2970e2ae8620a1a9a9ca14ad12e`;
the snapshot metadata records every prior and normalized file hash. Only line
wrapping changed, with no token, behavior, schema, or public API change.

## Mapping Contract

`src/agent/agentkit_bridge/mapping.rs` maps agentkit's normalized model into
owned Kit canonical values. Every upstream enum match intentionally has no
wildcard arm, so a newly added upstream variant fails compilation. The source
audit independently counts each enum in the pinned vendored source.

| Agentkit surface | Pinned shape | Kit semantics |
| --- | ---: | --- |
| `ItemKind` | 7 variants | system, developer, user, assistant, tool, context, notification |
| `Part` | 8 variants | text, media, file, structured, reasoning summary, tool call, tool result, custom |
| `PartKind` | 8 variants | same discriminants; reasoning begins a suppressed delta stream |
| `Modality` | 4 variants | audio, image, video, binary |
| `DataRef` | 4 variants | inline text, inline bytes, URI, artifact handle |
| `ToolOutput` | 4 variants | text, structured, nested parts, files |
| `Delta` | 6 variants | begin, append text, append bytes, replace structured, set metadata, commit |
| `FinishReason` | 7 variants | completed, tool call, max tokens, cancelled, blocked, error, provider other |
| `TranscriptEvent` | one struct shape | session-addressed append containing the complete canonical item |
| `LoopInterrupt` | 3 variants | durable approval wait, input wait, non-blocking tool-round boundary |
| `ToolInterruption` | 1 variant | pre-loop tool approval maps to the same durable approval semantics |
| `ApprovalReason` | 7 variants | preserved as a typed approval reason |

Item conversion is bidirectional. Provider item IDs, timestamps, ordinary
metadata, schemas, tool correlation IDs, error flags, and custom payloads are
preserved. Reasoning is deliberately asymmetric: only a provider-exposed
summary and redaction flag enter Kit. Opaque reasoning data and reasoning
metadata are represented as `null` and can never be converted back into
content. `DeltaMapper` tracks reasoning part IDs and replaces all associated
stream chunks and metadata with content-free `reasoning_suppressed` events.
This keeps hidden chain-of-thought out of events, artifacts, logs, and traces.

Agentkit's `Delta::CommitPart` does not carry the `PartId` from `BeginPart`.
The mapper consequently retains suppressed IDs for the life of a mapper;
part IDs must be unique within that stream. A fresh mapper is used for each
model stream.

## Usage

| Agentkit field | Kit field | Rule |
| --- | --- | --- |
| `tokens.input_tokens` | `input_tokens` | exact value, otherwise `null` |
| `tokens.output_tokens` | `output_tokens` | exact value, otherwise `null` |
| `tokens.reasoning_tokens` | `reasoning_tokens` | count only; reasoning content is never retained |
| `tokens.cached_input_tokens` | `cached_input_tokens` | exact value, otherwise `null` |
| `tokens.cache_write_input_tokens` | `cache_write_input_tokens` | exact value, otherwise `null` |
| unavailable | `uncached_input_tokens` | `null`; not derived because provider inclusion semantics are not stated |
| unavailable | `tool_calls` / `tool_time_ms` | `null`; accounted by Kit's tool boundary instead |
| unavailable | `compute_time_ms` | `null` |
| `cost.amount` | `cost_amount` | exact provider value, otherwise `null` |
| `cost.currency` | `cost_currency` | exact provider currency, otherwise `null` |
| `cost.provider_amount` | `provider_cost_amount` | exact provider display amount, otherwise `null` |
| `usage.metadata` | `metadata` | preserved |

No zero is invented for an unavailable count. In the reverse mapping,
agentkit `TokenUsage` is created only when both required input and output
counts are present, and `CostUsage` only when amount and currency are present.

## Interrupts And Cancellation

`ApprovalRequest` is blocking and maps to Kit's durable approval state with
approval, task, and tool-call correlation where agentkit provides them.
`AwaitingInput` maps to durable `waiting_for_input`; agentkit marks it
cooperative, so calling `next()` again is allowed, but Kit does not treat that
as durable resolution. `AfterToolResult` is a non-blocking safe yield before
the next model call and records session, turn, and transcript length. Agentkit
has no auth interrupt variant in this snapshot; auth waits belong to Kit's
capability/MCP durable boundary and therefore have no fabricated agentkit
field.

Cancellation is generation-based rather than an enum. An absent
`TurnCancellation` maps to `unavailable` with null generations. A checkpoint
whose observed generation is unchanged maps to `active`; a changed generation
maps to `cancellation_requested`. Cancellation is cooperative and does not
prove that a dispatched provider or tool effect did not occur. Kit records
`cancelled` only for work known not to have completed; uncertainty after
dispatch is `outcome_unknown`.

## Durability And Restart Limits

`LoopObserver::handle_event` and
`TranscriptObserver::on_transcript_event` return `()`. They are synchronous,
infallible, telemetry/replication notifications only. Kit must not acknowledge
a durable transcript append, intent, outcome, approval, auth decision, or
checkpoint from either observer callback.

Kit-owned `ModelAdapter` and `ToolExecutor` wrappers are durable effect
boundaries: commit intent before dispatch and commit the outcome, including an
explicit unknown outcome, before returning control to `LoopDriver`. Mutator
and capability wrappers obey the same ordering. One active Kit attempt owns
exactly one `LoopDriver`; a replacement driver always belongs to a new attempt.

Safe restart is limited to committed non-compacted boundaries and durable
waiting states: before dispatch, after a committed model/tool outcome, after a
complete transcript append, `AwaitingInput`, a committed approval wait, and
the `AfterToolResult` yield before the next model call. Provider streams,
in-flight tools, detached tasks, unresolved process-local approvals, and auth
operations cannot be reconstructed from `LoopSnapshot`. Before restart, an
uncertain dispatched operation must be committed as a durable error result or
`outcome_unknown`; it must not be blindly repeated or reported as success.

The reviewed loop invokes mutators and only then validates transcript pairing.
It has no fallible post-validation checkpoint-promotion hook. Semantic
compaction is therefore disabled at this boundary. Only independently
validated, versioned structural mutation is safe until agentkit provides a
hook that atomically promotes the validated transcript before the next model
call.

## Dependency Needs

The integration owner must add these exact direct path dependencies without
enabling provider, reporting, compaction, MCP, ACP, Runlet, TOON, or default
umbrella features:

```toml
agentkit-core = { version = "=0.10.2", path = "vendor/agentkit/crates/agentkit-core" }
agentkit-loop = { version = "=0.10.2", path = "vendor/agentkit/crates/agentkit-loop" }
agentkit-tools-core = { version = "=0.10.2", path = "vendor/agentkit/crates/agentkit-tools-core" }
```

These three crates define the mapped core model, loop/transcript/interrupt
surface, and typed approval reasons. None declares Cargo features. The
existing root `serde` derive and `serde_json` dependencies are also used.

## Provider TLS And License Attestation

The production Anthropic, Ollama, OpenAI, and OpenRouter adapters require
Agentkit's shared `reqwest 0.13.2` dependency with its `rustls` feature. That
feature selects `rustls-platform-verifier 0.6.2`: supported native targets use
the operating system trust facilities or CA bundle, while wasm, which has no
native certificate store, uses `webpki-root-certs 1.0.9` as its Mozilla root
set. Removing rustls would remove HTTPS from the real providers; changing to
native-tls would replace the existing rustls stack and modify the pinned
Agentkit snapshot without removing a necessary trust source on wasm.

The cached crate declares `CDLA-Permissive-2.0`, and its `LICENSE` SHA-256 is
`e271993808fec50ab29350b39539cdec611a9103f827e0aa26d61da70e2d33f8`.
The verified text permits use, modification, and sharing, imposes no
restriction on results, and requires only that the agreement text accompany
shared data. It is permissive and compatible with Kit distribution provided
that packaged root data retains that license text. `deny.toml` therefore has
an exception limited to package `webpki-root-certs =1.0.9`; the license is not
allowed globally. Cargo's package checksum remains pinned in `Cargo.lock`.

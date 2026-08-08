# M006 Six-Surface Injection Local Verification Report

- Unit: `7.14`, M006 gate evidence
- Run date: `2026-08-07`
- Candidate: uncommitted local worktree
- Dashboard evidence: `EV-G06-006` remains **pending/local**
- Gate status: `G06` remains **blocked_external**
- Trusted, production, release, and immutable-candidate evidence: **not claimed**

## Local Paths Exercised

The exact adversarial test reads back six projected surfaces: a service-persisted prompt artifact, a
service-persisted progress event through the run-timeline query, an encrypted durable telemetry
batch, a production-projected composition envelope, a real child-process PTY snapshot, and a native
workspace search result. Canonical clean event bytes remain unchanged. Trace input is constructed
locally and passed through the production telemetry adapter/exporter; composition projection is the
production pre-compiler boundary, but neither path claims a complete external invocation.

MCP tool, resource, prompt, log, sampling, form, and roots-shaped normalized payloads cross the same
artifact/model projection API. The configured MCP tool/resource/prompt transport result path also
uses custody before artifact sealing and presentation. This local test does not cause an external MCP
request or claim interoperability with an external MCP implementation.

Each corpus ingress has a distinct synthetic canary. The six named exports must contain their own
surface pointer and injection instruction, must contain a redaction marker or opaque prompt secret
reference, and must contain no reconstructable active canary. Scanner-level positive controls cover
raw, hexadecimal, standard and URL-safe base64, whitespace-interleaved base64, percent, JSON Unicode
escape, and ANSI/control-interleaved forms across chunk boundaries. The scanner also checks the
complete ordered aggregate envelope. These encoded controls do not each traverse all six production
paths.

The authority section runs one inert baseline and six independent normal `RunExecutor`
conversations. The reactive fake provider can request the production `kit_run` tool only when the
projected transcript contains an injection instruction and `kit_run` appears in its provider tool
specs. Provider specs are authority-filtered, so the configured but unauthorized binding is
undiscoverable and the reactive path produces no tool request. Every injected run records zero
durable effect intents, broker denials, kernel intents, kernel outcomes, effect dispatches, and kernel
dispatches; the effective-config reference and authoritative grant snapshot remain unchanged.

This pre-intent discovery rejection is distinct from kernel enforcement of a visible but
unauthorized route. `EV-7.14-C-201` remains bound to the exact existing selector
`broker_paths::broker_auth_is_bound_durable_and_precedes_kernel_effects`, which proves the latter
broker/kernel authority path. The six-surface test does not claim one broker denial per surface,
replace that integration evidence, or claim the equivalent MCP tool route.

## Current Local Evidence

| Command | Result |
| --- | --- |
| `cargo test --locked --test adversarial six_surface_injection::six_surface_injection -- --exact --test-threads=1 --nocapture` | `exit 0`; exactly 1 passed, 0 failed, 0 ignored; each injected surface logged `provider_tool_visible=false durable_intent=0 broker_denial=0 kernel_intent=0 kernel_denial=0 effect_dispatched=0 kernel_dispatched=0`; command exits nonzero on assertion failure |
| `cargo test --locked --lib telemetry::redact::scanner_tests -- --test-threads=1` | `exit 0`; exactly 11 current scanner tests passed, 0 failed, 0 ignored, including JSON surrogate-pair reconstruction, terminal saturation, post-saturation secret and repeated pushes, reset, and a valid 8 MiB stream; command exits nonzero on regression |
| `cargo test --locked --lib domain::secret::custody_tests -- --test-threads=1` | `exit 0`; exactly 12 current projection/custody tests passed, 0 failed, 0 ignored, including marker fixed-point safety, the maximum cursor-state bound, nested key/value splits under formerly trusted fields, and independent event-page boundary safety |
| `cargo test --locked --test conformance repo_discover -- --test-threads=1` | `exit 0`; 25 current focused repository tests passed, 0 failed, 0 ignored, including projected artifact resolution and source continuation metadata |
| `cargo test --locked --test conformance broker_paths::broker_auth_is_bound_durable_and_precedes_kernel_effects -- --exact --test-threads=1` | `exit 0`; exactly 1 passed, 0 failed, 0 ignored for the separate `EV-7.14-C-201` visible-route proof; command exits nonzero on regression |
| `cargo check --locked` | `exit 0` |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | `exit 0` |
| `cargo fmt --all -- --check` | `exit 0` |
| Registry generation/check, aggregate and coverage governance lint, dashboard lint, and `git diff --check` | all `exit 0`; 5 projections from 1038 records, 0 governance findings, 0 unmapped source lines, 12 valid dashboards with 116 exit-evidence bullets |

Registry evidence `EV-7.14-C-101` and `EV-7.14-C-201` remains pending with null artifact and
environment digests and null versions. Local command output is current developer evidence only.

## Pending External Evidence

No external model/provider, external MCP server, separate hostile sandbox, Linux deployment,
clustered topology, production daemon topology, canary deployment, immutable candidate attestation,
or trusted release job was exercised. Composition projection is local and in-process; it does not
claim M007 or `G07` completion. These paths remain pending and `G06` remains `blocked_external`.

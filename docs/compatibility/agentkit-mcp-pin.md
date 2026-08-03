# Agentkit MCP Protocol Revision Pin

- Work package: `M006-W07a`
- Agentkit release: `0.10.2`
- Patch ID: `m006-mcp-protocol-revision-pin`
- W07a snapshot SHA-256: `0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`
- Previous snapshot SHA-256: `cbcb095e304bbd2188524d0f1131061746aba5c3e592612b69371c73cf0c16c2`
- Current snapshot SHA-256: `3cc4569be6990cd88265f9e3d5d2c057c1cfd4eefad5da4ff0ece4150d758077` (local-only `M006-W07b` overlay)
- Upstream status: pending submission; upstream acceptance is non-blocking
- Blocker closed locally: `BLK-05` (`docs/decisions/PRE-0005-protocol-revisions.md`)

## Pin Contract

`agentkit-mcp` owns an explicit adapter constant,
`PINNED_PROTOCOL_VERSION = ProtocolVersion::V_2025_11_25`, and advertises it on
every rmcp `initialize`. The former upstream floating alias
(`rmcp_model::ProtocolVersion::LATEST`) no longer appears in any production
path; the only remaining reference is the drift-canary test that fails when an
rmcp upgrade moves the alias away from `2025-11-25`, forcing a deliberate
re-pin decision.

## Enforcement

Both transport connect functions terminate in one shared contract:
`enforce_pinned_protocol_version` (transport-independent) applied by
`enforce_negotiated_protocol_version` to every freshly initialized rmcp
service. A negotiated revision unequal to the pin — or a missing `initialize`
result — returns the typed `McpError::UnsupportedProtocolVersion` (server id,
expected pin, offending negotiation) and closes the transport first, so no
capability, discovery, catalog, or auth state ever exists for a refused
connection. Covered paths:

- initial stdio connect (`connect_rmcp_stdio`);
- initial Streamable HTTP connect (`connect_rmcp_streamable_http`);
- every reconnect, including auth-resolution reconnects (`reconnect_inner`
  re-enters the same two functions);
- `McpServerManager` connect/refresh paths, which build connections only
  through the functions above.

`McpConnection::negotiated_protocol_version` exposes the negotiated revision;
connect-family constructions are guaranteed to report the pin. The vendored
transport configuration caps SSE reconnect attempts and channel capacity, and
disables rmcp's transparent expired-session reinitialize because that hidden
handshake would bypass the adapter's negotiated-revision refusal.

`McpConnection::connect_kit_authorized_transport` is a cross-crate adapter seam
that accepts transport I/O, never a command. Kit does not expose arbitrary I/O,
process, or parts constructors: its stdio launcher accepts only an
executor-issued `PreparedCommandToken` and submits it to an injected
`OwnedStdioProcessService`. That durable executor service retains launch
custody, process-tree recovery ownership, and close/reap responsibility. If the
runtime service is unavailable, stdio fails closed with
`OwnedProcessUnavailable`; this worktree claims no live Kit stdio launch
evidence. The `kit-authorized` feature removes the public command/default-client
`McpConnection::connect*`, adopted-service, and manager entry points and adds
`connect_authorized_http`, which rejects configurations without an injected
HTTP client. AgentKit's examples and integration tests opt into the separate
`unmediated-dev` feature; Kit's root dependency enables only `kit-authorized`,
so workspace `--all-features` does not alter the production feature set.
Streamable HTTP remains rmcp's protocol worker but uses Kit's
custom `McpHttpClient`: exact protocol/session/resume headers, bounded
JSON/SSE/header/session/DNS-result inputs, finite async credential, DNS, and I/O
deadlines, disabled redirects, pinned
policy-authorized addresses, actual-peer validation on every response, and a
fresh operation-bound opaque-handle credential lease per request.

M006-W07b directly uses `agentkit-mcp` only behind `ReadyConnection`. That type
does not expose the raw connection: list/tool/resource/prompt operations must
present a current broker envelope whose method and JSON arguments match the
wire request. Intent, dispatch, and completed/auth-interrupted/outcome-unknown
states are durable. Challenge plus `AuthInterrupted` is one transaction, and a
resolved 401/403 replay reservation is the replay dispatch transaction itself;
unknown outcomes require reconciliation and are never replayed automatically.
Expired HTTP sessions recover only through the explicit broker-authorized
`reinitialize_expired_session` path; the interrupted operation remains
`outcome_unknown` and is never transparently replayed.

## Tests

The W07a `crates/agentkit-mcp/tests/protocol_revision_pin.rs` snapshot contains
13 tests and its focused package suite passed 43/43. The W07b overlay adds
`http_reconnect_replaces_server_capabilities`, so the current file has 14 tests
and the current focused package suite passes 44/44:

- exact-revision acceptance over real stdio (python3 child process) and real
  Streamable HTTP (axum mock), including `negotiated_protocol_version`
  equality with the pin;
- refusal of the known older revisions `2025-06-18`, `2025-03-26`,
  `2024-11-05` at both call sites with the typed error, zero discovery
  requests reaching the refused server, and HTTP session `DELETE` observed on
  refusal;
- missing `protocolVersion` refusal at both real call sites with
  `negotiated: None`;
- reconnect refusal after a server downgrades between negotiations;
- reconnect replacement of the complete negotiated server capability set;
- missing-negotiation refusal;
- a 1200-value generated corpus of unequal revisions driven through the
  shared contract both call sites terminate in;
- serialized pin equals `2025-11-25` exactly, and the upstream-alias drift
  canary.

`tests/dynamic_http_client.rs`'s mock now negotiates the pinned revision; its
prior `2024-11-05` reply is exactly what the patch refuses.

Kit's focused transport tests additionally cover the executor-owned stdio
adapter, typed missing-version refusal, exact initialize/initialized arguments,
logical response deadlines, retry-never, aggregate
headers, POST/GET/DELETE expiry, invalid-UTF-8 SSE replacement, endpoint and
capability confusion, exact HTTP headers, pinned peers, resume IDs, oversized
SSE refusal, 401/403 typing, and egress authorization. The 5 registered
`mcp_transport` conformance tests include a Rust-AST architecture assertion and
negative fixture that permit raw AgentKit MCP/rmcp construction, operations,
and re-exports only in `src/protocols/mcp/transport`. The 13 broker
tests include restart persistence, grant/scope/cancellation drift, atomic auth
crash windows, durable dispatch outcomes, and one atomic replay dispatch.

The architecture assertion and `publish = false` are a repository
code-integrity boundary, not a runtime defense against a contributor who can
modify and rebuild trusted source. Rust has no cross-crate friend visibility;
the small public AgentKit methods required by Kit remain callable by another
crate that deliberately enables `kit-authorized`. Kit's review and static test,
not type privacy, enforce that final cross-crate edge.

## Dependency Pins

The vendored MCP crate declares exact direct transport versions: `rmcp
1.5.0`, `http 1.4.0`, and `sse-stream 0.2.1`. `Cargo.lock` is unchanged from
the captured vendor snapshot. Kit's root manifest pins its direct
`agentkit-mcp`, `rmcp`, `reqwest`, `http`, `bytes`, and `tokio-util` edges and
records the resulting root lock digest in `build-manifest.yaml`.

The policy-owned pinned connector and actual-peer validation are the minimum
W08 scope pulled forward because Streamable HTTP could not safely carry a
credential through the system/default resolver. Redirect support remains
disabled rather than partially authorized.

## Snapshot Accounting

W07a changed payload file count 356 -> 357 by adding the protocol test file and
produced aggregate `0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`.
W07b keeps 357 payload files and modifies exactly the MCP library and that test
file, producing current aggregate
`3cc4569be6990cd88265f9e3d5d2c057c1cfd4eefad5da4ff0ece4150d758077`.
Both overlay steps and their exact before/after file hashes are recorded in
`src/protocols/mcp/agentkit_patch/manifest.yaml`; the snapshot manifest,
metadata, pin document, and build manifest carry the current aggregate.
`vendor/agentkit/Cargo.lock` is unchanged; no new vendor dependency was added.

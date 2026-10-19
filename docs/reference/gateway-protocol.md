# Gateway ACP HTTP protocol reference

Use this reference to implement an ACP adapter for a trusted Kit host. For host setup, terminal use, recovery, and safety guidance, start with the [gateway task guide](../user/gateway.md).

## Connect over HTTP/SSE

An ACP v2 HTTP client can connect directly to `http://HOST:7766/acp/v2`.
Configure its HTTP client with `Authorization: Bearer TOKEN` on every request,
including event streams and transport teardown. The initialize POST returns an `Acp-Connection-Id` header; include it on subsequent requests. Open a connection-level SSE GET with `Accept: text/event-stream`, then a separate SSE GET for every attached session with both `Acp-Connection-Id` and `Acp-Session-Id`. All requests use the same `/acp/v2` endpoint: there is no separate Kit session REST API. Open the session stream before a resume or prompt; for a new session, open it after receiving the new `sessionId`. Established POSTs return `202 Accepted`; correlated replies and notifications arrive on the appropriate SSE stream. A DELETE with the connection ID tears down that transport without canceling accepted session work.

This is a manually provisioned
bearer credential, not an OAuth login. Use the SDK's exact-endpoint constructor
(for the Rust SDK, `HttpClient::with_endpoint_and_client`) so it does not append
its default `/acp` path.

The supported wire protocol is ACP v2, using the pinned ACP Rust SDK 2.0.0. The supported integration transport is HTTP/SSE. ACP v1 and arbitrary ACP extensions are not supported; the selected bounded SDK path rejects WebSocket upgrades. The bundled TUI bridge and gateway should use the same Kit version; compatibility with future SDK or Kit versions is not promised by this experimental interface.

Start with `initialize`, then use standard `session/new`, `session/list`, and
`session/resume` requests. `session/new.cwd` must be an exact canonical root
approved with `--project`. Listing returns standard `sessions` entries with
`sessionId`, `cwd`, and `title`; Kit's `_meta["kit.gateway.state"]` identifies `resident`,
`restorable`, or `exited` entries. Resident means the gateway still owns the session,
including brief close-response retention; it does not guarantee prompt admission or
completed work. Restorable means a durable transcript is available. An `exited`
entry has no catalog row to restore and can appear transiently when its actor exits
during listing. Listing or creating sessions reclaims completed actor entries;
durable transcripts remain listed as restorable. Reclamation does not repair stale
transcript locks left by abrupt child termination.

Request `"replayFrom": {"type": "start"}` in `session/resume` to receive history as ACP
session notifications. Resuming without requesting history is not a full replay.
The bundled bridge maps `--remote-session` to this standard resume request and
requests replay from the start. Graceful DELETE stops new submissions and drains HTTP-accepted inbound frames before releasing control. It does not wait for inference to finish or guarantee outbound delivery. Later POSTs receive 410 while draining. Only confirmed clean adapter completion returns 202; a 30-second waiter deadline or drain failure returns 503. A timeout or canceled DELETE waiter does not reopen admission or abort accepted work: connection-owned draining continues, and another DELETE can join it while the connection remains registered. After removal, later requests receive 404. The bundled client's best-effort DELETE wait is capped at five seconds, so client exit alone is not confirmation of remote teardown. Submission outcomes can remain unknown: do not blindly resubmit work. Ending a transport connection is not a session cancellation or deletion. A single
ACP connection can control multiple sessions. Replacing one controller settles
that controller's pending requests with errors without detaching its other sessions.

### Supported operations

| Operation | Gateway behavior |
| --- | --- |
| `initialize` | Establish ACP v2 capabilities once per connection. |
| `session/new` | Create a child in an approved host project and acquire control. |
| `session/list` | List resident and restorable sessions; optional exact `cwd` filtering, no pagination cursor. |
| `session/resume` | Acquire or replace control of a session; optional replay from `start` only. |
| `session/prompt` | Submit work to the resident child. |
| `_kit/session/update_mcp` | Add or update the current controller's client-owned MCP servers; see [session MCP configuration](../user/mcp.md#acp-session-mcp-configuration). |
| `session/inject`, `session/replace_inject`, `session/revoke_inject` | Enqueue, replace, or revoke pending steering using the host-advertised ACP capability. |
| `session/close` | Close the resident session and its outstanding work; retain the transcript for explicit resume. The close response follows child cleanup. A cleanup error is reported, but does not reopen a partially closed child; resume is an explicit new restoration. |
| `session/cancel` | Interrupt the current response; ordinary top-level sessions retain detached background jobs. This is distinct from closing a connection. |
| `kit/background/cancel` | Request cancellation of one detached call using the existing Kit extension. |
| `session/set_config_option` | Select configuration advertised by the child, without changing client-local defaults. |

Unsupported requests fail explicitly. Client-to-agent request cancellation (`$/cancel_request`) does not interrupt accepted session work. Integrations must use `session/cancel` for that purpose, and cancel detached jobs separately. Do not automatically resend a prompt after a transport failure.

The Kit-private background operation uses **snake_case** parameters, unlike standard ACP session methods:

```json
{"jsonrpc":"2.0","id":4,"method":"kit/background/cancel","params":{"session_id":"SESSION_ID","call_id":"CALL_ID"}}
```

Only the current controller can send it. The gateway forwards the native response (`{"cancelled":true}`); this acknowledges the cancellation request, not completion or proof that the call was running. The native operation also accepts unknown call IDs and repeated requests. Observe subsequent task updates to confirm completion. Neither this operation nor `session/cancel` is a gateway-wide “stop all.”

<a id="pinned-wire-fixture"></a>

### Implement an ACP HTTP client

Use these request and response examples when implementing a direct ACP HTTP client. They describe the supported ACP v2 interface; use matching Kit versions while the gateway is experimental.

Initialize POST body (send `Authorization: Bearer TOKEN` and `Content-Type: application/json`):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"generic-http-fixture","version":"1"},"capabilities":{}}}
```

The response is HTTP 200 JSON with `Acp-Connection-Id`. Its `result` contains `protocolVersion: 2`, `info.name: "kit-gateway"`, the running Kit version, and these capabilities:

```json
{"session":{"prompt":{"image":{},"audio":{},"embeddedContext":{}},"inject":{"modes":["steer"],"steerInStream":["finish"],"pending":{"replace":true}},"list":{},"mcp":{"stdio":{},"http":{},"_meta":{"kit/updateMcp":{"method":"_kit/session/update_mcp","upsert":true}}}}}
```

Its `result._meta` explicitly identifies the experimental limits:

```json
{"kit/gateway":{"experimental":true,"transport":"bounded-http","maxFrameBytes":1048576,"coreBufferedBytesPerDirection":16777216,"httpEgressBytesPerConnection":4194304,"liveReplayLimitBytes":8388608,"liveReplayLimitEvents":4093}}
```

Both connection and session SSE GETs return HTTP 200 with `Content-Type: text/event-stream`. The session stream echoes both ACP ID headers. Accepted subsequent POSTs and successful transport DELETE return HTTP 202; DELETE failures follow the rules above. SSE `data:` contains JSON-RPC replies or notifications, for example:

```text
data: {"jsonrpc":"2.0","id":3,"error":{"code":-32002,"message":"Resource not found","data":{"reason":"unknown_message_id","messageId":"not-pending"}}}

```

The error above illustrates a session-routed revoke of an unknown pending message; clients should inspect the code and structured data rather than matching diagnostic text. IDs and credentials in examples are placeholders, not usable secrets.

## Close a resident session

Send `session/close` as the current controller, not HTTP DELETE:

```json
{"jsonrpc":"2.0","id":5,"method":"session/close","params":{"sessionId":"SESSION_ID"}}
```

The close response follows child cleanup. A cleanup failure returns an error and does not reopen a partially closed child. A successful close also confirms transcript durability after cleanup. Unflushed buffered writes, disabled persistence, or an earlier rejected transcript record cause an error; later storage recovery cannot erase evidence of a lost record. This strict close check applies to ACP v1 and v2 sessions, but the gateway transport accepts only ACP v2. Ordinary transcript delivery remains best effort. See [Close a session](../user/gateway.md#close-a-session) for the distinction between closing, canceling, and detaching.

A persistence failure returns a JSON-RPC `error` with code `-32603`, message `"Internal error"`, and diagnostic string `data` beginning with `"loop error: "`; the response has no `result`. Treat this as a failed close acknowledgement, not proof that cleanup was rolled back. The diagnostic explains the failure for operators; do not match its wording in client logic or use the generic internal-error code alone to identify a storage failure.

Close-response history is retained for at most one 15-second lease after child cleanup, or until the controller detaches. This is not durable response replay. Resume explicitly to restore the transcript; do not automatically resend an uncertain prompt.

<a id="configure-host-side-mcp-servers"></a>

## Configure session MCP servers

For native stdio and Streamable HTTP entries, additive update semantics, and the advertised `_kit/session/update_mcp` extension, see [ACP session MCP configuration](../user/mcp.md#acp-session-mcp-configuration). Resident `session/resume` retains the overlay when `mcpServers` is omitted or `[]`; a nonempty list is rejected with guidance to use `_kit/session/update_mcp`. The explicit update adds new client names or updates submitted client-owned names; omitted names remain and `[]` is a no-op. Host/plugin collisions and duplicate submitted names reject the whole batch. No removal operation is provided. Initial creation and cold restoration accept HTTP entries. Only the current controller can update a resident session. Legacy SSE MCP transport is not supported.

## Transport limits

- Healthy HTTP connections have no cumulative output cap. The SDK instead limits each core direction to 16 MiB / 256 frames and each connection's HTTP egress to 4 MiB / 64 frames. Admission saturation fails explicitly and can terminate the connection; it does not silently drop output while claiming the stream is complete. Accepted resident work is not cancelled by transport failure. There is no delivery acknowledgement, and HTTP body handoff does not prove peer consumption. The bridge's stdin mailbox holds at most 16 capped frames; each frame is limited to 1 MiB before parsing.
- The server admits at most 64 connections, 32 concurrent SDK POSTs, and 8 MiB of aggregate body reservations. Each connection permits at most 128 batch entries, 256 pending routes, 64 session registrations, and 65 streams. HTTP/core frames are limited to 1 MiB; SSE encoding and the client's 1 MiB parser limits can reject a near-limit frame. The bundled client independently caps HTTP reservations at 64 MiB / 128 slots, pending RPCs at 128, each POST lane at 32, and SSE streams at 8. Transport exhaustion is terminal rather than an unbounded wait. The gateway admits at most 32 concurrent POST/DELETE operations.
- These bounds exclude allocator overhead, transient JSON conversion, arbitrary application state, and HTTP/TLS/socket buffers. They are not a hard process-memory ceiling or a denial-of-service-hardening claim. The bounded transport supports HTTP/SSE, not WebSocket upgrades. SSE has no replay cursor: reopening a stream alone cannot recover lost events. Recovery uses fresh initialization and session-level resume, not SSE event IDs. There is no idle connection expiration or body-read deadline: disconnected peers that do not DELETE or otherwise terminate their transport can retain connection slots. Slot exhaustion may require restarting the gateway; do not expose it to untrusted clients.

HTTP POST bodies are limited to 1 MiB, including streamed bodies. Supervisor queues and pending child requests are bounded. Child stdout frames are limited to 8 MiB before JSON parsing; oversized or malformed frames stop that child. There is no fixed resident-session count ceiling; provision host resources independently of transport limits.

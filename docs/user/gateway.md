# Private-network gateway

`kit gateway` supervises ordinary Kit ACP child processes on one trusted host. A remote terminal connects with the same `kit` binary. The gateway owns the child, while terminal attachments own only a transport connection. Closing the terminal or losing the connection does not cancel accepted work.

This is an experimental, single-user, private-network facility, not a public service, sandbox, account system, browser interface, or relay. Use matching Kit versions on both hosts. Direct ACP HTTP access is first class: the gateway does not require kitagent.dev, a hosted account, a relay, or any other Kit control-plane service.

## Start the host

Configure the host's provider credentials, model, tools, and MCP servers as for a local Kit session. Children inherit the gateway host's environment and load its configuration; the remote terminal does not supply provider credentials or local MCP servers.

Create a high-entropy bearer credential without putting it in command-line arguments:

```sh
umask 077
openssl rand -hex 32 > "$HOME/.kit-gateway-token"
kit gateway --credential-file "$HOME/.kit-gateway-token" \
  --project /absolute/server/project
```

The default address is `127.0.0.1:7766`. Repeat `--project` to approve additional directories. The gateway canonicalizes each directory and accepts only exact registered roots, not arbitrary descendants. This restricts session placement, **not tool filesystem access**: prompts and tools retain the host user's normal authority.

Prefer an SSH tunnel or an encrypted private network. For example, forward local port 7766 to the host's loopback listener:

```sh
ssh -N -L 7766:127.0.0.1:7766 host
```

To bind directly to a private interface, supply both `--listen 10.0.0.5:7766` and `--allow-private-network`. Only loopback, RFC1918 IPv4, or IPv6 unique-local addresses are accepted; wildcard and public addresses are rejected. This address check is not a firewall. The server speaks HTTP without TLS: never expose it to an untrusted network. A private IP does not encrypt traffic.

## Create, list, and reconnect

Securely copy the bearer credential to the client, restrict its permissions to the current user (`chmod 600` on Unix), then create a session:

```sh
kit tui --remote http://127.0.0.1:7766 \
  --remote-credential-file "$HOME/.kit-gateway-token" \
  --root /absolute/server/project
```

`--root` is an absolute **server** path; it need not exist on the client. List resident sessions and durable transcripts in approved roots:

```sh
kit gateway list --url http://127.0.0.1:7766 \
  --credential-file "$HOME/.kit-gateway-token"
```

Reconnect using the returned session ID:

```sh
kit tui --remote http://127.0.0.1:7766 \
  --remote-credential-file "$HOME/.kit-gateway-token" \
  --root /absolute/server/project --remote-session SESSION_ID
```

If the child is resident, this attaches to that process. After a gateway restart or child exit, it restores the existing Kit transcript in that project. Restore uses the normal session ownership checks; it does not silently force takeover of another local process. In-flight work does not survive gateway termination; only persisted transcript state can be restored. Disk failures retain Kit's existing durability limitations.

The remote TUI supports normal prompting, model/config selection advertised by the host, steering, and explicit interruption. `Esc` or `Ctrl+C` during a running turn interrupts it. Quitting with `Ctrl+D` on an empty prompt, or losing the connection, detaches without interruption. One attachment controls a session at a time. Clean disconnection releases the attachment. An explicit resume replaces the previous controller, so a lost HTTP stream does not prevent reconnection. The old controller cannot submit new work once replaced.

To start another session, exit and omit `--remote-session` on the next invocation. To switch sessions, exit, list them, and supply the desired ID. In-place `/new`, session listing/resume/rename commands, local provider login/usage, and voice are disabled in a remote attachment. This milestone does not provide remote session deletion or a close command. Unsupported ACP close requests fail explicitly; they are not reported as successful no-ops.

## Standard ACP HTTP clients

An ACP v2 HTTP client can connect directly to `http://HOST:7766/acp/v2`.
Configure its HTTP client with `Authorization: Bearer TOKEN` on every request,
including event streams and transport teardown. The initialize POST returns an `Acp-Connection-Id` header; include it on subsequent requests. Open a connection-level SSE GET with `Accept: text/event-stream`, then a separate SSE GET for every attached session with both `Acp-Connection-Id` and `Acp-Session-Id`. All requests use the same `/acp/v2` endpoint: there is no separate Kit session REST API. Open the session stream before a resume or prompt; for a new session, open it after receiving the new `sessionId`. Established POSTs return `202 Accepted`; correlated replies and notifications arrive on the appropriate SSE stream. A DELETE with the connection ID tears down that transport without canceling accepted session work.

This is a manually provisioned
bearer credential, not an OAuth login. Use the SDK's exact-endpoint constructor
(for the Rust SDK, `HttpClient::with_endpoint_and_client`) so it does not append
its default `/acp` path.

The supported wire protocol is ACP v2, using the pinned ACP Rust SDK 2.0.0. The supported integration transport is HTTP/SSE. ACP v1 and arbitrary ACP extensions are not supported; although the underlying SDK also exposes WebSocket handling, it is not part of this gateway's validated integration contract. The bundled TUI bridge and gateway should use the same Kit version; compatibility with future SDK or Kit versions is not promised by this experimental interface.

Start with `initialize`, then use standard `session/new`, `session/list`, and
`session/resume` requests. `session/new.cwd` must be an exact canonical root
approved with `--project`. Listing returns standard `sessions` entries with
`sessionId`, `cwd`, and `title`; Kit's `_meta["kit.gateway.state"]` distinguishes `resident` from
`restorable` entries. Resident means the gateway still owns a live child;
restorable means a durable transcript is available. An `exited` slot has no catalog
entry to restore and can appear while actor cleanup is finishing. Completed actor
slots are reclaimed when listing or creating sessions; durable transcripts remain
listed as restorable. Reclamation does not repair stale transcript locks left by
abrupt child termination.

Request `"replayFrom": {"type": "start"}` in `session/resume` to receive history as ACP
session notifications. Resuming without requesting history is not a full replay.
The bundled bridge maps `--remote-session` to this standard resume request and
requests replay from the start. Clean SDK transport teardown stops new submissions and drains HTTP-accepted inbound frames through resident dispatch before releasing control; it does not wait for execution to finish or guarantee every outbound event was delivered. If the drain is not confirmed within 30 seconds, DELETE returns 503 without destructive teardown. The submission outcome is then unknown: do not blindly resubmit work. Ending a transport connection is not a session cancellation or deletion. A single
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
| `session/inject`, `session/replace_inject`, `session/revoke_inject` | Enqueue, replace, or revoke pending steering using the host-advertised ACP capability. |
| `session/cancel` | Explicitly interrupt work; this is distinct from closing a connection. |
| `session/set_config_option` | Select configuration advertised by the child, without changing client-local defaults. |

Unsupported requests fail explicitly. Client-to-agent request cancellation (`$/cancel_request`) does not interrupt accepted session work. Integrations must use `session/cancel` for that purpose. Do not automatically resend a prompt after a transport failure.

### Pinned wire fixture

`tests/gateway.rs::pinned_sdk_http_initialize_capabilities_and_session_stream_contract` is an executable HTTP fixture for the SDK pinned at `2f039993d1d6ed8da35b38c31f54a7cbb7338c70`. It validates these payloads and headers against a real gateway without any control-plane service.

Initialize POST body (send `Authorization: Bearer TOKEN` and `Content-Type: application/json`):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"generic-http-fixture","version":"1"},"capabilities":{}}}
```

The response is HTTP 200 JSON with `Acp-Connection-Id`. Its `result` contains `protocolVersion: 2`, `info.name: "kit-gateway"`, the running Kit version, and these capabilities:

```json
{"session":{"prompt":{"image":{},"audio":{},"embeddedContext":{}},"inject":{"modes":["steer"],"steerInStream":["finish"],"pending":{"replace":true}},"list":{}}}
```

Its `result._meta` explicitly identifies the experimental limits:

```json
{"kit/gateway":{"experimental":true,"connectionOutputLimitBytes":8388608,"connectionOutputLimitScope":"lifetime","terminalOutputAllowanceBytes":4096,"liveReplayLimitBytes":8388608,"liveReplayLimitEvents":4094}}
```

Both connection and session SSE GETs return HTTP 200 with `Content-Type: text/event-stream`. The session stream echoes both ACP ID headers. Subsequent POST and transport DELETE return HTTP 202. SSE `data:` contains JSON-RPC replies or notifications, for example:

```text
data: {"jsonrpc":"2.0","id":3,"error":{"code":-32002,"message":"Resource not found","data":{"reason":"unknown_message_id","messageId":"not-pending"}}}

```

The error above illustrates a session-routed revoke of an unknown pending message; clients should inspect the code and structured data rather than matching diagnostic text. IDs and credentials in examples are placeholders, not usable secrets.

## Failure and security boundaries

- Every HTTP operation, including listing, requires the bearer token. Possession grants control over all approved projects and their sessions. There are no separate users, permissions, or per-session ACLs.
- The credential is read from a regular file and must not be accessible to group or others on Unix. Authentication compares the token in constant time. Rotation requires restarting the gateway and updating clients. Do not log or share it.
- Client redirects are disabled so the bearer token cannot follow a redirect. Use a trusted URL and transport; HTTPS can be provided by a separately secured proxy, not by this command itself. The SDK client has an unbounded SSE parser and receive mailbox, so a malicious selected gateway can exhaust client memory.
- A failed send can have an **unknown outcome**. There is no automatic prompt retry. Reconnect and inspect the transcript before resubmitting, or you may duplicate work. A lost create response can leave a resident session; list sessions before creating another.
- Live reattachment replays the resident child's notifications and current configuration, not old request responses. Replies are scoped to the attachment that submitted the request, so a stale response cannot resolve a new terminal's request ID. ACP v2 state-update notifications carry active/idle state across reconnects.
- Replay is an in-memory convenience, not a second durable session format. It is limited to 8 MiB and approximately 4,096 events per session/attachment. Overflow expires an attachment or rejects a full live reattach rather than silently claiming complete replay. For large histories, reconnect with `session/resume` and omit `replayFrom` (or set it to null) to regain control without replay. The bundled TUI requests full replay and can therefore fail to reconnect beyond these limits; a no-replay ACP client is required in that case. Restarting the gateway does not guarantee oversized full replay will fit.
- Each HTTP connection also has a total 8 MiB output safety budget because the pinned SDK uses unbounded mailboxes. Startup diagnostics and the initialize response's `_meta["kit/gateway"]` advertise this experimental lifetime limit; it is not outstanding-byte accounting or backpressure. Exceeding that lifetime budget closes the transport, even when the client reads promptly. The gateway reserves one additional terminal frame of at most 4 KiB to explain exhaustion using a correlated error or session notice before closure. Delivery is best-effort after network loss. Reconnect without replay to regain control; large histories can exceed the budget again, so complete replay is not guaranteed beyond these limits. This budget does not cancel accepted child work and excludes HTTP/SSE framing overhead.
- At most 64 active or stopping actors occupy session slots. Listing or creating sessions reclaims completed actor slots without restarting the gateway or deleting durable transcripts. Detached actors, including idle actors, are not automatically terminated; accepted work continues. HTTP POST bodies are limited to 1 MiB before the SDK handler, including streamed bodies; supervisor command queues and pending child requests are also bounded. Child stdout frames are limited to 8 MiB before JSON parsing; oversized or malformed frames stop that child. The pinned SDK still has unbounded inbound/mailbox and SSE-decoder surfaces, and concurrent connections are not capped. This is not a denial-of-service-hardened public endpoint.
- Graceful `Ctrl+C` shutdown stops serving, releases resident actor ownership, closes each ACP child’s input, drains its output, and waits for cleanup and exit. A 10-second timeout falls back to killing and reaping the child. That fallback can leave a stale transcript lock requiring operator intervention. Hard process termination has operating-system-dependent cleanup behavior; arbitrary tool descendants are not a managed process group.
- Client-side ACP services, arbitrary extra directories, remote MCP injection, browser callbacks, and arbitrary ACP methods are not supported. Files, tool execution, and provider authentication belong to the gateway host.

The internal `gateway bridge` stdio command is a transport adapter for the bundled TUI, not a stable public protocol or general-purpose ACP proxy. The public transport is ACP HTTP at `/acp/v2`, implemented by the ACP SDK rather than a Kit-specific RPC protocol.

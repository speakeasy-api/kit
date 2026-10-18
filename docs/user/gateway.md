# Private-network gateway

Use `kit gateway` to run Kit on a trusted host and control it from a remote terminal. Closing the terminal or losing the connection does not cancel accepted work. Reconnect to the same session to inspect results and continue.

This is an experimental, single-user, private-network facility, not a public service, sandbox, account system, browser interface, or relay. Use matching Kit versions on both hosts. Direct ACP HTTP access is first class: the gateway does not require kitagent.dev, a hosted account, a relay, or any other Kit control-plane service.

## Start the host

Configure the host's provider credentials, model, tools, and MCP servers as for a local Kit session. Children inherit the gateway host's environment and load its configuration; the remote terminal does not supply provider credentials.

ACP clients may supply session-scoped stdio or Streamable HTTP `mcpServers` when creating or restoring a session. Stdio commands and HTTP requests run on the gateway host, not on the client. Each ACP v2 stdio entry needs `"type":"stdio"` and an absolute host executable path.

On resident reattach, omit `mcpServers` or send `[]` to retain the current client-supplied servers. A nonempty list is rejected; use `_kit/session/update_mcp` to add or update client-owned servers explicitly. Omitted servers remain registered and `[]` is a no-op. Client entries cannot overwrite host or plugin names. Session-scoped MCP configuration is not persisted in the transcript, so supply it again when restoring a stopped session. See [ACP session MCP configuration](mcp.md#acp-session-mcp-configuration) for HTTP entries and live add/update requests.

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

To bind directly to a private interface, supply both `--listen 10.0.0.5:7766` and `--allow-private-network`. This flag permits explicit RFC1918 IPv4 or IPv6 unique-local addresses, not wildcard addresses. Loopback is always permitted.

For an isolated container or another deployment that requires listening on all interfaces, explicitly supply `--allow-wildcard-bind` with `--listen 0.0.0.0:7766` or `--listen '[::]:7766'`. This flag permits only unspecified addresses; it does not permit explicit public, link-local, or multicast addresses, nor does it replace `--allow-private-network` for explicit private addresses. Neither flag changes the default loopback listener. Wildcard binding can expose the gateway on public interfaces (IPv6 dual-stack behavior depends on the host). Configure firewall rules and container port publishing to restrict access before starting it.

Every bind mode requires `--credential-file`, a valid bearer token, and owner-only credential-file permissions on Unix (`chmod 600`). HTTP requests still require bearer authentication. These address checks and credentials are not a firewall or encryption. The server speaks HTTP without TLS: never expose it to an untrusted network. Prefer an SSH tunnel or encrypted private network; a private IP does not encrypt traffic.

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

### Automatic recovery and uncertain requests

After a transient transport failure, the TUI stays open and shows **Reconnecting** while it tries to resume the same confirmed session. A successful recovery replaces the visible transcript with replayed history and restores current configuration and turn state. A failed attempt leaves the previous view intact.

Recovery does **not** retry prompts, session creation, configuration changes, steering, or cancellation. A submitted operation can have an **unknown outcome** after a lost response. Inspect the recovered transcript and state before deciding what to send next; retained composer text is not evidence that its earlier submission was rejected. New submissions during recovery are rejected rather than saved for automatic execution. If creation may have succeeded but no session ID was received, use `kit gateway list` and select `--remote-session` explicitly; the bridge never creates another session to guess the outcome.

Authentication, protocol, project, and replaced-controller errors stop automatic recovery. Automatic recovery does not take control back from a newer explicit attachment. Repeated transient failures exhaust a finite attempt/time budget and leave a visible disconnected state with recovery guidance.

### Recovering without historical replay

A full history that exceeds replay or transport limits is not silently truncated, and automatic recovery never silently switches to no-replay. To explicitly regain an existing session without its historical transcript, restart with:

```sh
kit tui --remote http://127.0.0.1:7766 \
  --remote-credential-file "$HOME/.kit-gateway-token" \
  --root /absolute/server/project --remote-session SESSION_ID \
  --remote-no-replay
```

This option requires an existing session ID. The TUI marks **History unavailable**; an empty historical pane is not a claim that the session has no history. The attachment still requires the authoritative current configuration and active/idle state before enabling control. The durable transcript stays on the host, and in-flight work is not restarted. Direct ACP clients can request the same no-replay behavior by omitting `replayFrom` from `session/resume`; full replay uses `{"type":"start"}`.

To start another session, exit and omit `--remote-session` on the next invocation. To switch sessions, exit, list them, and supply the desired ID. In-place `/new`, session listing/resume/rename commands, local provider login/usage, and voice are disabled in a remote attachment. Remote session deletion and a close command are not supported. Unsupported ACP close requests fail explicitly; they are not reported as successful no-ops.

## Standard ACP HTTP clients

Use an ACP v2 HTTP/SSE client with the exact endpoint `http://HOST:7766/acp/v2`, a bearer credential on every request, and matching Kit versions. There is no separate Kit session REST API. See the [gateway protocol reference](../reference/gateway-protocol.md) for connection setup, supported operations, wire examples, and transport limits.

### Supported operations

See [Supported operations](../reference/gateway-protocol.md#supported-operations) for the adapter contract.

<a id="pinned-wire-fixture"></a>

### Implement an ACP HTTP client

See [Implement an ACP HTTP client](../reference/gateway-protocol.md#implement-an-acp-http-client) for request and response examples.

## Failure and security boundaries

- Every HTTP operation, including listing, requires the bearer token. Possession grants control over all approved projects and their sessions. There are no separate users, permissions, or per-session ACLs.
- The credential is read from a regular file and must not be accessible to group or others on Unix. Authentication compares the token in constant time. Rotation requires restarting the gateway and updating clients. Do not log or share it.
- Client redirects are disabled so the bearer token cannot follow a redirect. Use a trusted URL and transport; HTTPS can be provided by a separately secured proxy, not by this command itself. Transport limits do not make an untrusted gateway safe: it still supplies protocol data and can terminate the connection.
- A failed send can have an **unknown outcome**. There is no automatic prompt retry. Reconnect and inspect the transcript before resubmitting, or you may duplicate work. A lost create response can leave a resident session; list sessions before creating another.
- Live reattachment replays the resident child's notifications and current configuration, not old request responses. Replies are scoped to the attachment that submitted the request, so a stale response cannot resolve a new terminal's request ID. ACP v2 state-update notifications carry active/idle state across reconnects.
- Replay is an in-memory convenience, not a second durable session format. It is limited to 8 MiB and approximately 4,096 events per session/attachment, including room for a current snapshot. Overflow rejects a full live reattach rather than falsely claiming complete replay. Use explicit `--remote-no-replay` only when historical omission is acceptable. Current configuration and actual ACP turn state follow historical events before resume completes; durable assistant text alone is never used as evidence that a turn is idle. Restarting the gateway does not guarantee oversized full replay will fit, and burst replay can exhaust transport budgets even below the history limit.
- At most 64 active or stopping sessions can be resident. List sessions to reclaim completed session slots without restarting the gateway or deleting durable transcripts. Creating a session also reclaims completed slots. Detaching does not stop a session, even when it is idle.
- Transport saturation can disconnect a client without canceling accepted work. HTTP frames are limited to 1 MiB; child output frames to 8 MiB. Oversized or malformed child output stops that child. There is no idle connection expiration or body-read deadline: abandoned transports can retain connection slots, and exhaustion may require restarting the gateway. See [Transport limits](../reference/gateway-protocol.md#transport-limits) before sizing an integration. These bounds are not a hard process-memory ceiling or denial-of-service protection.
- Use `Ctrl+C` on the gateway host for graceful shutdown. Kit waits for child cleanup and exit, then forcibly stops a child if it exceeds the 10-second timeout. That fallback can leave a stale transcript lock requiring operator intervention. Hard process termination has operating-system-dependent cleanup behavior; arbitrary tool descendants are not a managed process group.
- Client-side ACP services, arbitrary extra directories, legacy SSE MCP servers, browser callbacks, and arbitrary ACP methods are not supported. Files, tool execution, and provider authentication belong to the gateway host.

The internal `gateway bridge` stdio command is a transport adapter for the bundled TUI, not a stable public protocol or general-purpose ACP proxy. The public transport is ACP HTTP at `/acp/v2`, implemented by the ACP SDK rather than a Kit-specific RPC protocol.

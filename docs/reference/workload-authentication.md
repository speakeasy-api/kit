# Workload assertion authentication

Use workload authentication when a deployment supplies a short-lived JWT in a mounted file instead of a user login or API key. The issuer must accept the clientless RFC 7523 JWT bearer grant for the intended resource. Kit does not register a client, send a client secret, use a refresh token, or open a browser in this mode.

The shared configuration is:

```json
{
  "tokenFile": "/run/identity/token",
  "expectedIssuer": "https://auth.example.com/tenant",
  "tokenEndpoint": "https://auth.example.com/tenant/token",
  "resource": "https://service.example.com/mcp"
}
```

All fields are required. `tokenFile` must be absolute. URLs must use HTTPS without user information or fragments; the token endpoint must share the expected issuer's origin. Kit reads RFC 8414 authorization-server metadata and requires exact matches for the configured issuer and token endpoint before opening the assertion file. Redirects are not followed. For a path-based issuer, metadata is requested at `/.well-known/oauth-authorization-server/<issuer-path>`.

Treat the configuration, its containing directories, and the token mount as deployment-owned inputs. Do not let a model, repository dependency, or untrusted MCP response choose them. Protect the source assertion from unrelated processes. Kit follows symlinks so that atomically rotated projected token volumes work; it checks the opened handle is a regular file and reads at most 8 KiB. It checks compact JWT syntax but does not decode claims or use unverified claims for authorization.

Every exchange reopens the file and sends exactly `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`, `assertion`, and `resource` as a form body. Configure independent resources for MCP and inference; their sessions do not share access-token caches.

## Expiry, retries, and replay policy

Access tokens are kept only in process memory. Kit requires a positive `expires_in` and bearer token type, refreshes before the returned expiry, and caps its cache lifetime at one day. Acquisition is serialized per session and bounded to 15 seconds, including waiting for another acquisition. Missing files, malformed assertions, metadata mismatches, and exchange failures fail closed with redacted errors.

Metadata discovery retries a 503 response once. Token POSTs are not automatically retried because a lost response may already have consumed the assertion's replay identifier. No refresh token from a response is retained or used.

The issuer's replay policy still applies. A projected assertion can remain unchanged between exchanges or across independently scoped MCP and inference sessions. Such deployments require an explicitly configured reusable-assertion policy on the server, or a source that rotates the assertion before each exchange. Kit does not bypass or weaken the server's default single-use policy. An exchange refusal requires checking issuer admission, audience, resource, expiry, and replay configuration; API keys are not a fallback.

## Restoring sessions and process snapshots

Ordinary transcript restoration constructs fresh authentication clients and reacquires workload tokens. No workload assertion or access token is written to Kit's credential store.

A process-memory snapshot is different: it can retain tokens, HTTP pools, and work in flight. Kit does not expose a CLI, signal, or gateway resume hook for such snapshots. Applications must not resume a snapshotted executable with resident authentication state. Restart Kit after restore and restore its transcript instead.

An application embedding the Kit library can integrate `kit::workload_auth::invalidate_after_restore()`. Quiesce requests before the snapshot and keep them quiesced through restore; remount the current assertion source, invoke the hook, and only then admit new requests. The hook invalidates all shared workload token caches and fences acquisitions from the old generation. The application remains responsible for discarding or reconnecting snapshot-resident transports and ensuring no pre-restore request is replayed. Calling the hook without that application lifecycle barrier is not a complete snapshot recovery protocol.

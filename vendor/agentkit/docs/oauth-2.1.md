---
status: draft
---

# OAuth 2.1 support for `agentkit-mcp`

## Goal

Bring `agentkit-mcp` into conformance with the MCP 2025-06-18 authorization
profile so hosts can connect to any spec-compliant remote MCP server without
bespoke token-wrangling glue.

Today the crate surfaces a generic `AuthRequest`/`AuthResolution` interruption
carrying opaque `MetadataMap` credentials, and supports static
`Authorization: Bearer …` headers on HTTP/SSE transports. That is enough for
API-key servers but does not satisfy the spec.

This document defines the work to close that gap.

## What the spec requires

Normative requirements from the MCP 2025-06-18 authorization spec and the RFCs
it incorporates:

| Behaviour                                                                              | Requirement        | Source                                            |
| -------------------------------------------------------------------------------------- | ------------------ | ------------------------------------------------- |
| Bearer tokens in `Authorization` header, every request                                 | MUST               | MCP §Access Token Usage, OAuth 2.1 §5.1.1         |
| Never put tokens in query string                                                       | MUST               | MCP §Access Token Usage                           |
| Parse `WWW-Authenticate` on 401, extract `resource_metadata`                           | MUST               | MCP §Authorization Server Location, RFC 9728 §5.1 |
| Fetch `/.well-known/oauth-protected-resource`                                          | MUST               | RFC 9728                                          |
| Validate returned `resource` matches the request URL                                   | MUST               | RFC 9728 §3.3                                     |
| Fetch `/.well-known/oauth-authorization-server`                                        | MUST               | RFC 8414                                          |
| Validate `issuer` matches issuer identifier used in URL                                | MUST               | RFC 8414 §3.3                                     |
| PKCE with S256 on every authorization request                                          | MUST               | OAuth 2.1 §7.5.2, RFC 7636                        |
| `state` parameter, verified on callback                                                | SHOULD             | MCP §Open Redirection                             |
| Exact redirect URI match (loopback port variance allowed)                              | MUST (server side) | OAuth 2.1                                         |
| Redirect URI is `localhost`/`127.0.0.1` loopback or HTTPS                              | MUST               | MCP §Communication Security                       |
| HTTPS for every AS endpoint                                                            | MUST               | MCP §Communication Security                       |
| `resource` parameter (RFC 8707) on authorize AND token requests                        | MUST               | MCP §Resource Parameter Implementation            |
| Canonical resource URI (lowercase scheme/host, no fragment, usually no trailing slash) | SHOULD             | MCP §Canonical Server URI                         |
| Refresh-token rotation for public clients                                              | MUST (when used)   | OAuth 2.1 §4.3.1                                  |
| Dynamic Client Registration (RFC 7591)                                                 | SHOULD             | MCP §Dynamic Client Registration                  |
| Never forward an MCP-issued token to an upstream API                                   | MUST               | MCP §Access Token Privilege Restriction           |
| Secure token storage                                                                   | MUST               | OAuth 2.1 §7.1                                    |

STDIO transports are out of scope per spec: they "SHOULD NOT follow this
specification, and instead retrieve credentials from the environment."

## Non-goals

- **Being an authorization server.** The crate is a client (MCP client acting
  as OAuth 2.1 client). Servers hosted via `agentkit` that want to authorize
  inbound callers are a separate effort.
- **DPoP / mTLS / sender-constrained tokens.** OAuth 2.1 recommends them but
  MCP does not require them. Out of scope for this pass; leave the metadata
  field (`dpop_bound_access_tokens_required`) observable so we can add later.
- **Device-code or CIBA flows.** Only authorization-code + PKCE.
- **Built-in browser UI.** The crate opens URLs and runs the loopback
  listener, but rendering consent screens is the host's job.
- **Credential persistence policy.** The crate defines a `TokenStore` trait;
  disk/keychain backends live in host code or a future companion crate.

## Design principles

### 1. OAuth is a credential source, not a new control plane

The existing `AuthRequest` / `AuthResolution` / `AuthOperation` types already
model interruptions. OAuth slots in _under_ that surface: when a connection or
call produces a 401, the crate tries to satisfy it via an `OAuthProvider`
before escalating to the host as an `AuthRequired` interruption.

The host only sees an `AuthRequest` when interactive consent is unavoidable
(user must approve in a browser). Everything else — metadata fetch, token
refresh, PKCE — happens inside the transport.

### 2. OAuth is pluggable

Two extremes should both work:

- **Fully automatic.** Host supplies `OAuthClientConfig` with a redirect URI;
  the crate runs discovery, DCR, the browser redirect, and token storage.
- **Fully external.** Host supplies tokens it obtained elsewhere (device SSO,
  credential manager), and the crate just attaches `Bearer <token>` and
  handles 401 → refresh if possible, else interrupt.

Implementation: `trait OAuthProvider` + default built-in implementation, same
pattern as transports.

### 3. Discovery results are cached, not re-fetched per request

RS metadata, AS metadata, and registered client credentials are stable for
the lifetime of the connection. Cache them on `McpConnection` (and
optionally persist via `TokenStore`) so a healthy session does one discovery
pass up front.

### 4. Tokens are typed, not opaque

Introduce `AccessToken { value, expires_at, scope, resource, refresh_token }`
alongside the opaque `MetadataMap` route. The HTTP transport should know
enough to proactively refresh before expiry, not wait for a 401.

### 5. Fail loudly on spec violations

Missing `resource` field, issuer mismatch, non-HTTPS AS endpoints, non-loopback
non-HTTPS redirect URIs — all hard errors. This is the difference between
"OAuth-ish" and "OAuth 2.1". Do not paper over them.

## New types

Proposed additions under `crates/agentkit-mcp/src/oauth/`:

```rust
/// Canonical resource identifier (RFC 8707) for an MCP server.
pub struct ResourceUri(Url);

/// RFC 9728 protected resource metadata.
pub struct ProtectedResourceMetadata {
    pub resource: ResourceUri,
    pub authorization_servers: Vec<Url>,
    pub bearer_methods_supported: Vec<BearerMethod>,
    pub scopes_supported: Vec<String>,
    pub dpop_bound_access_tokens_required: bool,
    // ... remaining RFC 9728 fields preserved as serde_json::Value
}

/// RFC 8414 authorization server metadata.
pub struct AuthorizationServerMetadata {
    pub issuer: Url,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub registration_endpoint: Option<Url>,
    pub jwks_uri: Option<Url>,
    pub code_challenge_methods_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub scopes_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    // ...
}

/// Host-provided OAuth configuration for a given MCP server.
pub struct OAuthClientConfig {
    /// Pre-registered client_id. If None and AS supports RFC 7591,
    /// the crate will register dynamically.
    pub client_id: Option<String>,
    pub client_secret: Option<SecretString>,
    pub redirect_uri: RedirectUri,
    pub scopes: Vec<String>,
    /// Human-readable name sent during dynamic registration.
    pub client_name: String,
    pub token_store: Arc<dyn TokenStore>,
    pub browser_opener: Arc<dyn BrowserOpener>,
}

pub enum RedirectUri {
    /// Spawn a loopback listener on an ephemeral port; path is /callback.
    Loopback { preferred_port: Option<u16> },
    /// Host-supplied HTTPS redirect. Host is responsible for delivering the
    /// callback back to the OAuthProvider.
    Https { url: Url, inbox: Arc<dyn RedirectInbox> },
}

pub struct AccessToken {
    pub value: SecretString,
    pub token_type: String,           // "Bearer"
    pub expires_at: Option<Instant>,
    pub refresh_token: Option<SecretString>,
    pub scope: Option<String>,
    pub resource: ResourceUri,
    pub issued_at: Instant,
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    async fn load(&self, key: &TokenKey) -> Result<Option<AccessToken>, McpError>;
    async fn save(&self, key: &TokenKey, token: &AccessToken) -> Result<(), McpError>;
    async fn delete(&self, key: &TokenKey) -> Result<(), McpError>;
}

#[async_trait]
pub trait BrowserOpener: Send + Sync {
    async fn open(&self, url: &Url) -> Result<(), McpError>;
}

/// Pluggable OAuth driver. Default impl performs authorization code + PKCE.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    async fn token_for(
        &self,
        resource: &ResourceUri,
        scopes: &[String],
    ) -> Result<AccessToken, OAuthError>;

    async fn refresh(
        &self,
        token: &AccessToken,
    ) -> Result<AccessToken, OAuthError>;
}
```

`TokenKey` is `(issuer, resource, scopes_hash)` so tokens are not shared
across resources or scope sets, matching RFC 8707 audience binding.

## Mapping to existing auth plumbing

The crate already has:

- `AuthRequest` / `AuthOperation` / `AuthResolution` (in `agentkit-tools-core`)
- `McpError::AuthRequired(Box<AuthRequest>)`
- `McpServerManager::resolve_auth_and_resume`
- `parse_auth_request` sniffing 401 / -32001 JSON-RPC errors

OAuth 2.1 layers in by:

1. **`OAuthProvider::token_for` runs first.** If it returns a valid token, the
   transport attaches it silently; the host never sees an `AuthRequest`.
2. **Interactive consent escalates as `AuthRequest`.** When the default
   provider needs a browser redirect and either (a) no `BrowserOpener` is
   configured, or (b) the host explicitly opted into manual handling, emit an
   `AuthRequest` with `AuthOperation::McpConnect { … }` and `challenge`
   metadata containing `authorize_url`, `state`, `code_verifier_handle`,
   and `resource`. `AuthResolution::Provided { credentials }` carries
   `{ "code": "...", "state": "..." }`; the provider exchanges the code
   and stores the token.
3. **Refresh is never an interrupt.** If refresh fails, clear the cached
   token and restart the flow as a fresh `token_for` — which may then
   interrupt.
4. **Non-OAuth credentials still work.** `McpAuthConfig` gets a variant:
   - `Static { headers }` (today's behaviour)
   - `Bearer { token }` (preset token, no OAuth dance)
   - `OAuth(OAuthClientConfig)` (new)
   - `Custom(Arc<dyn OAuthProvider>)` (host-owned)

## HTTP transport integration

The HTTP and SSE transports change as follows.

### Request path

1. Before sending, ask `OAuthProvider::token_for(resource, scopes)`.
2. Attach `Authorization: Bearer <token>`.
3. Never put the token in the URL, POST body, or an MCP JSON-RPC field.

### 401 handling

1. Parse `WWW-Authenticate`. Extract `resource_metadata` if present;
   otherwise fall back to `<base>/.well-known/oauth-protected-resource`
   derived from the MCP server URL per RFC 9728 §3.
2. Fetch RS metadata; verify `resource` equals the MCP server's canonical URI.
3. Pick an `authorization_servers[i]`. Fetch `<issuer>/.well-known/oauth-authorization-server`; verify `issuer` matches.
4. Invalidate cached token for this `(issuer, resource, scopes)` key, then
   retry `token_for`. If that returns a fresh token, replay the request
   exactly once. If it still 401s, bubble `McpError::AuthFailed` — do not
   retry in a loop.

### Refresh

Proactive: if `expires_at - now < 60s`, refresh before sending.

Reactive: on 401 with a valid non-expired cached token, assume the token was
revoked; delete and restart.

Public-client refresh-token rotation: always replace the stored refresh token
with the new one from the response (OAuth 2.1 §4.3.1).

## STDIO transport

Unchanged. Env-var credential loading, as today. The OAuth surface is a no-op
for STDIO per MCP spec.

## Canonical resource URI

Construct once per `McpServerConfig` and cache on the connection:

- Lowercase scheme and host.
- Drop default ports (`:443` for https, `:80` for http).
- Drop fragment.
- Drop trailing slash unless the path is just `/`.
- Reject URIs without a scheme or with a fragment.

Expose helper `ResourceUri::from_server_url(&Url) -> Result<Self, McpError>`.

## Dynamic client registration

When `OAuthClientConfig::client_id` is `None` and the AS advertises
`registration_endpoint`, POST to it with:

```json
{
  "client_name": "<config.client_name>",
  "redirect_uris": ["<resolved redirect_uri>"],
  "grant_types": ["authorization_code", "refresh_token"],
  "response_types": ["code"],
  "token_endpoint_auth_method": "none",
  "scope": "<space-joined config.scopes>"
}
```

Persist the returned `client_id` (and `client_secret` if issued — most MCP
flows are public clients so expect `"none"`) in the `TokenStore` under a
separate `ClientRegistrationKey` so we only register once per AS.

If the AS does not support DCR and no `client_id` is configured, fail with
`OAuthError::RegistrationUnavailable` and surface a host-facing
`AuthRequest` whose `challenge` metadata includes the AS's registration URL
(if any) so the host can walk the user through manual registration.

## PKCE

- `code_verifier`: 43–128 char unreserved-charset random string. Default to
  32 random bytes base64url-encoded (43 chars). Use `rand::rngs::OsRng`.
- `code_challenge`: `BASE64URL(SHA256(code_verifier))`.
- Always send `code_challenge_method=S256`. Never fall back to `plain`; fail
  if AS does not advertise `S256` in `code_challenge_methods_supported`.

## Loopback redirect listener

When `RedirectUri::Loopback` is configured:

1. Bind TCP on `127.0.0.1:<preferred_port or 0>`.
2. Serve one GET request matching `/callback?code=…&state=…`.
3. Verify `state` matches the value generated for this authorize call.
4. Return a minimal HTML page ("You may close this window.").
5. Shut down the listener immediately after one successful match or on
   timeout (default 5 min).

Loopback is the default because it matches OAuth 2.1's exact-match rule while
permitting port variance.

## Security checklist (must all be true before declaring support)

- [ ] `Authorization` header only; never URL, body, or RPC field.
- [ ] `resource` parameter on every `authorize` and `token` call.
- [ ] PKCE S256, verifier regenerated per authorize call, never reused.
- [ ] `state` parameter generated per authorize call, verified on callback,
      rejected on mismatch.
- [ ] AS endpoints over HTTPS (reject non-HTTPS unless host is loopback).
- [ ] Redirect URI is loopback or HTTPS.
- [ ] RS metadata `resource` equals the request URI.
- [ ] AS metadata `issuer` equals the issuer identifier used in the URL.
- [ ] Refresh tokens rotated; old value discarded after successful refresh.
- [ ] Tokens never logged. `SecretString` with `Debug` scrubbed.
- [ ] MCP-issued tokens never forwarded to upstream APIs made from inside
      the MCP server (but that is a server-side concern; note it in docs).
- [ ] Token audience bound per `(issuer, resource, scopes)` — no cross-MCP-server reuse.

## Module layout

```text
crates/agentkit-mcp/src/
  oauth/
    mod.rs            # re-exports, OAuthProvider trait
    config.rs         # OAuthClientConfig, RedirectUri, TokenKey
    metadata.rs       # RS metadata, AS metadata, fetch + validate
    discovery.rs      # 401 → WWW-Authenticate → RS → AS pipeline
    registration.rs   # RFC 7591 dynamic client registration
    pkce.rs           # verifier/challenge generation
    flow.rs           # authorize + token exchange (default provider)
    refresh.rs        # token refresh + rotation
    loopback.rs       # one-shot redirect listener
    store.rs          # TokenStore trait + in-memory impl
    error.rs          # OAuthError
    resource_uri.rs   # canonical URI construction
```

Keep the surface behind a `oauth` feature flag in `Cargo.toml`; the
`http` / `sse` features depend on it when OAuth is requested via config.

## Dependencies

Added to `agentkit-mcp`:

- `url = "2"` — URL parsing/canonicalization (already transitive via `reqwest`).
- `sha2 = "0.10"` — S256 challenge.
- `base64 = "0.22"` — base64url encoding.
- `rand = "0.8"` — CSPRNG for verifier/state.
- `secrecy = "0.10"` — `SecretString` wrapper.
- `hyper`/`axum` (or a tiny hand-rolled listener): loopback callback server.
  Prefer a minimal hand-rolled implementation on `tokio::net::TcpListener`
  to avoid pulling a full HTTP framework for a single GET.

Reuse existing `reqwest` for AS/RS metadata, token, and registration calls.

## Flow walkthroughs

### First connect, fresh host

1. `McpConnection::connect_with_auth(config, None)` sends `initialize`.
2. Server returns 401 + `WWW-Authenticate: Bearer resource_metadata="…"`.
3. Transport parses header → fetches RS metadata → fetches AS metadata.
4. `OAuthProvider::token_for` sees no stored token; no stored `client_id`;
   calls `register()` → stores `client_id`.
5. Generates PKCE + state, builds authorize URL including
   `resource=<canonical>`, opens browser, starts loopback listener.
6. User approves. Listener captures `code`. Provider posts to token endpoint
   with `code`, `code_verifier`, `resource`.
7. Receives `access_token` + optional `refresh_token`. Stores via
   `TokenStore`. Returns token.
8. Transport retries `initialize` with `Authorization: Bearer …` → 200.

### Second connect, valid stored token

1. Transport asks `TokenStore` for `(issuer, resource, scopes)`; gets a
   non-expired token.
2. Sends `initialize` with bearer. Server returns 200. No user interaction.

### Token expired mid-session

1. Transport sees `expires_at` within 60s of now → calls `refresh(token)`
   before sending.
2. Receives new access + rotated refresh token. Persists. Retries.

### Host opts out of browser automation

1. `OAuthClientConfig` with `BrowserOpener = Arc::new(ManualOpener)`.
2. Default provider generates authorize URL but `open()` returns
   `OAuthError::ManualInteractionRequired { authorize_url, state, ... }`.
3. Transport wraps that into `McpError::AuthRequired(AuthRequest)` with
   `AuthOperation::McpConnect` and `challenge` metadata containing the URL.
4. Host displays the URL, user completes flow externally, host calls
   `resolve_auth_and_resume` with `{ "code": "...", "state": "..." }`.
5. The provider's replay path exchanges the code and stores the token.

## Testing plan

### Unit

- RS metadata parser: accepts spec examples; rejects `resource` mismatch.
- AS metadata parser: accepts spec examples; rejects `issuer` mismatch.
- `ResourceUri` canonicalization: exhaustive table against the spec examples
  ("valid canonical URIs" and "invalid canonical URIs" lists).
- PKCE: verifier length, charset, S256 matches known test vectors from RFC
  7636 Appendix B.
- `WWW-Authenticate` parser: quoted and unquoted `resource_metadata`,
  multiple schemes.

### Integration (mock servers)

Spin up a mock AS with `wiremock` or `hyper` in tests:

- Full authorization-code + PKCE happy path.
- DCR happy path (registration endpoint issues `client_id`).
- Refresh token rotation (reject reuse of old refresh token).
- Issuer mismatch rejection.
- `resource` mismatch rejection.
- 401-with-`WWW-Authenticate` → discovery → retry.
- Token expiry triggers proactive refresh.
- Loopback listener: correct `state` accepted, mismatched rejected.

### Interop

Against at least one real MCP server. Candidates (verify at implementation
time that these are still correct):

- A public reference MCP OAuth server if the ModelContextProtocol org
  publishes one.
- An internally stood-up server using `rmcp` with its OAuth adapter enabled.

## Phased delivery

### Phase A — discovery and static tokens

- `ResourceUri` canonicalization.
- RS metadata + AS metadata fetchers with validation.
- `WWW-Authenticate` parser.
- New `McpAuthConfig::Bearer { token }` variant; transport attaches header.
- 401 handling that surfaces `AuthRequest::McpConnect` with discovery URLs
  in `challenge` metadata (no OAuth flow yet — host resolves externally).

Exit: a host that already has a token (from its own OAuth code) can connect
to a spec-compliant MCP server and is handed spec-compliant discovery info
on 401.

### Phase B — default OAuth provider

- PKCE generator.
- Loopback listener.
- Authorization-code + token exchange.
- Token refresh with rotation.
- In-memory `TokenStore`.
- `McpAuthConfig::OAuth(OAuthClientConfig)`.

Exit: a host configured with `OAuthClientConfig` and no pre-existing tokens
can complete a real OAuth flow end-to-end against a mock AS.

### Phase C — dynamic client registration

- RFC 7591 registration client.
- Persistent client registration cache (via `TokenStore`).
- Fallback path surfacing manual registration instructions when DCR is
  unavailable.

Exit: first-run UX against a DCR-supporting AS requires zero pre-configured
`client_id`.

### Phase D — host extension points

- `OAuthProvider` trait public + `McpAuthConfig::Custom`.
- `BrowserOpener` + `RedirectInbox` traits.
- Documented integration example for host-supplied redirect delivery (e.g. a
  desktop app that routes its own custom URL scheme).

Exit: hosts with constraints (no browser, custom redirect, external credential
manager) can substitute any component without forking the crate.

### Phase E — polish

- `book/` chapter on MCP auth.
- `docs/mcp.md` updates cross-referencing this doc.
- Security-audit the checklist above with a second reviewer.
- Telemetry: emit observer events for `oauth.discovery.*`,
  `oauth.token.refreshed`, `oauth.token.rotated`, `oauth.auth.failed`.
  No token values in events ever.

## What we should validate early

Before landing Phase B, prove against a mock AS:

1. Discovery pipeline correctly walks 401 → RS → AS with validation.
2. PKCE verifier reaches the token endpoint byte-for-byte identical to what
   was hashed for the challenge.
3. `resource` parameter is present on both authorize and token requests.
4. Refresh-token rotation rejects replay of the prior refresh token.
5. A failed discovery step (issuer mismatch, missing `resource`) produces a
   precise `McpError`, not a generic transport failure.
6. Tokens never appear in `Debug` output, error messages, or observer events.

If any of those are awkward, the seam between the HTTP transport and the
OAuth module is wrong and should be reworked before wiring in DCR.

## Open questions

- **Token-store placement.** In-memory is enough for v1; does a first-party
  keychain backend belong in `agentkit-mcp`, a new `agentkit-mcp-oauth-keychain`
  crate, or entirely in host code? Default recommendation: separate crate so
  `agentkit-mcp` stays runtime-light.
- **Multi-audience tokens.** RFC 8707 allows multiple `resource` values. MCP
  says "MUST identify the MCP server that the client intends to use the token
  with" (singular). Default to single-resource requests; revisit if a real
  use case emerges.
- **Scope negotiation.** Who decides which scopes to request? Options: host
  config, discovered `scopes_supported`, union. Default: host config only;
  the crate does not invent scopes.
- **Concurrent refresh.** If two in-flight requests both detect an expired
  token, only one should drive the refresh. Use `tokio::sync::Mutex` on the
  `TokenStore` entry, not a global lock.
- **Clock skew.** Proactive-refresh window (60s) is a guess. Consider making
  it configurable on `OAuthClientConfig`.

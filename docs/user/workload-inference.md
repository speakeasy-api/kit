# Workload identity for inference

Use workload identity for deployed workloads that receive projected identity tokens.
For interactive local use, follow [provider authentication](getting-started-and-configuration.md#choose-a-provider-and-authenticate) instead.

For `openrouter` or `speakeasy`, configure a provider-specific workload exchange
in `~/.kit/config.toml`. Kit reads a deployment-projected JWT file; it does not
mint the external assertion. The deployment must rotate that file before renewal.
These settings override API-key and browser credentials for that provider only.
An exchange failure stops inference: Kit never falls back to another credential.

OpenRouter uses [workload identity federation](https://openrouter.ai/docs/guides/overview/auth/workload-identity-federation)
with an organization federation policy targeting an inference-capable key:

```toml
provider = "openrouter"
model = "anthropic/claude-sonnet-4"

[inference_providers.openrouter]
endpoint = "https://openrouter.ai/api/v1/chat/completions"
[inference_providers.openrouter.workload]
tokenFile = "/var/run/identity/assertion.jwt"
endpointMode = "explicit"
grant = "tokenExchange"
tokenEndpoint = "https://openrouter.ai/api/v1/oauth/token"
scope = "inference"
[inference_providers.openrouter.workload.extensions]
federation_policy_id = "<FEDERATION_POLICY_ID>"
```

Speakeasy uses its shared workload issuer's JWT-bearer token endpoint. Configure
issuer trust and project authorization on the server first, then use the actual
token endpoint returned for that issuer:

```toml
provider = "speakeasy"
model = "anthropic/claude-sonnet-4"

[inference_providers.speakeasy]
endpoint = "https://ai.speakeasy.com/chat/completions"
[inference_providers.speakeasy.workload]
tokenFile = "/var/run/identity/assertion.jwt"
endpointMode = "explicit"
grant = "jwtBearer"
tokenEndpoint = "https://ai.speakeasy.com/oauth/usi/<ISSUER_ID>/token"
resource = "urn:speakeasy:inference"
[inference_providers.speakeasy.workload.extensions]
project_id = "<PROJECT_ID>"
```

The Speakeasy assertion's audience is the shared issuer URL or its token endpoint,
not `urn:speakeasy:inference`. Assertions with a `jti` (or `uti`) are single-use: a new
exchange needs a fresh assertion. This is ordinary workload inference, not a
replacement for managed assistant credentials or chat-bound authorization. Kit
sends bearer authentication without API-key, project, or assistant/chat headers.

Both examples pin HTTPS token and completion endpoints explicitly; redirects are
disabled. Optional `audience`, `resource`, `scope`, and string-valued `extensions`
belong to the workload exchange, not completion headers. `endpointMode = "discovery"`
instead requires `expectedIssuer`, same-origin issuer metadata, and an HTTPS
`resource`; use explicit mode for Speakeasy's inference URN. Keep this configuration
operator-controlled: the endpoints determine where credentials are sent.

Kit shares in-memory tokens across sessions for each configured provider, caches
only until the returned `expires_in` minus a safety margin, and rereads the source
file on renewal. An RFC 8693 response without `expires_in` is used once and not
cached. A token may outlive its source assertion when the server permits it.
Tokens and assertions are not saved in Kit's credential store. Workload requests
are not automatically replayed on authentication refusal, throttling, or server
errors; API-key retry behavior is unchanged. Select models explicitly: workload
grants do not imply model-catalog access, and Kit does not discover a context limit
through these grants. Other features with separate credentials (such as images or
evaluation) are not authenticated by this configuration.

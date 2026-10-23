# Speakeasy workload authentication

Use native Speakeasy inference without a saved API key or browser sign-in. Supply
a short-lived JWT assertion file and an HTTPS authorization server that accepts
the RFC 7523 JWT bearer grant. Ask your administrator for the issuer, token
endpoint, and inference resource. The token endpoint must share the issuer origin.

Create a deployment-owned file such as `/etc/kit/speakeasy-workload.json`:

```json
{
  "project": "your-project",
  "workload": {
    "tokenFile": "/run/identity/assertion.jwt",
    "expectedIssuer": "https://identity.example.com",
    "tokenEndpoint": "https://identity.example.com/oauth/token",
    "resource": "https://ai.speakeasy.com"
  }
}
```

Set `KIT_SPEAKEASY_WORKLOAD_AUTH=/etc/kit/speakeasy-workload.json` in the process
environment. The configuration path must be absolute and name a regular file of
at most 64 KiB. Select a `speakeasy:` model as usual. This explicitly selects
workload mode: invalid configuration or token acquisition fails inference, without
API-key, browser, or refresh-token fallback. Unset the variable to restore normal
Speakeasy authentication.

Keep configuration under deployment control, not in model-generated input or MCP
server responses. `tokenFile` must be absolute. The issuer, endpoint, and resource
must be HTTPS URLs. The resource must use the `https://ai.speakeasy.com` origin on port 443; MCP access uses separate token
sessions even if the assertion source is shared. Tokens are not saved to the
credential store.

Requests send `Authorization: Bearer` and `Gram-Project`, not `Gram-Key`. The bearer
is restricted to `https://ai.speakeasy.com` on port 443. Redirects and custom
inference origins are unsupported. A 401 invalidates the token for the next
request, without automatically replaying inference.

Cold starts construct fresh token sessions. Embedders restoring process-memory
snapshots must quiesce requests and call
`kit::workload_auth::invalidate_after_restore()` before resuming. Memory restore
without that hook is unsupported for workload authentication.

See [workload authentication and restore requirements](../reference/workload-authentication.md)
for replay-policy prerequisites, expiry, and the application snapshot barrier.
See [Configuration and authentication](getting-started-and-configuration.md).

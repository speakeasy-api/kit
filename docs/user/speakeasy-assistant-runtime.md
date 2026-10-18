# Speakeasy assistant runtime inference

A trusted control plane can provision one revocable assistant-wide inference
credential before starting Kit. The same credential serves multiple ACP sessions;
each session carries its own canonical Gram chat binding. Kit does not mint or
refresh this credential, or call a per-turn credential API.

## Provisioning

Select the existing file credential backend with `--credential-store file` and
`--credential-dir <private-directory>`. Provision this file inside that directory:

```text
f5a93b2411bbf44d12df4db53aa064d87685df4fad637f3c61a653f4bc5b200f.json
```

The filename is the lowercase BLAKE3 digest of `speakeasy`, a NUL byte, and
`assistant-runtime`, followed by `.json`. Use a private directory and a file
readable only by the runtime user. Do not put the credential in ACP metadata,
transcripts, command-line arguments, or logs.

```json
{
  "version": 1,
  "bearer_token": "<durable credential issued by AICP>",
  "origin": "https://your-gram-origin.example"
}
```

Set `SPEAKEASY_ORIGIN` to the same origin. Runtime credentials have no default
production origin. Origins cannot contain URL credentials, paths other than `/`,
queries, or fragments. HTTPS is required unless `SPEAKEASY_ALLOW_INSECURE_HTTP=1`
is explicitly set for development. For local Docker, both origins can be
`http://aicp:8080` with that opt-in. Authenticated requests do not follow redirects.

The runtime record takes precedence over the existing `speakeasy/default` API-key
record. Malformed runtime records, unsupported versions, and origin mismatches
fail closed; Kit does not fall back to a different identity. Existing API-key
records retain their format and default production origin.

## Models and inference

Kit sends the bearer credential to `GET /chat/models` and `POST /chat/completions`
at the configured origin. The model endpoint must return the server-filtered
OpenRouter-compatible shape `{"data":[{"id":"provider/model","context_length":123456}]}`.
Only returned models are advertised for Speakeasy: discovery failure does not fall
back to the public OpenRouter listing or add a guessed current model. Catalogs
retain the existing response bounds of 2 MiB and 10,000 entries.

## Canonical chat association

For ACP session creation or restoration, the trusted control plane supplies:

```json
{
  "_meta": {
    "ai.speakeasy/gram-chat-id": "<existing assistant_threads.chat_id UUID>"
  }
}
```

This is the existing control-plane chat UUID, not an ACP session UUID or hash.
The binding is immutable for the session, persists across restoration, and is
forwarded as `Gram-Chat-ID` for runtime inference. Runtime inference requires a
non-nil UUID binding. The model catalog request does not require a chat binding.

The server must validate that the supplied chat belongs to the authenticated
assistant/runtime and suppress inference transcript capture. AICP's ACP
projection is the sole transcript writer. This contract supports the assistant's
`runtime_version=3`; it does not change Kit's ACP protocol version.

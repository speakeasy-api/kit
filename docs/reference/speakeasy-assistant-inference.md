# Speakeasy assistant inference adapter contract

Use this reference when implementing an AICP adapter that launches Kit, creates
ACP sessions, and projects their transcripts. For operator setup and recovery,
see [Run a Speakeasy assistant with a provisioned credential](../user/speakeasy-assistant-runtime.md).

## Authenticate inference requests

Provision the version-1 credential documented in the setup guide through Kit's
credential store. The record uses namespace `speakeasy` and identity
`assistant-runtime`; its filename is the lowercase hexadecimal BLAKE3 digest of
`UTF8("speakeasy") || 0x00 || UTF8("assistant-runtime")`, followed by `.json`.
Do not create a separate credential-file environment setting.

The record takes precedence over dashboard API-key credentials. Its `origin`
must match `SPEAKEASY_ORIGIN` after URL normalization. Runtime credentials have no
production-origin default. HTTPS is required unless
`SPEAKEASY_ALLOW_INSECURE_HTTP=1`; requests do not follow redirects.

| Operation | Request | Authentication |
| --- | --- | --- |
| Model discovery | `GET {origin}/chat/models` | `Authorization: Bearer <credential>` |
| Inference | `POST {origin}/chat/completions` | The same bearer credential |

The bearer is assistant/runtime-wide, not tied to one ACP session. Issue it
before startup, enforce admission and revocation server-side, and do not require
Kit to call a bootstrap or per-turn credential API. Runtime requests omit the
legacy `Gram-Key` and `Gram-Project` headers. Dashboard API-key authentication
remains supported separately. Its default production origin is
`https://ai.speakeasy.com`; saved API keys must carry the origin that verified them,
and configuration must match it. Historical API-key records without an origin
require a fresh login rather than automatic migration. Runtime records are never
rewritten or retargeted when the production origin changes.

## Discover eligible models

Return only models eligible for the authenticated principal, using
OpenRouter-compatible model objects:

```json
{
  "data": [
    {
      "id": "provider/model",
      "context_length": 200000
    }
  ]
}
```

Kit advertises the returned model IDs and retains positive `context_length`
values for context-window decisions. The response is bounded to 2 MiB and 10,000
entries; the selector does not truncate valid catalogs at 2,000 models.
Discovery errors do not trigger a public OpenRouter fallback or append the
current model to the advertised Speakeasy list. Model discovery sends no
`Gram-Chat-ID` header.

## Associate an ACP session with a chat

On session creation, supply this extension in the ACP request's `_meta` object:

```json
{
  "_meta": {
    "ai.speakeasy/gram-chat-id": "550e8400-e29b-41d4-a716-446655440000"
  }
}
```

The value must be the canonical non-nil UUID of the intended control-plane chat,
not a newly generated value, an ACP session ID, or a hash of that ID. For AICP,
use the chat already associated with the assistant thread. Kit normalizes the
UUID and sends it on each runtime inference request:

```http
Authorization: Bearer <assistant-runtime-credential>
Gram-Chat-ID: 550e8400-e29b-41d4-a716-446655440000
```

The server must validate that this chat belongs to the authenticated
assistant/runtime and resolve the corresponding thread. Omitting the header is
not sufficient for an assistant-wide credential serving multiple chats. Kit
rejects runtime model-session initialization without the explicit association.

## Preserve chat identity through session lifecycle

- **New session:** validate the supplied UUID and persist its binding before
  inference starts.
- **Load/resume:** keep the persisted binding. The client may omit metadata on
  cold resume or repeat the same value; a different value is rejected. Resume
  cannot bind an older unbound session. Resident session ownership checks still
  apply.
- **Model change or compaction:** retain the owner's canonical association for
  subsequent inference, including auxiliary compaction calls.
- **Direct ACP fork:** supply the intended binding explicitly; Kit does not
  implicitly copy the source chat binding into the fork.
- **Built-in child session:** Kit explicitly supplies the parent's authorized
  inference association on child startup and native fork. Child configuration
  retains it through seeded forks and restart. External harnesses do not receive
  this metadata.

## Keep one transcript writer

For the dedicated assistant runtime credential, the server must suppress
inference transcript capture, including child and compaction calls. AICP's ACP
projection owns transcript persistence. Sharing the parent's chat association
with a child attributes inference to that chat; it does not authorize a second
transcript capture path.

This contract applies to AICP assistant `runtime_version=3`. That deployment
setting is separate from Kit's ACP wire protocol version, selected with
`--protocol-version 1` or `--protocol-version 2`.

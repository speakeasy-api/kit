# Agent Hooks Protocol

Use hooks to enforce tool policies, inspect model requests, or add session context.
Kit delivers [Agent Hooks Protocol](https://agenthooksprotocol.com/)
events to hook backends registered in `~/.agents/hooks.json` (global) and
`<root>/.agents/hooks.json` (project). When neither file exists, hooks are off.

## Configure hooks

Install a trusted hook backend, then create either registration file. This
example intercepts tool calls; replace `command` with your backend executable.
The backend must implement the [Agent Hooks Protocol](https://agenthooksprotocol.com/).

```json
{
  "protocolVersion": "draft",
  "hooks": [
    {
      "id": "org.example.policy",
      "transport": {
        "type": "stdio",
        "command": "/usr/local/bin/policy-hook",
        "lifecycle": "persistent"
      },
      "subscriptions": [
        {
          "events": ["tool.before"],
          "mode": "intercept",
          "timeoutMs": 3000,
          "failurePolicy": "fail-closed",
          "content": {"default": "body"}
        }
      ]
    }
  ]
}
```

Kit reads the global file, then the project file. A project hook replaces the
global hook with the same `id`; other hooks from both files stay active. Kit
skips an unreadable or malformed file with a warning. A subscription
selector that matches no event Kit emits disables hooks with a warning.

Stdio hook processes inherit Kit's full environment, including any credentials
or other sensitive values present in the environment.
A transport's `env` entries override inherited values. Only configure stdio
hooks that you trust with those values.

Hook backends run local processes or receive session content. Review both files
before you run Kit in a project you do not trust.

## Sign in to an OAuth hook backend

Each HTTP hook backend has its own sign-in. Signing in to one backend never
signs you in to another, and a backend that is signed in is never asked again.
One sign-in covers a backend's event endpoint and its upload endpoints.

### What happens during a session

A delivery that needs a sign-in never opens a browser and never waits. It
fails, and the subscription's `failurePolicy` decides the result: a
`fail-closed` backend denies the operation, a `fail-open` backend lets it
continue. The backend stays registered.

When the next turn starts, Kit prepares the sign-in in the background and
shows its link where only you see it:

- The TUI shows `sign in to hook <backend>: <link>` in the status line.
- Other ACP clients receive a notice with the link, and a second notice when
  sign-in finishes.
- A session without a client writes one line per link to stderr.

Every turn shows the same link until it expires (at most 10 minutes); then
the next turn shows a new one. Kit listens on
`http://127.0.0.1:<port>/callback` while a sign-in is pending, so you can
open the link at any time before it expires. The port is fixed per backend,
so every Kit process uses the same redirect. When several Kit processes run,
one listens and any of them can complete the sign-in, because they share the
credential store. Hook messages and sign-in links never enter the transcript
that the model reads.

### Sign in from the command line

```sh
kit auth status hooks --root /path/to/project
kit auth login hooks --root /path/to/project            # each backend that needs it
kit auth login hooks com.example.policy --root /path/to/project
kit auth logout hooks com.example.policy --root /path/to/project
```

`status` prints one line per backend: `authorized`, `needs-login`, `anonymous`,
`bearer token`, or `failing (<reason>)`. `login` prints each link and waits
until you approve it. `logout` removes the backend's tokens, pending sign-in
and registered client, and nothing of any other backend. ACP clients that
support terminal authentication offer one `hooks` method, which runs
`kit auth login hooks`.

Sign-in requires a persistent credential store (`--credential-store file` or
`keychain`), as MCP authentication does. Memory-only storage cannot keep a
sign-in.

### Configure a backend

A registration `authentication` preset fixes the resource, issuer and client:

```json
"authentication": {
  "type": "oauth",
  "flow": "authorization_code_pkce",
  "resource": "https://policy.example.com/hooks",
  "issuer": "https://login.example.com",
  "clientId": "client-id-from-the-operator",
  "scopes": ["hooks:invoke"]
}
```

Without a preset, Kit learns the authorization server from the backend's
protected resource metadata after the backend rejects an anonymous request,
and uses it only if `oauth.issuer` names it or `oauth.trustAdvertisedIssuer`
is `true`.
Put further settings in an `oauth` object on the backend, beside its
`authentication`. Other AHP harness adapters read the same object, and AHP
receivers ignore it. All settings are optional, and they take precedence over
the preset:

```json
{
  "id": "com.example.policy",
  "transport": { "type": "http", "url": "https://policy.example.com/hooks" },
  "oauth": { "issuer": "https://login.example.com", "registration": true }
}
```

| Setting | Meaning |
| --- | --- |
| `issuer` | The authorization server trusted for this backend. A preset's `issuer` counts too. |
| `trustAdvertisedIssuer` | Without `issuer`, trust the authorization server the backend advertises. Default `false`: Kit refuses it. Client credentials always need `issuer`. |
| `flow` | `authorization_code_pkce` (default) or `client_credentials`. |
| `scopes` | Scopes to request. Kit never records a broader grant. |
| `clientId` | A client registered for Kit in advance. It always wins. |
| `clientSecretRef` | For client credentials: a key in `~/.kit/ahp-secrets.json`. |
| `clientMetadataDocument` | `{ "url", "redirectUris" }`: a Client ID Metadata Document that you publish. Kit uses it where the issuer supports such documents. `redirectUris` must include this backend's redirect. |
| `registration` | `true`, or `{ "clientName", "scopes", "initialAccessTokenRef" }`: permission to register a client (RFC 7591) at `issuer`, which it requires. |
| `redirectPort` | The callback port (1024 to 65535). Default: a port derived from the backend id. |
| `loginLifetimeMinutes` | How long a sign-in link lasts, up to 10 (the default). |
| `allowPrivateNetwork` | Allow the authorization server on loopback or private addresses. Default `false`. |

Kit selects the client in this order: `clientId` (or the preset's), then the
Client ID Metadata Document, then dynamic registration. If none applies,
`status` reports `failing (credential_unavailable)`.

OAuth requests go only to HTTPS endpoints. They follow no redirects, and they
connect only to the public addresses they resolved, unless
`allowPrivateNetwork` is set. A client secret is sent only to its issuer's own
origin.

`~/.kit/ahp-secrets.json` holds secrets that hooks.json names by reference
(`clientSecretRef`, `initialAccessTokenRef` and a bearer `tokenRef`), as
`{ "version": 1, "secrets": { "<ref>": "<secret>" } }`. Make it readable only
by you (`chmod 600`); otherwise Kit ignores it. Secrets of dynamically
registered clients go in Kit's credential store.

An explicit `Authorization` header in `transport.headers` takes precedence
over all of the above: Kit sends that header and nothing else.

### Backends that supply their own subscriptions

A backend that supplies its subscriptions through `hooks/configuration`
contributes none while it needs a sign-in. After you sign in, its
subscriptions apply from the next session.

## Use server-provided subscriptions

Omit a backend's `subscriptions` field to let it supply subscriptions through
`hooks/configuration`. Kit resolves discovery before deciding whether to buffer
model output or disable speculative compaction. Explicit `subscriptions`,
including an empty array, bypass discovery.

Kit trusts configured hook backends to supply upload settings, including upload
destinations, credential bindings, and forwarding of configured backend headers.
Only configure backends you trust with the selected content and credentials.
Server-provided subscriptions cannot expand the event or effect authority Kit
grants in the table below.

If a backend's discovery request fails or its response is invalid, the SDK skips
that backend for this hook instance. Check the backend's availability and
`hooks/configuration` response, or configure explicit subscriptions when you
need client-defined routing and failure policies.

## Events

| Event | Where Kit emits it | Hook authority |
| --- | --- | --- |
| `session.start` | A session's model connection opens | Inject context |
| `turn.start` | A logical agent-loop turn admits its input, with native session and turn IDs | Deny, replace the admitted prompt, inject context |
| `turn.end` | That logical turn completes, fails, or is cancelled | Observe |
| `session.end` | A prompt run finishes or an ACP session closes | Observe |
| `user.message.inbound` | A user prompt arrives, including an interjection during a turn | Deny, replace the prompt |
| `model.request.before` | Before each model call, with the transcript | Deny, replace messages, supply a response, inject context |
| `model.response.after` | A model call finishes, with its output | Replace messages, block response |
| `model.error` | A model call fails | Observe |
| `tool.before` | Before a tool runs | Allow, deny, return a result, replace or merge input, inject context |
| `tool.after` | A tool finishes, with its output | Replace output, block output, inject context |
| `context.compact.before` | Before compaction summarizes the transcript | Deny, return a summary, replace summarizer instructions, inject context |
| `context.compact.after` | Before the summary is installed | Replace the summary |

Every event that supports interception also supports `observe` subscriptions.
Interceptable events accept `message` effects for user-facing notices.
Context injection accepts text messages with `deliverAt: "now"`, delivered once
with the next actual model request; it does not change the saved transcript.
Model message edits accept text replacements and preserve unchanged native
items. Response interception withholds streaming output until hooks finish.
A compaction denial fails the turn without replacing the transcript.

`user.message.inbound` applies arrival policy before `turn.start`. The latter's
`items` contain the effective admitted input, including attachments and
notifications submitted with it, not prior-turn history. A `modify` effect with
target `prompt` and operation `replace` on `turn.start` replaces this entire
canonical item list. Retain the supplied opaque part identities to preserve
attachments; edits cannot fabricate media. Both events support user-facing
`message` effects. An interjection during an active turn emits
`user.message.inbound` without another `turn.start`. Context injected at
`turn.start` reaches the turn's first model request; it is discarded when the
prompt is denied.

A denied prompt is removed together with its admitted resources and
notifications. Denial fails the current turn without a further model call,
including when the denied input is an interjection. Prior-turn history is
preserved. A denied model request fails the turn. A denied tool call returns
a permission error to the model.

Kit checks edited tool input against the tool's input schema before the tool
runs; an edit that does not match counts as a hook failure. An edit that the
hook does not also allow goes through Kit's permission policy again.

`tool.after` accepts `modify` with target `output` and operation `replace`
(messages containing text only), or `flow.stop` to withhold output and
return a permission error. Blocking takes precedence over replacement but
cannot undo side effects; use `tool.before` to prevent execution.

If all `tool.after` subscriptions use `observe` mode, a hook failure does not
block the tool result. Kit displays a warning instead. If a `tool.after`
interceptor is registered, a dispatch failure can still block the result.

Tool events cover the tools that compose scripts call, such as `shell`, `edit`,
`read_file` and the MCP `tool` dispatcher. Kit projects text, structured data,
tool calls and tool results into categorized message parts. Tool-call IDs and
result status remain in selectable content. Readable reasoning summaries are
included; opaque provider continuation data and arbitrary metadata are not.
Media and file references retain their tagged representation without fetching
remote URLs or resolving artifact handles. Inline binary media uses AHP-owned
attachments. A subscription that selects attachment bodies must configure an
AHP upload destination; select `metadata` or `omit` for media/file categories
when the backend does not accept uploads. All content respects the
subscription's selection policy.

### Payload context

Events include the owning session ID and Kit's configured working directory
and workspace root. Model and tool events also include their native turn ID.
Compaction events identify the conversation being compacted, not the separate
summarizer session. The SDK supplies event IDs, timestamps, source, harness
identity, and the session-start manifest, except that Kit chooses each
`tool.before` event's ID itself.

A subagent session's `session.start` carries `parentEventId`: the ID of the
parent session's `tool.before` event for the `subagent` call that started it.
A backend can follow it to nest subagent sessions under their parent at any
depth. A top-level session and a fork have none.

Model responses report the provider's resolved model and available token,
cache, and cost usage. Turn-end usage is the loop's aggregate. Missing usage
is omitted, not reported as zero. Model latency measures the host-observed
call after hook admission; tool duration is included when the tool reports it.
Structured provider errors retain sanitized status, code, and retry accounting.
The AHP attempt identity is synthesized for each host model call; its number
is not a provider retry count.

Kit-specific fields use these namespaces:

- `params["dev.kit.tools"]`: model-visible tool names, descriptions, and input schemas.
- `params["dev.kit.cache"]`: requested cache mode, retention, and strategy, without routing keys.
- `extensions["dev.kit.response"]`: provider response ID, when reported.
- `extensions["dev.kit.usage"]`: reported reasoning-token count.
- `extensions["dev.kit.retry"]`: sanitized provider failure and retry accounting.

These are host settings and observations, not a copy of the provider's wire
request. Credentials, environment contents, arbitrary request metadata, and
opaque continuation state are not added to payloads. Provider-only generation
settings, exact compaction token deltas, and generic tool MCP/batch/file-change
metadata are omitted when unavailable. Unknown
required model identities are explicitly identified in `gaps`.

Tool calls, model retries and model continuations stay within the same logical
turn; they do not emit extra turn boundaries. Kit delivers turn observations
in order, before the session's next hook event and whenever the session goes
idle. An abrupt process exit can drop a terminal observation. Do not rely on
terminal observations as a durable audit log. The start event has no transcript
snapshot and reports an external trigger. Kit does not initiate AHP hook
continuations; the continuation count is zero.

Kit does not emit `turn.finish.before`, `model.switch.*`, `config.change.*`, `file.changed`
or the tool permission and progress events.

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
| `turn.start` | A logical agent-loop turn starts, with native session and turn IDs | Observe |
| `turn.end` | That logical turn completes, fails, or is cancelled | Observe |
| `session.end` | A prompt run finishes or an ACP session closes | Observe |
| `user.message.inbound` | A new turn admits a user prompt | Deny, replace the prompt |
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

A denied prompt is removed, together with resources attached to it, and the
turn ends without a model call. A denied
model request fails the turn. A denied tool call returns a permission error to
the model.

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
identity, and the session-start manifest.

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

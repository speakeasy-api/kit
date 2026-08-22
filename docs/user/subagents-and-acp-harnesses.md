# Reusable subagents and ACP harnesses

Kit can start parent-owned nested agents through the Agent Client Protocol (ACP). A subagent is a reusable Runlet value, not a detached background task: start it with `subagent`, continue the same session with `prompt`, branch its completed context with `fork`, list retained handles with `subagents({})`, or terminate one with `close`.

## Start, prompt, and fork a reusable subagent

Use the object form of the hidden tools inside `compose`:

```text
first = subagent({ prompt: "Inspect the parser and identify the smallest risk." })
second = prompt({
  subagent: first,
  prompt: "Now propose a minimal fix."
})
branch = fork({
  subagent: second,
  prompt: "Explore an alternative without changing the original session."
})
return { main: second.output, alternative: branch.output }
```

Each successful turn returns a session value with `id`, `output`, and `generation`. `subagent` creates an ID at generation 1. `prompt` keeps that ID and increments its generation. `fork` creates a different ID whose generation is one greater than the supplied source value; it does not advance the source session. `subagents({})` returns active sessions in ID order. A session whose initial prompt is still in progress appears as `{ id, status: "starting" }`; it can be closed by ID but cannot be prompted or forked until the initial `subagent` call returns its complete handle. Completed sessions retained for reuse appear as their latest values. Close a session with either `close(value)` or `close({ id: value.id })`; the latter is useful when only an ID is available. Closing an unknown ID fails with `unknown subagent session`. Kit sends ACP `session/close` when the harness advertises it. A standalone process without that capability is terminated when its handle is dropped. If native-fork siblings share a process and the harness cannot close one logical session, `close` fails rather than claiming success or disrupting the siblings.

Always pass the latest completed value back to `prompt` or `fork`. Reusing an older value fails with `stale subagent generation N; current generation is M`. This prevents two continuations from silently racing on one session. Calls on an individual ACP session are serialized, while separate forked sessions can be prompted concurrently.

The optional `harness` and `model` arguments belong only on `subagent`. `harness` overrides the user's configured harness preference. `model` selects an exact model value ID advertised by that harness through its ACP session configuration, or a model alias configured for that harness. Omit either argument to retain the configured preference. `prompt` and `fork` retain the original session's harness and model. An explicit model fails before the first prompt if the harness does not advertise a selectable `model` option or rejects the value.

## Require structured JSON output with `output_schema`

Pass a JSON Schema object or boolean to any turn-producing call:

```text
review = subagent({
  prompt: "Review the proposed change.",
  output_schema: {
    type: "object",
    properties: {
      approved: { type: "boolean" },
      reason: { type: "string" }
    },
    required: ["approved", "reason"],
    additionalProperties: false
  }
})
return { approved: review.output.approved, reason: review.output.reason }
```

Kit validates `output_schema` before dispatch and appends instructions asking the child for one bare JSON value. If the response parses and validates, `output` is that JSON value. If a completed response is malformed, wrapped in Markdown, or does not match the schema, `output` falls back to the raw response text. The turn still succeeds and its generation advances, so a later `prompt` can request a repair. A later `prompt` or `fork` may use a different schema. Runlet treats `output` dynamically; it does not statically derive field types from the schema. An explicit `null` schema is invalid and reports `output_schema must be a JSON Schema object or boolean`.

### Text and ACP update capture

Without `output_schema`, `output` is text. A turn that emits selected non-text agent-message content, tool calls, tool-call updates, or plans also returns a sibling field:

```text
{
  id: "...",
  output: "Completed review",
  generation: 1,
  updates: { items: [...], truncated: false }
}
```

Text-only turns omit `updates`. Capture is limited to 64 update objects and 64 KiB of serialized update data per turn. Excess or oversized updates are omitted and set `updates.truncated` to `true`. When an ACP tool update's rendered content is only a JSON copy of its structured `rawOutput`, Kit retains only `rawOutput` in the parent-visible update. Kit does not expose child thoughts, user-message echoes, usage, modes, commands, configuration, or session metadata through this value.

## Choose the built-in `acp.kit` harness

`acp.kit` is always available and is the default when `[subagent].harness` is not configured. By default Kit launches the installed `kit` executable as `kit acp`. A built-in child inherits the runtime root, provider, model, MCP configuration and credential storage, cancellation, and nesting depth. An explicit `subagent.model` selection overrides the inherited model for that ACP session. It does not start an A2A listener.

You can override only the executable and base arguments while preserving built-in Kit behavior:

```toml
[acp.kit]
command = "kit"
args = ["acp"]
```

Kit appends its required runtime, resolved reasoning-effort, persistent-session, resume, MCP, credential, and inherited-depth arguments after those base arguments. Use installed-binary commands such as `kit --help` and `kit acp --help` to inspect the current command interface rather than relying on an exhaustive flag list here.

Kit's ACP server also advertises a separate reasoning-effort session selector with `Default`, `Low`, `Medium`, and `High` values. Changes take effect on the next turn without changing the selected model. `Default` leaves the effort unset, preserving provider defaults and `OPENROUTER_REASONING_EFFORT` when OpenRouter supplies one. Set the startup default with top-level `reasoning_effort` or `--reasoning-effort`; the bundled TUI exposes the same selector through `/effort`. Built-in `acp.kit` children inherit the startup value resolved from CLI and TOML, while generic ACP harnesses keep their own behavior.

Built-in subagent transcripts are durable on disk, but their reusable parent-owned values exist only for the lifetime of the owning parent session. Closing that main session drops its subagent manager, which closes every child actor and terminates the retained child processes. A later Kit process cannot pass an old value to `prompt` or `fork`.

## Configure a generic ACP v1 harness

A generic harness must speak ACP v1 as newline-delimited JSON-RPC over stdio and support `initialize`, `session/new`, and `session/prompt`. `session/fork` and `session/close` are optional capabilities. Keep stdout protocol-only; the agent may log to stderr. Kit runs the executable directly with the runtime root as its current working directory and inherits the parent environment. It does not invoke a shell, so pipes, environment assignments, compound commands, and shell quoting in `command` or `args` do not work.

Configure trusted argv profiles in `~/.kit/config.toml`:

```toml
[acp.review]
command = "review-agent"
args = ["acp"]
permissions = "deny"

[subagent]
harness = "acp.review"
```

References use the fully qualified `acp.<name>` form. Profile names must be non-empty and contain neither whitespace nor dots. `command` must be non-empty; use an executable on `PATH` or an absolute path. Kit does not perform a generic agent's login or ACP authentication flow, so authenticate and configure that agent before starting Kit. Generic session persistence beyond the parent process is agent-defined. Per-subagent model selection works for generic agents such as Codex, Claude, or Cursor only when their ACP adapter advertises and implements the selectable `model` session option.

An unknown selection fails with `unknown ACP harness "acp.name"` or `unknown subagent ACP harness "acp.name"`. Invalid references may report `ACP harness references must use acp.<name>`.

## Configure model aliases and allowed overrides per harness

Model namespaces differ between ACP harnesses. Configure aliases and explicit-override policy under the fully qualified harness reference:

```toml
[subagent]
harness = "acp.review"

[subagent.harnesses."acp.review"]
allow_model_overrides = ["provider:model-a", "provider:model-b"]

[subagent.harnesses."acp.review".models]
review = "provider:model-a"
fast = "provider:model-b"
```

A call may use either a configured alias or an exact model ID. Aliases are scoped to one harness, resolve before ACP startup, and are then checked against that harness's `allow_model_overrides`. Kit rejects configuration when an alias resolves outside its harness's allowlist.

Omitting `allow_model_overrides` allows all explicit model selections accepted by the harness. An empty list disables all explicit model overrides for that harness. The allowlist applies only when `subagent.model` is present; it does not inspect or restrict the harness's inherited or default model. Configured aliases indicate available routing choices, not recommendations. Agents should omit `harness` and `model` unless the user or active workflow explicitly requests an exact override or configured alias.

## Fork capability and transcript fallback

Kit reads fork capability from the child's ACP `initialize` response at runtime. If advertised, `fork` uses native `session/fork`. If not advertised:

- `acp.kit` clones a completed durable transcript and starts an isolated Kit process for the branch. This fallback also applies when `[acp.kit]` overrides the executable/base arguments.
- A generic profile fails with `ACP harness "acp.name" does not advertise session/fork; transcript fallback is only available for Kit`. Its existing session still supports ordinary `prompt` calls.

Do not infer support from the agent name or an old compatibility table; ACP capabilities can change between agent releases.

## Headless permission policy

Nested agents cannot ask the user interactively. ACP permission requests therefore use the profile's fail-closed `permissions` policy:

- `permissions = "deny"` (the default) selects `reject_always`, or `reject_once` if that is the only rejection offered. If no rejection option exists, Kit cancels the permission request.
- `permissions = "cancel"` always cancels the request.

Kit never selects an allow option. Configure any additional non-interactive policy in the generic agent itself, with the same care as when running that executable directly.

## Cancellation, stop reasons, and retired sessions

Cancelling an outer turn propagates to nested work. For a dispatched prompt, Kit sends ACP `session/cancel` and allows up to five seconds for the child to settle; a child that does not settle is terminated. Cancellation while starting, waiting for the session lock, prompting, or forking returns a cancelled tool result.

`end_turn` and `max_tokens` are successful completed turns. In particular, a max-token response returns its partial output and remains reusable. `cancelled`, refusal (`nested agent refused the prompt`), `max_turn_requests` (`nested agent reached its turn-request limit`), protocol errors, and unknown stop reasons are failures. Once a `prompt` continuation has been dispatched and fails, Kit retires that session because its transcript may have changed; retry by starting a new subagent rather than reusing the old value. Reuse can report `unknown subagent session`, `subagent session is retired`, or `nested agent process is no longer running`.

## Limits and troubleshooting

Kit currently permits nesting to depth 2 and at most 120 live parent-owned subagent sessions per main session. Starting or forking beyond those bounds reports `subagent depth limit (2) reached` or `live subagent session limit (120) reached`. Use `subagents({})` to inspect retained sessions and `close` to release sessions that are no longer needed. Closed, failed, or explicitly closed children no longer consume capacity.

ACP startup must complete within 30 seconds. Native `session/fork` must also answer within 30 seconds. Common diagnostics include:

- `ACP harness spawn failure`: verify `command`, `args`, executable installation, and `PATH`.
- `ACP harness handshake timeout` or `ACP harness protocol handshake failure`: verify ACP v1 support, required methods, and that stdout contains only protocol messages. Check stderr for lines prefixed `ACP harness <name>:`.
- `ACP harness did not answer session/fork within 30 seconds`: update or repair the agent, or avoid `fork`; only `acp.kit` has the transcript fallback.
- `nested agent exited during startup` or `nested agent process exited without a response`: run the configured installed executable directly enough to verify installation and login, then inspect its stderr.
- Structured output remains text: ask for bare JSON, inspect the raw `output`, and use a repair `prompt` with an appropriate `output_schema`.

For current top-level and subcommand options, run `kit --help` or `kit <command> --help`.

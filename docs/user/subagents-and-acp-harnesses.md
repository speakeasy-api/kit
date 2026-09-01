# Reusable subagents and ACP harnesses

Kit can start parent-owned nested agents through the [Agent Client Protocol (ACP)](https://agentclientprotocol.com/get-started/introduction). ACP is an open interface between an agent client and an agent runtime. That separation lets Kit orchestrate Kit, Claude Code, Codex, Cursor, and other compatible harnesses through one lifecycle instead of maintaining a custom integration for each one.

A subagent is a reusable Runlet value, not a detached background task: start it with `subagent`, continue the same session with `prompt`, branch its completed context with `fork`, list retained handles with `subagents({})`, or terminate one with `close`.

## Why use another harness from Kit?

Keep Kit as the orchestrator and route only a bounded task to a specialist. The parent can start independent specialists concurrently, give each child explicit context, require structured output, continue a useful session, or fork an alternative. This makes the specialist's result composable with shell commands, edits, tests, MCP calls, and other agents in the same Runlet program.

An external harness remains a separate program with its own configuration, tools, account, and usage limits. Kit does not turn a Claude subscription into provider credentials for the built-in `acp.kit` harness. The `@agentclientprotocol/claude-agent-acp` package includes the Claude Agent SDK CLI, so it does not require a separate Claude Code installation. Authenticate that adapter out of band with either a Claude subscription or Anthropic Console. With `--claudeai`, work counts against subscription limits instead of metered Console API billing; whether that costs less depends on the workload and plan. This also lets you choose a model for work where it is particularly effective—for example, Claude Opus for visual and graphic design—without moving the whole coding session out of Kit.

Configure a descriptive alias for that route:

```toml
[acp.claude]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@0.69.0"]
permissions = "deny"

[subagent.harnesses."acp.claude".models]
designer = "opus"
```

Then ask for the configured specialist explicitly:

```text
design = subagent({
  name: "Visual Designer",
  harness: "acp.claude",
  model: "designer",
  prompt: "Review the existing dashboard and propose a coherent visual direction for the new analytics view. Return design guidance, not code."
})
return design.output
```

`designer` is a Kit-side alias for the model ID advertised by that ACP adapter. The external harness must accept the configured ID. Before starting the first Claude subagent, authenticate the adapter outside Kit:

```sh
npx -y @agentclientprotocol/claude-agent-acp@0.69.0 --cli auth login --claudeai  # Claude subscription
# Use --console instead for Anthropic Console API billing.
```

Kit does not perform this login. You can start Kit before authenticating the adapter; authentication only needs to finish before the subagent starts.

## Start, prompt, and fork a reusable subagent

Use the object form of the hidden tools inside `compose`:

```text
first = subagent({
  name: "Implementer",
  cwd: "../parser-worktree",
  prompt: "Inspect the parser and identify the smallest risk."
})
second = prompt({
  subagent: first,
  prompt: "Now propose a minimal fix."
})
branch = fork({
  subagent: second,
  name: "Alternative Reviewer",
  prompt: "Explore an alternative without changing the original session."
})
return { main: second.output, alternative: branch.output }
```

Each successful turn returns a session value with `id`, `name`, `output`, and `generation`. `subagent` creates an ID at generation 1. `prompt` keeps that ID and name while incrementing its generation. `fork` creates a different ID and uses its own preferred or fallback name; its generation is one greater than the supplied source value, and it does not advance the source session. Close a session with either `close(value)` or `close({ id: value.id })`; the latter is useful when only an ID is available. Closing an unknown ID fails with `unknown subagent session`. Kit sends ACP `session/close` when the harness advertises it. A standalone process without that capability is terminated when its handle is dropped. If native-fork siblings share a process and the harness cannot close one logical session, `close` fails rather than claiming success or disrupting the siblings.

Always pass the latest completed value back to `prompt` or `fork`. Reusing an older value fails with `stale subagent generation N; current generation is M`. This prevents two continuations from silently racing on one session. Calls on an individual ACP session are serialized, while separate forked sessions can be prompted concurrently.

The optional `name` argument is preferred on `subagent` and `fork`; `prompt` has no naming input and preserves the session name. The optional `harness`, `model`, and `cwd` arguments belong only on `subagent`. `harness` overrides the user's configured harness preference. `model` selects an exact model value ID advertised by that harness through its ACP session configuration, or a model alias configured for that harness. `cwd` selects the new subagent's working directory; relative paths resolve from Kit's working directory, and missing paths or non-directories fail before startup. Omit an argument to retain its configured default. `prompt` and `fork` retain the original session's harness, model, and working directory. An explicit model fails before the first prompt if the harness does not advertise a selectable `model` option or rejects the value.

## Inspect display names

The calling model should give each new `subagent` and `fork` a concise role-oriented display name based on its task, such as `Round 2 Implementer` or `Reviewer`. This uses the model already making the tool call; Kit does not make a separate naming request. The name is display metadata and never changes the prompt sent to the child. Omitting `name`, or supplying an invalid name, uses the lowest available `Agent N` label.

Kit trims preferred names and accepts 1–32 bytes of printable ASCII. Names compare case-insensitively among one parent's direct live children; clashes receive the lowest available numeric suffix, with the base shortened as needed to stay within 32 bytes. The immutable `s-…` ID remains authoritative; names never select handles.

A name is reserved when creation starts. A failed creation releases it; otherwise the reservation survives starting, working, idle, and reusable failures until `close` or terminal retirement. Nested Kit processes allocate independently, so separate descendant branches can generate duplicate visible names in the agent roster even though each parent keeps its direct-child names unique.

Use `subagents({})` to inspect live direct children. Each row contains `id`, `name`, `status`, `generation`, and the bounded current task summary. Closed and terminally retired children are omitted.

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

`acp.kit` is always available and is the default when `[subagent].harness` is not configured. By default Kit launches the installed `kit` executable as `kit acp`, whose default stdio protocol is ACP v1. A built-in child uses `subagent.cwd` when provided and otherwise inherits Kit's working directory; it also inherits the provider, model, MCP configuration and credential storage, cancellation, and nesting depth. An explicit `subagent.model` selection overrides the inherited model for that ACP session. It does not start an A2A listener.

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

Generic external child harnesses remain ACP v1: they must speak newline-delimited JSON-RPC over stdio and support `initialize`, `session/new`, and `session/prompt`. `session/fork` and `session/close` are optional capabilities. Keep stdout protocol-only; the agent may log to stderr. Kit runs the executable directly from the subagent's selected working directory, which defaults to Kit's working directory, and inherits the parent environment. It does not invoke a shell, so pipes, environment assignments, compound commands, and shell quoting in `command` or `args` do not work.

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

Kit currently permits nesting to depth 2 and at most 120 live parent-owned subagent sessions per main session. At the maximum depth, Kit omits `subagent` and `fork` from the compose catalog because neither operation can succeed there; the runtime depth check remains as a fallback. Exceeding the depth or capacity bounds reports `subagent depth limit (2) reached` or `live subagent session limit (120) reached`. Use `subagents({})` to inspect retained sessions and `close` to release sessions that are no longer needed. Closed, failed, or explicitly closed children no longer consume capacity.

ACP startup must complete within 30 seconds. Native `session/fork` must also answer within 30 seconds. Common diagnostics include:

- `ACP harness spawn failure`: verify `command`, `args`, executable installation, and `PATH`.
- `ACP harness handshake timeout` or `ACP harness protocol handshake failure`: verify ACP v1 support, required methods, and that stdout contains only protocol messages. Check stderr for lines prefixed `ACP harness <name>:`.
- `ACP harness did not answer session/fork within 30 seconds`: update or repair the agent, or avoid `fork`; only `acp.kit` has the transcript fallback.
- `nested agent exited during startup` or `nested agent process exited without a response`: run the configured installed executable directly enough to verify installation and login, then inspect its stderr.
- Structured output remains text: ask for bare JSON, inspect the raw `output`, and use a repair `prompt` with an appropriate `output_schema`.

For current top-level and subcommand options, run `kit --help` or `kit <command> --help`.

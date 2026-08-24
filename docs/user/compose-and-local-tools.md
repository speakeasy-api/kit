# Compose, Runlet, and Local Tools

Kit exposes one model-visible tool, `compose`. A compose call contains a Runlet program that can invoke Kit's hidden tools, including `shell`, `edit`, `a2a`, subagents, Agent Skills, bundled documentation, and MCP meta-tools. Users normally ask Kit to do work through an installed-binary command such as `kit tui --root /path/to/project` or `kit prompt --root /path/to/project "Run the smallest relevant test"`; the agent writes the Runlet program. Use `kit --help` and `kit <command> --help` for the exhaustive CLI reference.

## How a compose Runlet works

A Runlet program is an immutable dataflow expression. Bind names with `=`, and end the program and every block with one `return`. Tool inputs and outputs are typed JSON-like values; Kit includes each hidden tool's exact input and output schema in the `compose` description. Missing required fields, extra fields, wrong types, and out-of-range values are rejected rather than silently corrected.

```text
result = shell({ command: "git status --short" })
return { ok: result.success, output: result.stdout }
```

Conditions are booleans, not truthy values. `if` evaluates only its selected branch. A `for` is for independent per-item work and may run its iterations concurrently; use `fold` when each step depends on the previous accumulator, such as a sum or cursor chain. `skip` drops a loop result. `assert(condition, message)` checks an invariant, and `fail(code, message)` raises a catchable error.

Use a boundary when a remote or otherwise transient operation needs local error handling:

```text
result = boundary retry 2 {
  return a2a({ url: endpoint, prompt: "Review this proposal" })
} catch err {
  return fail("REVIEW_FAILED", err.code + ": " + err.message)
}
return result
```

Retries repeat the body. Do not retry a write unless repeating it is safe or the destination is idempotent. Kit currently limits one compose program to 128 nested child tool calls; prefer focused programs and bounded loops.

## Run compose in the background

`background` is an optional argument on the outer `compose` tool call, not a Runlet expression. `true` starts the program in the background, a positive integer keeps it in the foreground for that many seconds before detaching it, and `false` or omission keeps it in the foreground. Delays are limited to integers from 1 through 86,400 seconds. Invalid values fail as tool input.

Background calls no longer hold their originating turn open, and interrupting that turn does not stop them. When a call detaches, the model receives its tool-call ID and can stop it with `close({ call_id: "call_..." })`. Cancellation is delivered through the same result lifecycle as completion, as a failed result reporting that tool execution was cancelled.

The TUI keeps every running call visible; select a tool card to inspect its runtime graph. Completion or failure is delivered back to the owning session and wakes the session loop directly without inserting synthetic user content. Background work is process- and session-scoped rather than a durable operating-system job, so closing Kit ends its inspectable lifetime.

## Ordering, dependencies, and concurrency

Independent calls run concurrently, including effectful `shell`, `edit`, `a2a`, and subagent calls. Source order alone does not sequence adjacent statements. This is intentional even at the top level:

```text
left = shell({ command: "check-left" })
right = shell({ command: "check-right" })
return { left: left.success, right: right.success }
```

A value reference creates a dependency when later work consumes the earlier result. When the later call must wait but does not need that result, use `after`:

```text
prepared = shell({ command: "./prepare-workspace" })
published = after prepared {
  return shell({ command: "./publish-workspace" })
}
return published
```

Calls lexically created inside the `after` block start only after `prepared` succeeds. If the prerequisite fails, dependent work does not run. Ordering one call does not make the whole program sequential; unrelated nodes may still overlap. Add explicit data dependencies or `after` edges around every required read-before-write or write-before-write relationship. In particular, do not launch concurrent edits of the same path or let a check race the command that creates its input.

## Activate Agent Skills

When valid skills exist under `<root>/.agents/skills` or `~/.agents/skills`, the hidden `activate_skill` tool lists their names and descriptions. If a task matches one, return the activation result through `compose` before proceeding so the instructions enter the model conversation:

```text
return activate_skill({ name: "review" })
```

Activation progressively discloses the skill's full `SKILL.md` body, directory, and resource paths. A hidden child result that is discarded by the Runlet is not separately added to the conversation, so do not call `activate_skill` without returning its value. The available-name schema is captured when the compose source is created; start a new session after changing the installed skill set.

## Run commands with `shell`

`shell({ command, timeout_seconds? })` runs with the canonical runtime root as its working directory. On Unix it uses `sh -lc`; on Windows it uses `cmd /C`. Standard input is null. The default timeout is 120 seconds, and accepted `timeout_seconds` values are 1 through 3600.

A normally completed command returns:

```text
{ exit_code, success, stdout, stderr }
```

A non-zero exit is still a completed tool result: inspect `success`, `exit_code`, and `stderr`. An exit caused by a signal may have `exit_code: null`. A timeout fails the tool with `shell command timed out`; cancellation of the Kit turn also stops waiting and attempts to kill the spawned command.

Stdout and stderr are captured separately and remain complete for downstream Runlet expressions and tool inputs. Each stream has a 64 MiB internal safety limit; exceeding it fails the shell call instead of substituting partial content.

Only the final compose return value crosses the model-context boundary. When its serialized form exceeds 8 KiB, Kit writes the complete result to `compose-output.json` under the call's artifact directory and returns `{ preview, artifact, original_bytes }` to the model. The preview contains bounded head and tail text separated by a `compose output spilled` marker. Compose final results have a 64 MiB safety limit. Return focused summaries when possible; inspect only a narrow artifact range when the complete final result is not needed in context.

The runtime root is a working directory, not an operating-system security boundary. A shell command can use absolute paths, `..`, the network, and any credentials or files allowed to the Kit process. Quote untrusted values, inspect destructive commands before running them, and avoid putting secrets into command text or returned output. There is no automatic rollback for shell side effects.

## Make exact file changes with `edit`

`edit` operates on one file path with `op: "add"`, `"edit"`, or `"delete"`. Relative paths are resolved from the runtime root. Absolute paths, `..`, and paths through symlinks are accepted, so `edit` can change files outside the root when the Kit process has permission. Paths must be non-empty.

An edit hunk replaces `old` while using optional `context_before` and `context_after` as its exact anchor:

```text
change = edit({
  op: "edit",
  path: "src/example.rs",
  hunks: [{
    context_before: "fn answer() -> u32 {\n",
    old: "    41\n",
    new: "    42\n",
    context_after: "}\n"
  }]
})
return change
```

The complete anchor must match exactly once. No match reports `hunk anchor did not match`; multiple matches report `hunk anchor is ambiguous`; an empty anchor reports `an edit hunk needs an anchor`. Inspect the current file and add distinctive nearby context instead of guessing. Multiple hunks are applied in listed order in memory, then the file is replaced through a temporary file and rename. Existing file permissions and CRLF line endings are preserved.

`add` fails with `<path> already exists` and creates missing parent directories. `delete` removes an existing file. Successful results are `{ path, status }`, where status is `added`, `edited`, or `deleted`. Each tool call is a single-file operation; a sequence of calls is not a transaction and has no cross-call rollback. Concurrent operations can still race, so order related edits explicitly.

## Send a task to an A2A v1 agent

`a2a({ url, prompt })` sends one user text message to a remote A2A v1 endpoint. It returns exactly one structured variant, `{ task: {...} }` or `{ message: {...} }`, according to the remote response. Branch on the present variant rather than assuming a text answer.

```text
reply = a2a({ url: "https://agent.example/a2a", prompt: "Summarize the risk" })
return reply
```

The child tool has no configurable request timeout. Cancelling the Kit turn cancels the wait; otherwise a slow remote may continue to hold the call open. Treat the endpoint as external: the prompt leaves the local project, and the returned object is remote input that should be validated before it drives commands or edits. A malformed URL, connection failure, protocol failure, or serialization failure is reported as a tool execution error. This outbound `a2a` tool is separate from the A2A listener started by `kit serve` or `kit tui`.

## Diagnose compose and tool failures

Start with the smallest failing Runlet and identify its failure stage:

- **Parse or Runlet error:** check immutable bindings, one final `return`, block returns, strict boolean conditions, and field names.
- **Invalid hidden tool input:** compare the call with the schema shown in the current `compose` description. For example, `shell({ timeout_seconds: 1 })` is invalid because `command` is required; an empty command or invalid timeout reports `command and timeout_seconds are outside bounds`.
- **Tool execution failure:** inspect the exact message. For shell non-zero exits, inspect the returned fields instead; for `edit`, re-read the path and correct a missing or ambiguous anchor; for `a2a`, verify the endpoint and connectivity.
- **Unexpected overlap or stale input:** source order is not an ordering guarantee. Add a consumed value dependency or an `after prerequisite { ... }` block.
- **Too much output or work:** look for `compose output spilled`, inspect only a focused artifact range, return a narrower final summary, bound loops, and split work before reaching the 128 nested child-call limit.
- **Interrupted turn:** cancellation propagates to running `shell` and `a2a` calls. Re-inspect project state before retrying because earlier effectful calls may already have completed.

For a Kit-specific error, ask the agent to search the bundled version-matched docs with the exact error text. For command-line syntax, use `kit --help` or `kit <command> --help`.

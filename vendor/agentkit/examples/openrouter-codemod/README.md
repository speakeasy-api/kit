# openrouter-codemod

A runnable example that uses the [`compose`](../../crates/agentkit-tool-compose)
Lua tool to perform a bulk codemod across a directory of files in a single
model turn. The agent is asked to rewrite every `println!(...)` call inside a
scratch directory to `tracing::info!(...)`.

## Why `compose` is the right tool here

A codemod is N small tool calls glued together by trivial data shaping:
list → read → transform → write, repeat per file. Doing that with one
tool-call-per-step bloats the transcript, multiplies the cost of every
intermediate model round, and forces the host to gate each step independently.

`compose` collapses the whole pattern into one tool call:

- The model writes a Lua script that loops over files and calls
  `tool(name, input)` for each filesystem operation.
- Data lives in Lua locals between calls; no JSON round-trip with the model.
- The permission policy still gates every child call — and if one of them
  needs approval, `compose` records the work done so far, surfaces the
  approval, and resumes exactly where it left off after the user decides.
  See `compose_nested_approval_surfaces_and_resumes` in
  `crates/agentkit-integration-tests/tests/compose_tool.rs`.

## How to run

```bash
cargo run -p openrouter-codemod
```

The example creates `target/codemod-demo/` with a handful of small `.rs`
files (deleting it first if it exists), then runs the agent against that
scratch directory only. After the run, the files have been rewritten in
place. Inspect them with `diff` or your editor.

## Required environment

Both come from the workspace `.env` (read by `dotenvy::dotenv`):

- `OPENROUTER_API_KEY` — your OpenRouter key.
- `OPENROUTER_MODEL` — the routing model id (e.g. `anthropic/claude-sonnet-4`).

## Example Lua script the model might generate

```lua
local root = input.root
local entries = tool("fs_list_directory", { path = root })
local files_changed = 0
local replacements = 0
for _, entry in ipairs(entries) do
  if entry.kind == "file" and entry.name:match("%.rs$") then
    local body = tool("fs_read_file", { path = entry.path })
    local rewritten, count = body:gsub("println!", "tracing::info!")
    if count > 0 then
      tool("fs_write_file", { path = entry.path, contents = rewritten })
      files_changed = files_changed + 1
      replacements = replacements + count
    end
  end
end
return { files_changed = files_changed, replacements = replacements }
```

Compose exposes the tool catalog through a global `tools()` function and the
caller-supplied JSON value through the global `input`. The model is free to
shape its own loop, error handling, and summary structure; the example just
asks it to return `{ files_changed, replacements }`.

## Permission policy: approval-replay in action

The example pins the path policy to the scratch directory:

```rust
PathPolicy::new()
    .allow_root(scratch.clone())
    .require_approval_outside_allowed(true)
```

On the happy path nothing in the compose script needs approval and the demo
runs end-to-end. If the model strays outside the scratch directory, the
nested child call surfaces a `LoopInterrupt::ApprovalRequest`; `compose`
records every completed child call so that resuming after approval replays
those without re-executing them, and only the gated call is actually re-run.
The demo treats any approval interrupt as a misbehaving model and aborts.

## Where to look in the code

- `src/main.rs` lines 87-103 — permission policy wiring (`PathPolicy` +
  `CommandPolicy` + `CompositePermissionChecker`).
- `src/main.rs` lines 78-83 — three tool registries (fs, shell, compose)
  merged into one `ToolRegistry`.
- `src/main.rs` lines 105-119 — the single user prompt that asks the model
  to perform the codemod in one compose call.
- `crates/agentkit-tool-compose/src/lib.rs` — the compose tool itself.
- `crates/agentkit-integration-tests/tests/compose_tool.rs` — end-to-end
  tests showing how compose surfaces and resumes from nested approval
  interrupts.

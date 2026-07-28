# Filesystem tools

A coding agent needs to read, write, and navigate files. This chapter covers `agentkit-tool-fs`: the built-in filesystem tools and their session-scoped safety policies.

## The tool set

`agentkit-tool-fs` ships seven tools:

| Tool                  | Description                                  | Annotations   |
| --------------------- | -------------------------------------------- | ------------- |
| `fs_read_file`        | Read file contents with optional line ranges | `read_only`   |
| `fs_write_file`       | Write or overwrite a file                    | `destructive` |
| `fs_replace_in_file`  | Find-and-replace within a file               | `destructive` |
| `fs_move`             | Rename or move a file                        | `destructive` |
| `fs_delete`           | Delete a file                                | `destructive` |
| `fs_list_directory`   | List directory contents                      | `read_only`   |
| `fs_create_directory` | Create a directory                           | —             |

All tools implement the `Tool` trait and can be registered with a single call:

```rust
let registry = agentkit_tool_fs::registry();
```

## Read-before-write enforcement

The most important safety feature in the filesystem tools is `FileSystemToolPolicy`:

```rust
let resources = FileSystemToolResources::new()
    .with_policy(
        FileSystemToolPolicy::new()
            .require_read_before_write(true),
    );
```

When enabled, the policy tracks which files have been read in the current session. A write or replace operation on a file that hasn't been read first is denied. This prevents the model from blindly overwriting files it hasn't seen — a surprisingly common failure mode.

The tracking state lives in `FileSystemToolResources`, which implements the `ToolResources` trait and is passed to tools through `ToolContext`.

## Permission preflight

Every filesystem tool emits a `FileSystemPermissionRequest` before execution:

```rust
pub enum FileSystemPermissionRequest {
    Read { path: PathBuf },
    Write { path: PathBuf },
    Edit { path: PathBuf },
    Delete { path: PathBuf },
    Move { from: PathBuf, to: PathBuf },
    List { path: PathBuf },
    CreateDir { path: PathBuf },
}
```

These structured requests let `PathPolicy` make informed decisions:

- Allow reads under the workspace root
- Require approval for writes outside the workspace
- Deny deletes of protected paths

## Read-before-write: why it matters

Without this policy, the model can — and routinely does — overwrite files it hasn't seen. The typical failure mode:

```text
Without read-before-write:

  User: "Add error handling to parser.rs"
  Model: ToolCall(fs_write_file, { path: "src/parser.rs", content: "... entirely new file ..." })

  The model hallucinated the file contents. The original code is gone.
  Any code that wasn't in the model's context window is lost.


With read-before-write:

  User: "Add error handling to parser.rs"
  Model: ToolCall(fs_write_file, { path: "src/parser.rs", content: "..." })
  → Denied: "src/parser.rs has not been read in this session"

  Model: ToolCall(fs_read_file, { path: "src/parser.rs" })
  → Success: file contents returned

  Model: ToolCall(fs_replace_in_file, { path: "src/parser.rs", find: "...", replace: "..." })
  → Success: targeted edit
```

The policy is session-scoped — the tracker resets when a new session starts. Reading a file once unlocks writes and edits to it for the remainder of the session.

## Implementation patterns

### `fs_read_file`

Accepts a `path` and optional `from`/`to` line numbers. Returns the file contents as text. Records the path as "read" in `FileSystemToolResources` for read-before-write tracking.

Line range support lets the model read specific sections of large files without consuming the entire context window:

```text
fs_read_file({ path: "src/main.rs", from: 50, to: 75 })
→ Returns lines 50-75 only
```

### `fs_replace_in_file`

Accepts a `path`, `find`, `replace`, and an optional `replace_all` boolean. Reads the file, performs the replacement, writes the result. This is the primary editing tool — it's more precise than full-file writes because the model only needs to specify the changed region.

The replacement is exact string matching, not regex. If the search text doesn't appear in the file, the tool returns an error. When `replace_all` is false (the default), only the first occurrence is replaced — this avoids accidental mass edits.

### `fs_write_file`

Writes or overwrites an entire file. Subject to read-before-write policy for existing files. New files (that don't exist yet) can be written without a prior read.

### `fs_list_directory`

Returns the contents of a directory. Useful for the model to explore project structure before reading specific files. Returns filenames and basic metadata (file vs directory, size).

### Error handling

Filesystem errors (file not found, permission denied, etc.) are returned as a `ToolResult` whose `ToolResultPart` has `is_error: true`. They are not panics or exceptions. The model sees the error message and can decide what to do — try a different path, ask the user, or give up.

```text
Error flow:

  fs_read_file({ path: "nonexistent.rs" })
  → ToolResult { result: ToolResultPart { is_error: true, output: "File not found: nonexistent.rs", .. }, .. }
  → Model: "The file doesn't exist. Let me check the directory structure..."
  → fs_list_directory({ path: "src/" })
  → Model finds the correct file name and retries
```

This is a key design principle: tool errors are part of the conversation, not exceptions. The model can reason about errors and recover, which is essential for autonomous operation.

> **Example:** [`openrouter-coding-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-coding-agent) uses the full filesystem registry to read, edit, and write files in a one-shot coding task.
>
> **Crate:** [`agentkit-tool-fs`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-fs) — depends on [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core) and [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core).

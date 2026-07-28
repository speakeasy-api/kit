# agentkit-tool-fs

<p align="center">
  <a href="https://crates.io/crates/agentkit-tool-fs"><img src="https://img.shields.io/crates/v/agentkit-tool-fs.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-tool-fs"><img src="https://img.shields.io/docsrs/agentkit-tool-fs?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-tool-fs.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Filesystem tools and session-scoped policies for [agentkit](https://crates.io/crates/agentkit).

This crate provides seven tools that let an agent interact with the local
filesystem, plus a policy layer that can enforce rules like "read before write."

## Tools

| Tool name             | Description                                      |
| --------------------- | ------------------------------------------------ |
| `fs_read_file`        | Read a UTF-8 file, optionally a line range       |
| `fs_write_file`       | Write UTF-8 text, creating parent dirs if needed |
| `fs_replace_in_file`  | Find-and-replace exact text in a file            |
| `fs_move`             | Move or rename a file or directory               |
| `fs_delete`           | Delete a file or directory                       |
| `fs_list_directory`   | List entries in a directory                      |
| `fs_create_directory` | Create a directory (and parents)                 |

Every tool declares a `ToolSpec::output_schema` describing the JSON it returns. Hosts and composing tools (e.g. `agentkit-tool-compose`) can read it via the tool's spec to drive validation or surface it to the model.

## Quick start

Get a `ToolRegistry` containing all filesystem tools with a single call:

```rust
use agentkit_tool_fs::registry;

let reg = registry();
assert_eq!(reg.specs().len(), 7);
```

## Policy configuration

`FileSystemToolResources` tracks which paths each session has inspected.
Combined with `FileSystemToolPolicy`, you can require that the agent reads a
file before it writes, replaces, moves, or deletes it:

```rust
use agentkit_tool_fs::{registry, FileSystemToolPolicy, FileSystemToolResources};

let resources = FileSystemToolResources::new()
    .with_policy(
        FileSystemToolPolicy::new()
            .require_read_before_write(true),
    );

// Pass `resources` as the ToolResources in your ToolContext so the
// filesystem tools can enforce the policy at invocation time.
let _ = resources;
```

## Using individual tools

Each tool struct implements `Default` and the `Tool` trait, so you can also
register only the tools you need:

```rust
use agentkit_tool_fs::{ReadFileTool, WriteFileTool};
use agentkit_tools_core::{Tool, ToolRegistry};

let reg = ToolRegistry::new()
    .with(ReadFileTool::default())
    .with(WriteFileTool::default());

assert_eq!(reg.specs().len(), 2);
```

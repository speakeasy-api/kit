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

The TUI keeps every running call visible. A running compose card shows its Runlet source inline, with live call states, binding resolution, and loop or retry counts. Completion replaces the source with the compose output. Unless the user explicitly opened or closed it, the output collapses when a later tool call or model message arrives and remains available from the tool card. Completion or failure is delivered back to the owning session and wakes the session loop directly without inserting synthetic user content. Background work is process- and session-scoped rather than a durable operating-system job, so closing Kit ends its inspectable lifetime.

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

## Load Agent Skills

When valid skills exist under `<root>/.agents/skills` or `~/.agents/skills`, the hidden `skill` tool lists their names and descriptions. If a task matches one, return the loaded skill through `compose` before proceeding so the instructions enter the model conversation:

```text
return skill({ name: "review" })
```

Loading progressively discloses the skill's full `SKILL.md` body, directory, and resource paths. A hidden child result that is discarded by the Runlet is not separately added to the conversation, so do not call `skill` without returning its value. Skills can be loaded repeatedly. The available-name schema is captured when the compose source is created; start a new session after changing the installed skill set.

## Run commands with `shell`

`shell({ command, timeout_seconds? })` runs from Kit's canonical working directory. On Unix it uses `sh -lc`; on Windows it uses `cmd /C`. Standard input is null. The default timeout is 120 seconds, and accepted `timeout_seconds` values are 1 through 3600.

A normally completed command returns:

```text
{ exit_code, success, stdout, stderr }
```

A non-zero exit is still a completed tool result: inspect `success`, `exit_code`, and `stderr`. An exit caused by a signal may have `exit_code: null`. A timeout fails the tool with `shell command timed out`; cancellation of the Kit turn also stops waiting and attempts to kill the spawned command.

Stdout and stderr are captured separately and remain complete for downstream Runlet expressions and tool inputs. Each stream has a 64 MiB internal safety limit; exceeding it fails the shell call instead of substituting partial content.

Only the final compose return value crosses the model-context boundary. When its serialized form exceeds 8 KiB, Kit writes the complete result to `compose-output.json` under the call's artifact directory and returns `{ preview, artifact, original_bytes }` to the model. The preview contains bounded head and tail text separated by a `compose output spilled` marker. Compose final results have a 64 MiB safety limit. Return focused summaries when possible; inspect only a narrow artifact range when the complete final result is not needed in context.

Kit's working directory is project context, not an operating-system security boundary. A shell command can use absolute paths, `..`, the network, and any credentials or files allowed to the Kit process. Quote untrusted values, inspect destructive commands before running them, and avoid putting secrets into command text or returned output. There is no automatic rollback for shell side effects.

## Read spilled output with `artifact`

Large compose results include an `artifact` path. Read it through `artifact`, which sees both persisted files and output temporarily held in Kit's memory filesystem. Shell commands only see real disk files.

```text
chunk = artifact({ path: output.artifact, offset: 0, limit: 1024 })
return chunk
```

The result contains `content`, `next_offset`, `total_bytes`, and `eof`. Continue from `next_offset` to read another chunk. Reads preserve UTF-8 character boundaries and are limited to 1,024 bytes per call. Only artifacts in the calling session's namespace are accepted; traversal and symlinks are rejected.

An artifact-storage error does not turn an already-completed tool into a failed tool call. Kit returns a bounded preview with `artifact_error` when output cannot be retained. Do not repeat a side-effecting tool merely to obtain its output again.

## Return images with `read_file`

`read_file` is a hidden compose callable, not another model-exposed tool. It imports a local image into Kit-managed storage and returns a small JSON **File reference**, not a path or a base64 string:

```text
image = read_file({ path: "screenshot.png" })
return { screenshot: image }
```

Only File references reachable from the final return deliver pixels to the parent model. A reference used only in an intermediate binding does not attach its image. Arrays and nested objects work; repeated references deliver one image, labeled with its first position as an escaped JSON Pointer. Every occurrence must have valid metadata, including duplicates. The whole selection is validated before any image is delivered.

The reader supports **nonanimated PNG and JPEG**. PNG compressed profiles (`iCCP`), compressed text (`zTXt`), and international text (`iTXt`) are rejected before decoding to prevent ancillary metadata expansion; this also rejects uncompressed international text. It sniffs content rather than trusting the extension, rejects corrupt images and nonregular files, and reads at most 8 MiB. An image can have at most 8,192 pixels on either axis, 16 megapixels, and a 64 MiB decoder allocation. GIF, WebP, animated PNG, SVG, PDF, URLs, and text files are unsupported. Use `shell` for ordinary text reads. Relative paths resolve from Kit's working directory; absolute paths follow the Kit process's filesystem access, not a new project sandbox.

Imports preserve the original bytes, metadata, and orientation. Width and height describe the encoded raster. There is no automatic transformation or export, and importing never overwrites the source.

### Transform images in one compose program

`image_rotate`, `image_crop`, and `image_resize` consume authorized File references and create new immutable references. `export_file` explicitly writes a reference's exact bytes to a new local file. All four are hidden callables; **compose remains the only exposed tool**.

```text
source = read_file({ path: "screenshot.jpg" })
rotated = image_rotate({ image: source, degrees: 90 })
cropped = image_crop({
  image: rotated,
  aspect_ratio: { width: 1, height: 1 },
  anchor: "center"
})
thumbnail = image_resize({ image: cropped, width: 256, height: 256, fit: "contain" })
receipt = export_file({ file: thumbnail, path: "thumbnail.png" })
return { thumbnail, receipt }
```

Only `thumbnail` delivers pixels in this example. Returning just `receipt` delivers no image. The export has a dependency on `thumbnail`; source order alone does not sequence independent compose calls. Export and transforms are effectful, including when their return values are unused.

Geometry is evaluated after normalizing EXIF orientation:

- **Rotate:** `degrees` is exactly `90`, `180`, or `270`, clockwise.
- **Crop:** ratio `width` and `height` are positive integers at most 8,192. Take the largest inscribed crop with an integer-rounded aspect ratio: retain one source dimension and floor the other. Reject a dimension rounded to zero. `anchor` is required: `center`, `top_left`, `top`, `top_right`, `left`, `right`, `bottom_left`, `bottom`, or `bottom_right`. Center offsets are floored, leaving an odd extra pixel outside the crop on the right/bottom. The rounded ratio need not be mathematically exact.
- **Resize:** `width`, `height`, and `fit` are required. `contain` preserves the ratio within the requested box, floors the shortened dimension, and adds no padding; zero-rounded dimensions fail. `cover` center-crops using the target's integer-rounded ratio, then resizes exactly to the target dimensions; rounding can cause slight ratio distortion. `stretch` directly resizes to the exact dimensions. All fits permit upscaling and use Triangle filtering.

Transforms support nonanimated PNG/JPEG inputs and emit fresh **RGBA8 PNG**, including when the input was JPEG. All eight EXIF orientations, including mirrored ones, are applied before geometry. The decoder either rejects malformed orientation metadata or falls back to identity. Source EXIF, ICC profiles, text, and other ancillary metadata are not copied. Stripping a profile is **not** color-managed conversion. Imports retain their original encoded dimensions and bytes; transformed descriptors describe the new raster. Source paths and existing managed objects are never overwritten.

Transforms enforce the reader's encoded, dimension, pixel, and decoder limits on input and output, plus a **128 MiB resize-scratch limit** and **256 MiB estimated live-pixel-work limit**. The Triangle implementation's intermediate buffer depends on source width × target height, so even narrow images can exceed scratch limits. These are allocation checks, not a total-process RSS guarantee; independent compose operations can run concurrently. Encoded output is capped at 8 MiB. Cancellation is cooperative at stage boundaries and during bounded writes; a running decoder/filter cannot be forcibly interrupted. Failure or cancellation can leave unreachable managed objects but returns no usable new reference.

### Explicit file export

`export_file({ file, path })` returns `{ path, size_bytes, status: "exported" }`, not a File reference. It writes the exact stored bytes without decoding, format conversion, or extension-based rewriting. Relative paths resolve from Kit's working directory. Absolute paths and parent symlinks use ordinary process OS authority, as with `shell` and `edit`; there is no project sandbox or ancestor confinement.

The destination's parent must already exist. Atomic create-new refuses any existing destination, including files, directories, and dangling final symlinks. It never overwrites an imported source. New Unix files are created with mode `0600` (subject to umask). Export bypasses volatile storage fallback: success requires disk writing and successful file sync. It does not promise atomic visibility or crash-durable directory creation.

Cancellation before creation produces no destination. Errors or cancellation after creation retain a potentially partial **or complete** destination and report that path; Kit does not unlink it because another actor could have replaced it. A successful file sync is the commit point, with no cancellation rollback afterward. Retrying the same destination fails until you explicitly deal with the existing file. Do not rerun export blindly after interruption or delivery failure.

### Delivery limits and provider support

A final compose return can select at most 8 distinct images, 16 MiB of encoded image bytes, and 32 megapixels in total. Selection traversal is bounded to 100,000 JSON nodes, depth 64, and 64 reference occurrences, with position labels bounded to 2 KiB each and 4 KiB in total. These limits are separate from the **8 KiB text-output budget**. Large returned JSON spills to a text artifact without hiding the selected image parts or their labels; media bytes do not enter the text artifact.

Kit retains typed image tool results in the canonical transcript. The verified `openai-subscription:gpt-5.4` route sends native image tool output. Other subscription models (including `gpt-6-astra`) and the OpenRouter/Speakeasy adapters use their ordinary user-image input encoding: the tool result keeps its text and points to a following user-role image message, placed after the complete tool-result batch. This message exists only in the outgoing provider request, not as a synthetic user turn in session history. Replay and provider switching project the retained images again without rerunning compose. This transport fallback does not imply that every model supports vision; select a model with image input support. Provider image limits and normalization still apply. Do not rerun a side-effecting compose program merely because output delivery failed.

The canonical tool result retains typed images. Supported terminal graphics render them in expanded tool cards using the existing bounded image cache; disabled graphics or decoding failures leave a text fallback. Displaying a tool image does not create a synthetic user message.

### File identity, durability, and lifetime

File descriptors reserve `"$kit": "file"` and use schema version 1. They contain an opaque ID, a bounded display name, MIME type, encoded byte count, and image dimensions. Unknown fields, unknown versions, altered metadata, missing objects, and inaccessible IDs fail rather than appearing as successful text-only image delivery. Do not edit descriptors or invent IDs.

Kit stores immutable snapshots under `~/.kit/files/<session-namespace>/` (or `<root>/.kit/files` when HOME is unavailable). A descriptor is returned only after the binary object and directory entries cross the disk durability barrier. Storage failure returns no usable descriptor. Each versioned binary envelope has a bounded metadata header and a digest-verified payload; truncated or corrupted objects are rejected.

References survive process restart and source modification or deletion. Authorization comes from the calling session, not from possession of a marker or an arbitrary filesystem path. Copying a descriptor to a fork or another session does **not** grant access. Cross-session grants are not part of this reader.

There is no automatic managed-file garbage collection in this phase. Calls, cancellation, session close, and process exit do not delete these objects. Cancelled or failed imports can leave unreachable objects. Explicit removal of a session's managed-file directory invalidates its references; do not remove retained objects that you need after resume. Finalization can repeat against the same immutable references without importing again. A delivery error states that the compose program already completed and side effects may have occurred; it is not a rollback or an invitation to retry blindly.

## Make exact file changes with `edit`

`edit` operates on one file path with `op: "add"`, `"edit"`, or `"delete"`. Relative paths are resolved from Kit's working directory. Absolute paths, `..`, and paths through symlinks are accepted, so `edit` can change files outside the root when the Kit process has permission. Paths must be non-empty.

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

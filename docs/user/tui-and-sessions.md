# TUI Interaction, Sessions, and Recovery

Kit's bundled terminal UI is an ACP v2 client backed by a persisted session. It supports prompt editing, active-turn steering, turn cancellation, transcript and tool-output navigation, fresh or resumed conversations, and automatic or manual context compaction. The session ID appears at the right of the TUI header on terminals wide enough to fit it; click it to copy the `kit tui --root … --resume <session-id>` command to the clipboard through OSC 52. When the TUI exits, it prints the Kit banner and the same resume command, so a session can be reopened without searching the session list. Run `kit --help` and `kit <command> --help` for the current, exhaustive command-line options.

## Start or resume the terminal UI

Start an interactive session at a project root with the installed binary:

```sh
kit tui --root /path/to/project
```

Open the session picker at startup, or resume a known ID directly:

```sh
kit tui --root /path/to/project --resume
kit tui --root /path/to/project --resume <session-id>
```

Without an ID, `--resume` opens the same workspace-scoped, newest-first picker as `/sessions`, with the same session names, selection, and inline rename interactions. It resolves `--root` and configured root defaults just like normal startup. No new persisted session is created to display the picker; a session is resumed only after selection. Plain `kit tui` still starts normally.

At the top-level picker, `Esc` or `Ctrl+C` cancels startup and exits successfully without creating or resuming a session; `Esc` during inline rename only cancels the rename. An empty catalog reports that the workspace has no resumable sessions and exits successfully. Catalog read failures report an actionable error and exit unsuccessfully. If the selected session disappears, becomes invalid, or is locked before resume, Kit reports the resume error rather than starting a new session.

You can also list and rename sessions from the command line, then resume the ID shown in the header or catalog:

```sh
kit sessions --root /path/to/project
kit sessions rename <session-id> "OAuth token bug" --root /path/to/project
kit sessions rename <session-id> --clear --root /path/to/project
kit tui --root /path/to/project --resume <session-id>
```

The catalog requires an existing directory and is workspace-filtered and newest-first. It reports each durable top-level session ID and updated time; sessions created as subagents are omitted based on structured origin metadata in their initial transcript. Sessions created before Kit recorded that metadata remain visible. Filtering affects discovery only; a known omitted ID can still be resumed explicitly. The generated title comes from the earliest retained useful user text so compaction does not rename a session; the preview describes the current retained history. A custom display name overrides that title in CLI, TUI, and ACP listings without changing the immutable session ID or preview. Names may contain Unicode, are trimmed, may be at most 100 characters, and may not contain line breaks or terminal control characters. Use `--clear` to restore the generated title.

A session ID must be 1–128 ASCII letters, digits, `-`, or `_`. `kit prompt` uses the same durable sessions: it prints `session_id: <id>` after its answer, and that ID can be continued by either `kit prompt --resume <session-id>` or `kit tui --resume <session-id>`.

## Recovering from full storage

If Kit reports a memory-only storage warning, **keep Kit running and free disk space or resolve the storage quota**. Kit temporarily retains accepted internal changes in bounded memory and retries persistence automatically, including while idle. The TUI keeps the warning visible until recovery completes; stderr also reports degradation and recovery. You can free space through a normal tool call without restarting the session.

**Pending memory state does not survive process termination and is invisible to other processes.** Finishing a turn or switching sessions in the same process does not discard it, but restarting Kit can. Wait for the recovery notice before exiting if you need those changes to be durable.

On normal exit, Kit makes a final recovery attempt and warns with an unsuccessful exit if changes remain unpersisted. A shutdown timeout can force termination with possible data loss. Temporary storage is limited to 64 MiB and 4,096 pending operations; reaching either limit requests cancellation and orderly shutdown. An allocation failure can instead force immediate exit, so do not rely on memory-only operation for continued work.

This fallback protects Kit's internal state, not arbitrary tool writes. Explicit `edit` and shell operations still use the real filesystem and report failures; plugin subprocesses also need their inputs on disk. Storage recovery does not bypass permissions or another process's session lock.

Use the `artifact` tool to read spilled output, including memory-only artifacts; a shell cannot read memory-only output.

## TUI keys, prompt editing, and navigation

| Key or input | Action |
| --- | --- |
| `Enter` | Send a non-empty prompt when idle; steer and finish the current response while active if the agent advertises that capability |
| `Shift+Enter`, `Option+Enter`, `Ctrl+J` | Insert a newline |
| `Esc` | Cancel a pending-message edit and restore the previous draft; leave the queue selector; otherwise interrupt a running turn or dismiss an idle notice |
| `F2` | Focus the pending-message queue (or return to the composer) |
| `Up` / `Down`, `Enter`, `Backspace` / `Delete` in the queue | Select a pending message, edit it if supported, or remove it |
| `Command+B` | Move the newest running foreground top-level compose call to the background |
| `Ctrl+C` | Interrupt a running turn; clear a non-empty idle prompt; quit when idle with an empty prompt |
| `Ctrl+D` | Quit when the prompt is empty |
| `Option+Left/Right`, `Ctrl+A`/`Ctrl+E`, `Home`/`End` | Move by word or to the start/end of a line |
| `Option+Backspace`, `Ctrl+W` | Delete the previous word |
| `Command+Backspace`, `Ctrl+U` / `Ctrl+K` | Delete to line start / end |
| `Up` / `Down` | Move through prompt lines, then prompt history |
| `Shift+Up` / `Shift+Down`, `PageUp` / `PageDown`, mouse wheel, drag the scrollbar | Scroll the transcript |
| `Ctrl+Home` / `Ctrl+End` | Jump to transcript top / bottom |
| `Ctrl+R` / `Ctrl+L` / `Ctrl+T` | Toggle the agent roster / agent log / reasoning |
| `Ctrl+K` | Kill the selected running background tool call; otherwise delete to the end of the editor line |
| `Ctrl+O`, or click a tool card | Fold or unfold raw tool output |
| `Ctrl+Y` | Copy the drag selection, or the latest agent response as original Markdown |
| `Command+C` | Copy the drag selection when the terminal forwards the shortcut |
| Click a fenced code block | Copy its contents without the backticks or language label |

`Command` and `Shift+Enter` require a terminal with the Kitty keyboard protocol, such as Ghostty, Kitty, WezTerm, or recent iTerm2. The control-key alternatives work without that protocol.

Drag over the transcript to select text within Kit. A press on the scrollbar scrolls instead: drag its thumb, or click the track to jump there. `Command+C` copies that selection when the terminal forwards the shortcut through the Kitty keyboard protocol; if the terminal intercepts it for native Copy, use `Ctrl+Y` instead. Selection copying rejoins wrapped lines and removes code-block gutters. Shift-drag uses the terminal's own selection instead. While text is selected, the footer shows `Ctrl+Y` to copy; after Kit receives a `Command+C` keypress, it shows `⌘C` instead. Keyboard-protocol support alone cannot establish whether the terminal forwards that specific shortcut.

`Ctrl+Y` copies with the terminal's OSC 52 clipboard protocol. It preserves the agent's original Markdown, whitespace, and newlines instead of copying rendered TUI borders, list glyphs, or wrapped lines. Clipboard access must be enabled in the terminal; multiplexers such as tmux may also require OSC 52 passthrough.

Pasted text is inserted rather than sent. Bracketed paste is used when available; otherwise Kit treats a rapid key burst as a paste, so returns in that burst become line breaks. This keeps a multiline paste in one prompt. Press plain `Enter` afterward to submit it.

When the session is idle, `Enter` starts a normal prompt. While the agent is active, `Enter` uses ACP v2 `steer` injection with `finish` stream behavior only when the agent advertised both capabilities. An accepted injected user message first appears in the pending queue above the composer. It moves to the transcript when the agent delivers it as part of the current turn. If steering is unavailable, the editor keeps the message and shows `this agent does not support active steering`. Local commands and agent-advertised session commands are available only while idle.

### Edit or remove a pending message

Press `F2` to focus the queue, then `Up` or `Down` to select a message. The queue opens only when at least one message is pending; otherwise Kit shows `no pending messages` and leaves you in the composer. The selected row stays visible even when the queue is long. Press `Enter` to edit its text in the composer, or `Backspace` / `Delete` to request removal (the normal Mac Delete key works). `Esc` or `F2` returns from the selector without changing your draft. When the last message is delivered or removed, or the turn finishes, the selector closes automatically and keyboard focus returns to the composer. An in-progress text edit remains available to copy or cancel rather than being discarded.

After automatic closure, typing, pasting, or moving the composer cursor works normally. Until that fresh composer interaction (or `Esc` to acknowledge focus), Enter and deletion keys are ignored with a visible hint, so a key intended for the disappearing queue cannot send or delete your parked draft. An empty-queue F2 attempt does not enable this safeguard.

While editing, plain `Enter` saves the replacement with the **same message ID and queue position**. `Esc` cancels the edit. Either a successful save or cancellation restores the previous composer draft, cursor, and attachments. Queue edits do not run slash commands. Editing is available only when the agent advertises ACP `session.inject.pending.replace`; removing a pending injection does not require this optional capability.

Save and removal requests run without blocking the UI; updates and `Ctrl+C` cancellation remain available, including when delivery empties the selected queue. Only one change per message can be in flight. You can continue typing or press `Esc` while a save is in progress; `Esc` restores your draft but does not undo a request already sent. A save response never closes a reopened edit or discards text typed after that save began.

If a request fails, Kit retains the replacement text and the parked original draft. You can retry, or press `Esc` to restore the original. If the message has already been delivered or no longer exists, it cannot be changed or removed: the stale queue entry disappears and any in-progress edit stays available to copy before you press `Esc`. A late response never adds that entry back to the queue. Starting or resuming another session clears the queue, selection, and edit state.

Active steering continues to support media attachments. Pending-message **edits are text-only**: messages accepted with image, audio, or other non-text content cannot be edited in the queue, but can still be removed. Kit determines editability from the content actually sent, not stale attachments left in the composer. Media paths pasted during a queue edit are ordinary text, not new attachments. Attachments in the parked original draft remain unchanged when you save or cancel.

### Assistant image previews

Assistant Markdown images, such as `![Chart](charts/result.png)`, display a static image preview when the terminal supports Kitty, Sixel, or iTerm2 graphics. Absolute local paths, paths relative to the session project root, `file://` URLs, and HTTP(S) URLs are supported. Inline images keep their position in the message: text before the image appears before its preview, and text after it appears below. Image syntax inside fenced or inline code stays literal; incomplete streamed syntax is not fetched.

Kit automatically reads local image files and fetches remote images when their preview scrolls into view, without a permission prompt. Remote requests follow at most five redirects and have a ten-second total timeout (three seconds to connect). A single background worker uses a bounded queue for fetching and decoding, with a 10 MiB source limit, dimension and decoded-allocation limits, and a bounded preview cache. Cached images are reused across redraws; successful previews can be fetched again after cache eviction. Failed previews are not retried during the current image-runtime lifetime. Starting another session clears the cache.

The Markdown text or structured image label remains visible while loading and if reading, fetching, or decoding fails. Unsupported terminals show text only and do not fetch Markdown images. Previews reserve a fixed-height viewport, show only the first frame of animated images, and are downscaled to at most 1600 × 1200 pixels before terminal encoding. Reference-style Markdown images are not expanded.

### Attach local images and audio

Drag one or more supported local media files into the terminal while editing a prompt. Terminals deliver a drop as pasted, shell-escaped paths rather than as a dedicated file-drop event. Kit treats the paste as attachments only when every parsed token resolves to a supported regular file. Mixed text and paths, unsupported files, invalid shell quoting, missing files, and ambiguous input remain ordinary pasted text. There is no `/attach` command.

Press `Ctrl+V` to read the native clipboard explicitly. If it contains an image, Kit saves a temporary PNG and attaches it; otherwise Kit pastes clipboard text. Kit also recognizes `Ctrl+Shift+V` and `Shift+Insert` when the terminal forwards those keys. On macOS, `Command+V` works when the terminal forwards the Command key through the Kitty keyboard protocol. VS Code's integrated terminal can instead send an empty bracketed paste for an image-only clipboard; in the main composer, Kit responds by checking only for a native clipboard image, never falling back to clipboard text. If no image is available, the empty paste remains a no-op. A terminal that sends neither a key event nor a paste event cannot trigger native image paste; use `Ctrl+V` or paste a saved image's file path instead. Non-empty bracketed text paste remains terminal-delivered and never causes a separate clipboard read. Native clipboard access is supported on macOS, Windows, and X11; native Wayland sessions need XWayland clipboard interoperability. Clipboard images cannot be added while editing a pending queue message, and paste shortcuts respect modal and queue-focus ownership.

Supported files are PNG, JPEG, GIF, WebP, WAV, and MP3. Up to 8 attachments can be pending, each file can be at most 10 MiB, and their combined size can be at most 20 MiB. Clipboard pixel data is limited to 64 MiB before PNG encoding. Kit checks the actual file sizes again when the prompt is submitted.

An accepted file appears in the editor as `[Image #N]` or `[Audio #N]`. Add surrounding prompt text normally, or delete a placeholder to omit that file from submission. Unsent numbers are reused after deletion; accepted attachments keep their numbers for the session, including after resume, so later attachments continue the sequence. Sent image placeholders are real OSC-8 terminal hyperlinks. Use your terminal's open-link gesture (typically Command-click on macOS, Ctrl-click on Linux/Windows) to open them. VS Code's integrated terminal can open local image links in an editor tab; other terminals choose their configured file handler. Ordinary unmodified clicks are also handled by Kit through the platform's default application when terminal mouse reporting is available. A new session clears pending attachments. If Kit cannot read or validate the files when constructing the request, it shows a notice and retains the pending attachment records. Restore or re-enter the prompt placeholders before retrying.

If you press `Enter` while native clipboard pastes are pending, submission waits for those pastes without blocking the terminal. A failed paste or further editing cancels automatic submission and keeps the draft available. Clearing the prompt or switching sessions discards stale clipboard results so they cannot populate another draft.

Native clipboard reads, clipboard image encoding, and reconstructed-image validation run on a bounded background worker so they do not pause terminal input or redraws. Clipboard files and reconstructed image links use bounded, session-owned temporary storage. On resume, Kit reconstructs local image links from available message content without reading stale paths or downloading URLs. The stable reconstructed-link cache holds at most 64 files / 64 MiB. If that budget is exhausted but image bytes are still retained, the underlined placeholder remains openable with an ordinary Kit click: a bounded worker validates and materializes it on demand. These overflow placeholders do not support terminal modifier-click / OSC-8 opening. Opened overflow files are reused by image identity and retained until session switch or exit, so opening another image does not invalidate a file already handed to a viewer. This separate, non-evicting pool holds at most 64 files / 32 MiB (matching the retained image-source byte budget), with the unchanged 10 MiB per-image limit. Once the pool is full, Kit shows a notice instead of removing older files; previously opened images remain openable. Starting a new session releases these budgets. Retained base64 image sources are separately capped at 32 MiB; exceeding that limit shows a notice, and labels without either retained bytes or a stable link cannot be opened on demand. Invalid or unavailable content cannot be opened; Kit shows a notice if an on-demand open fails. Kit deletes its temporary files on session switch or exit; original files you attached by path are not deleted.

Terminal compatibility: OSC-8 links are understood by current VS Code, Ghostty, Kitty, WezTerm, iTerm2, and many other terminals, subject to their link settings. For older terminals, use Kit's ordinary-click fallback. Multiplexers such as tmux must support or pass through the keyboard, paste, and hyperlink sequences; terminal-native shortcuts can differ from a direct session. Clipboard access and `file://` paths are local to the machine running Kit: over SSH, Kit cannot read your laptop's clipboard or make remote temporary files accessible to your local editor. Transfer an image to the remote host and paste its path instead.

The model-facing prompt retains canonical `file://` Markdown links, while Kit also reads and sends the file bytes because remote providers cannot access local files. Image and audio acceptance remains model-dependent. Kit supports these request shapes through OpenRouter and OpenAI subscription; an individual model can still reject a modality it does not support. Video is not supported.

User-attached images remain compact attachment labels without inline previews. Structured assistant and tool images use the static preview renderer; animated GIF and WebP previews show only their first frame. Audio is not played. Only bounded `file://`, `http://`, and `https://` links are displayed; base64 and `data:` URLs are never copied into terminal text or Markdown links.

### Interrupt a running turn or quit

Press `Esc` or `Ctrl+C` once to request cancellation. The TUI shows `interrupting the turn`, then records `turn interrupted` when cancellation completes.

If a turn does not stop, press `Ctrl+C` again while Kit is cancelling to leave the TUI and terminate its agent child. On normal exit during a turn, Kit first requests cancellation and briefly allows the turn to unwind so tool outcomes can be persisted, then closes the session and releases its lock.

Press `Command+B` to detach the newest running foreground top-level compose call without waiting for it to finish. This shortcut requires a terminal that reports the Command key through the Kitty keyboard protocol; it has no control-key equivalent. Interrupting a turn does not stop detached background calls. Select a running background tool card and press `Ctrl+K` to kill only that call; the selected title is accented and shows `^k kill`. When a background result starts an autonomous agent continuation, the TUI displays it as an active turn, and `Esc` or `Ctrl+C` interrupts it normally.

At an idle, non-empty editor, `Ctrl+C` clears the prompt instead of unexpectedly discarding it and quitting in one step; press it again with the empty editor to quit.

## Answer agent questions

When an agent needs a decision, Kit can show a form with single-choice or multiple-choice options and optional custom-answer fields. The form appears in a bounded panel below the conversation, keeping the surrounding context visible. Answers stay local to the form until you submit the review page.

- Use `Up` / `Down` to highlight an option and `Space` to select it. For multiple-choice fields, `Space` toggles each option independently.
- Text and numeric fields use the composer editor: move the cursor with arrow keys, use `Alt+Left` / `Alt+Right` to move by word, and `Alt+Backspace` or `Ctrl+W` to delete the previous word. `Ctrl+U` / `Ctrl+K` delete to the start / end of the line. `Shift+Enter` inserts a newline; `Ctrl+V` / `Shift+Insert` and bracketed paste insert text at the cursor. Clipboard images are not accepted in form fields. Use `Left` / `Right` to choose false / true for a boolean field, or `Space` to toggle it.
- When a question offers **Other**, its text input appears below the choices on the same page. Press `Tab` to move from the choices to Other, then to the next question. You can keep selected options and an Other answer together, or enter only an Other answer.
- Press `Tab` to move forward through inputs and questions to review, even when an answer is blank. `Shift+Tab` moves backwards through the same inputs: from Other to choices, or from choices to the previous question’s last input. Neither key validates answers or submits the form; drafts are preserved. Unrelated form fields remain on separate pages.
- Press `Enter` to validate the current question and advance. A required choice question must have a selection or non-blank Other text.
- On the review page, press `Enter` to validate and submit all answers together, or `Shift+Tab` to revise them. Skipped required answers return you to the input that needs attention. `Tab` on review stays there without submitting.
- Press `Delete` to clear a choice or boolean field; in text fields it deletes forward from the cursor. Use `PageUp` / `PageDown` to scroll the question or review.
- Press `Esc` to dismiss the request, or `Ctrl+D` to explicitly decline. Neither action submits selections.

Do not enter passwords, API keys, or other credentials in these forms. Kit supports session-scoped ACP form elicitation, not URL elicitation. Unsupported form constraints are rejected rather than silently ignored. If your ACP client does not support forms, ask and answer in ordinary chat instead.

## Read the transcript

The transcript keeps a three-column gutter: `›` marks your prompts, `⋮` reasoning, a spinner, `✓`, or `✗` each compose program, `⇅` a compaction, and `↩` a background result landing after its turn ended. Prose has an empty gutter, so the left edge alone shows the turn's shape.

Each turn ends on a faint line with the wall time since your last prompt, for example `· 4m12s`. Turns restart on their own when a detached program's result lands, so the clock spans those autonomous turns rather than restarting with them; steering does not reset it. While detached programs are still running when a turn ends, the line says the work is not finished: `· ◔ 1 in background · 4m12s so far`. When such a program finishes after its turn ended, a `↩` line marks where its result landed, with the program's title and how long it ran; the program's own card stays where it started.

A running compose program lists its nested calls as lanes beneath its title: the call's kind (`shell`, `edit`, `subagent`, ...), a bar on the program's clock showing when the call started and how long it ran, and its duration. Concurrent calls read as overlapping bars. The header carries the call count, elapsed time, and a `▸ N lines` fold for the program's output; click the card or press `Ctrl+O` to open it, then again to view the Runlet source and again to close. A finished program folds its lanes behind the call count until opened. The runtime does not report binding, loop, or retry state, so a lane appears only once its call has started; the source view is the honest view of the rest.

Reasoning folds to one line while it streams, labelled with its latest heading; `Ctrl+T` expands every reasoning block. Compaction appears as a `⇅ compacted context` block with its duration and reason.

The dock above the composer holds everything alive outside the current turn: a `◔ background` row per detached program (with `^k stop` when that program is selected), the agents summary on narrow terminals, and each `⇥ steer` waiting for injection. The dock shows at most four rows; with more alive it windows around the selected entry (the focused background program, or the selected steer while the queue has focus) and one row says how many are hidden. The composer's frame title says `steer · turn N` while a turn runs and `message` when idle. The status line's key hints change with the state and only list keys that work right now.

## Monitor subagents in the agent roster

Press `Ctrl+R` or enter the exact local command `/agents` while idle to toggle the agent roster. The roster keeps its own visibility and scroll position. The roster covers every subagent observable to the current top-level Kit process tree, whether its call is foreground or background and regardless of the focused transcript block or tool call. Direct children are tree roots, and nested Kit descendants appear immediately beneath their parent at arbitrary depth in an always-expanded tree. Siblings retain lifecycle/creation/ID ordering within each parent, so an active child remains grouped beneath an idle parent instead of moving across subtrees. A descendant whose parent event has not arrived temporarily appears as a root with `Name · via Parent` and automatically reparents when the parent arrives. Generic ACP has no portable child-session enumeration, so agents created privately inside a generic harness cannot appear unless the harness forwards compatible Kit runtime events.

Press `Ctrl+G` to move keyboard focus to or from the root agent roster. Use `Up` and `Down` to select an agent and `Enter` to inspect it, or click its row. The selected row is highlighted. On narrow terminals, keyboard roster focus exposes a compact, selectable roster.

The inspected child's view shows its name, handle, generation, and capability or lifecycle notice above its transcript. The root roster stays visible so you can select another child. Press `Esc` or click **Back to main** to return without interrupting either turn. Use `PageUp` / `PageDown`, `Home` / `End`, or the mouse wheel over the child transcript to scroll; the roster has independent scrolling.

The child composer accepts **text steering only**, and only for an active direct child that supports compatible steering. Descendants remain visible in the root roster, but their transcript route is currently unsupported and the inspection view says so; Kit does not substitute a parent's transcript. Steering is routed only to direct children. Idle, closed, unsupported, or unavailable children are also read-only; the notice explains availability and errors. There are no attachments or slash commands in the child composer. Root session commands, model changes, cancellation, and other root actions are not available from this view. Return to main to use them; your root draft is retained while you inspect children.

Opening a child loads its available inspection history and follows live updates. Switching children or returning to main stops those reads without changing the root transcript, draft, or scroll position. Reopening a child reloads its available history.

Inspection history is temporary, not a durable session archive. If a read fails, return to main and reopen the child to retry; this does not recover records already omitted because of storage failure. Inspection failures do not stop child execution or roster updates.

After reconnecting, the child may supply only partial history. Native forks do not include inherited transcript history in this view. Kit marks incomplete history and omitted oversized updates explicitly; submitted prompts and harness-reported echoes may both appear. Use the child inspection view to monitor work, not as a complete audit record.

Use each roster row to check:

- **Status:** `starting` means the child is being prepared; `working` means a turn is running; `idle · resumable` means the parent can continue it with `prompt`. A red `✗` marks a recent failure. A failed turn does not necessarily make the child unusable; inspect the error before continuing.
- **Identity and progress:** the name, harness, model, and generation (for example, `g2`) identify the child and its current turn. The activity excerpt describes its current work. Elapsed time stops when the turn finishes and starts again for a later prompt.
- **Usage:** context usage and any reported cost help you monitor resource consumption. Availability depends on what the child harness reports.

The footer summarizes live agents, for example `3 agents · 2 working · 1 idle`, whether their calls run in the foreground or background. Idle reusable children remain listed until closed; closed or retired children leave the live count. To continue an idle child, return to main and ask the parent to prompt it. For recovery after a Kit restart, see [Resume delegated work](subagents-and-acp-harnesses.md#resume-delegated-work-after-restarting-kit).

Wide terminals show the roster beside the transcript. Narrow terminals show a compact summary above the composer; use `Ctrl+G` to select an agent. Hiding the roster gives the transcript the full main area.

## Manage sessions and compact from the TUI

The TUI handles `/new`, `/resume`, `/sessions`, `/close`, `/model`, `/effort`, `/usage`, and `/agents` as exact local slash-command tokens. It also discovers agent commands through ACP and highlights them without interpreting them locally:

```text
/new
/new Start by reviewing the tests
/resume <session-id>
/sessions
/close
/compact
/compact Continue with the migration
/model
/effort
/effort high
/usage
/usage openrouter
/agents
```

`/usage [provider]` displays provider usage and quota as local informational output, using the credential storage and OpenRouter key resolved when this TUI launched. It does not send a model prompt or query a remote agent’s credentials. The check runs in the background, so input and model updates remain responsive. It is available while idle or during a turn; repeated requests are ignored while a check is in progress. Omit the provider to check authenticated supported providers. See [provider usage](getting-started-and-configuration.md#provider-usage).

The other commands listed above are available only while the session is idle. `/agents` toggles the agent roster without starting a model turn. `/new` closes the current session and starts a fresh persisted session. It clears the visible transcript but does not delete or alter the previous session, which remains resumable by its ID. Text following `/new` becomes the new session's first prompt. `/resume <session-id>` closes the current session, resumes the requested durable session, and replays its transcript; selecting the already-active ID is a no-op. `/sessions` opens a visible newest-first selector for the same workspace. Up and Down move, Enter uses the existing resume flow, `R` opens an inline rename field, and Esc cancels renaming or closes the dialog. Submit an empty rename and confirm to clear the custom name. After a save, the picker remains open on the selected session and refreshes its displayed name. `/close` closes the current session and exits the TUI.

`/model` opens the model selector on a provider list; Enter on a provider shows that provider's models. Once the session is open, providers that Kit can sign in to but that have no credentials yet are listed greyed out below the signed-in ones. Enter on one starts its terminal sign-in, the same flow as `/login <method>`, and is available only while the session is idle; after a successful sign-in, the selector reopens on that provider's models with the same Tab setting. Typing searches only the current page, and the search clears when you move between pages. Esc returns from the model list to the provider list, and closes the selector from the provider list. `/effort` opens the advertised ACP reasoning-effort selector; `/effort default|low|medium|high` selects directly. In either dialog, Tab toggles saving the selection to `~/.kit/config.toml`, Enter selects, and Esc closes. Saving `default` removes top-level `reasoning_effort`; other values update it without replacing unrelated TOML. New sessions start from the resolved CLI/TOML default. Durable sessions remember their reasoning-effort selection, including an explicit provider `default`, and restore it on load or resume even if the process default has changed. Older transcripts without a recorded effort continue to use the process default. Forks inherit the source session’s current effort. Saving to TOML changes the default for future sessions; it is not required to retain the effort of the current session. Model selection still starts from the resolved CLI/TOML default on reload. Reasoning-effort records require a Kit version that understands transcript schema 6; older versions reject these records rather than silently discard the saved selection.

Before changing models, Kit compares the latest provider-reported transcript occupancy with the target model's advertised context window. It adds a 20% tokenizer margin (`ceil(tokens × 1.20)`) and warns when that estimate is at least 80% of the target window. This is the latest request's occupancy, not accumulated session usage; cached input tokens are not counted twice.

The TUI warning offers **Continue anyway**, **Compact**, and **Cancel** (the default). Continue switches without compacting. Compact runs the existing compactor with the **original model** and applies the new selection only after successful completion and durable transcript replacement. A failure or cancellation keeps the original model selected; compaction can already have changed the transcript if cancellation arrives after replacement. Cancel dismisses the warning without changing the model or transcript, and preserves the input draft. Esc cancels an in-progress switch; repeated Esc is harmless, and Ctrl+C while cancellation is pending exits if it is stuck. Stale confirmations are rejected when the session, selected model, target, or transcript changes.

If the latest token count or target context window is unavailable (including fallback/custom models without catalog metadata), Kit allows an **unchecked switch** rather than inventing a limit or forcing compaction. The estimate is a warning, not a guarantee that the next provider request fits.

External ACP clients receive a confirmation-required error instead of a Kit dialog. Its extension key in error data is `kit.model_switch`; clients can resubmit the same configuration request with `_meta: {"kit.model_switch": {"token": <returned token>, "action": "continue"}}`. Both ACP versions also accept `"action": "compact"` to compact transactionally with the original model. Confirmation tokens are session-local, in-memory, one-shot decisions, not persisted configuration.

The ACP server advertises `compact` for every new session. The TUI submits `/compact` unchanged like any other prompt; the runtime consumes exactly one text part beginning with the exact raw token before model dispatch and permits other client-provided context parts. Used alone, it ends after compaction. Whitespace-trimmed text following `/compact` and any other context parts are retained as the latest user message and start the next turn after compaction. Leading whitespace, near-misses such as `/compactness`, prompts containing multiple `/compact` command parts, and unknown slash commands remain ordinary prompts. Local commands win if an advertised command has the same name.

## Persisted transcripts and session files

Kit stores durable JSONL transcripts, locks, and session-associated fatal error logs in:

```text
~/.kit/sessions/w-<workspace-hash>/<session-id>.jsonl
~/.kit/sessions/w-<workspace-hash>/<session-id>.lock
~/.kit/sessions/w-<workspace-hash>/<session-id>.metadata.json
~/.kit/errors/<session-id>/<event-id>.json
```

The workspace hash is the BLAKE3 digest of the canonical workspace-root path. It keeps identical session IDs in different workspaces in separate storage directories. The optional metadata sidecar stores only the custom display name, is replaced atomically, and does not modify or lock the append-only transcript. Missing or malformed metadata falls back to the generated title without hiding the session.

Fatal error records use their own versioned JSON schema and are not transcript content. Schema v2 adds optional structured transport diagnostics; schema v1 records remain readable. Transport diagnostics contain only bounded, allowlisted request/stream stage, retry, attempt, the provider's strictly validated `x-request-id` value, reqwest classification, and typed Hyper, HTTP/2, and I/O fields. Unknown or truncated source chains are identified without storing source text. Kit never stores raw error display/debug text, arbitrary headers, prompts, tool arguments, response bodies, credentials, URLs, or peer-controlled HTTP/2 debug text in these records. Files are written atomically with owner-only permissions on Unix, and Kit retains the newest 50 records per session. Cancellation is not a fatal error and does not create a record. When persistence succeeds, local prompt and ACP terminal errors include the log path; A2A records stay server-local.

`HOME is unset; cannot locate durable sessions` means Kit cannot determine this directory. Set `HOME` to the intended home directory before starting Kit.

Transcript records are versioned and have consecutive generations. Transcript schema v3 records the canonical workspace root so ACP discovery and resume cannot expose a session to another project; schema v1 and v2 records remain readable and gain that binding when they are next resumed. Normal items are appended and synced to disk before they are accepted into the in-memory conversation. Operations such as compaction append a replacement record; older records remain in the JSONL file, but readers treat the latest valid replacement as the canonical transcript.

Older sessions stored directly under `~/.kit/sessions` or under `<root>/.kit/sessions` remain readable. On resume, Kit compares workspace-hashed, workspace-bound global, and project-local candidates and selects the history that descends from the others; equivalent histories prefer the workspace-hashed copy, while divergent histories fail instead of choosing silently. An old global transcript without workspace metadata is a fallback only for an explicit resume when no workspace-hashed or project-local candidate has that ID. The first successful resume copies the authoritative history into the workspace-hashed directory and leaves redirects in applicable legacy files. A live legacy lock produces `legacy session is actively locked by another Kit instance ...; stop it before resuming with this Kit version`.

### ACP session loading

ACP v1 clients restore a closed durable session with `session/load` and discover sessions with the optional `session/list` capability. ACP v2 clients use `session/list` and `session/resume`. Both list variants use the same newest-first catalog, optional exact-cwd filter, and opaque `offset:<n>` pagination cursors, and return titles and RFC 3339 updated times. Both versions use the exact durable session ID and return the same model and reasoning configuration options as `session/new`.

Session discovery and restoration are isolated to the server's canonical workspace root. The primary `cwd` must match that root. ACP `session/new`, `session/load`, `session/resume`, and supported `session/fork` requests can also supply `additionalDirectories`: absolute paths to existing project directories. Kit canonicalizes and deduplicates these paths, loads their ancestor `AGENTS.md` instructions as session context, and reports the roots in `SessionInfo`. Additional roots are not a filesystem allowlist and do not change the default tool cwd, configuration, or durable session namespace. Each attachment supplies its complete additional-root set; an empty list clears prior extra roots. These roots are persisted as extensible transcript metadata, so old sessions without that metadata report an empty list. Legacy transcripts under a project-local `.kit/sessions` directory follow the same migration and root checks as CLI resume; they do not make a same-named session visible from another workspace. Old global transcripts without workspace metadata are excluded from discovery in every workspace, but an explicit resume by ID remains supported and binds the transcript to that workspace. An individually malformed or concurrently incomplete transcript is omitted from catalog results without preventing valid sessions from being listed; explicit resume remains strict and reports its error.

An arbitrary ACP load or resume never applies the server process's configured `--force` setting. The one exception is the initial resume requested by `kit tui --resume [<id>] --force`: only the explicitly named or picker-selected session may use the stale-lock override. If another live Kit instance owns the session lock, restoration fails instead of taking over the session. A missing or invalid ID also fails normally. After the session closes and releases its lock, an ACP client can restore it again.

Before the restoration response, Kit replays the canonical transcript as ordered ACP updates for representable user text and attachments, assistant text and thoughts, and tool calls and results. Internal instructions, ambient context, notifications, and provider-specific content are not replayed to the client, but remain in the model transcript. Because compaction replaces the canonical transcript, restoring a compacted session replays its canonical summary history rather than the superseded pre-compaction items.

### Session locks, `--resume`, and `--force`

Only one live Kit instance may mutate a session. A normal exit removes its `.lock` file. If opening a session reports:

```text
session is locked by another Kit instance (...); use --force to override a stale lock
```

first confirm that no Kit process is still using the session. Then retry the resume with:

```sh
kit tui --root /path/to/project --resume <session-id> --force
```

`--force` is only for a stale lock left by an exited or crashed process, and the CLI accepts it only with `--resume`. With `kit tui --resume --force`, the override applies only to the session selected in the startup picker. It does not steal a lock held by a live process: the OS-level lock check instead reports `session is actively locked by another Kit instance (...)`. Do not manually remove a lock belonging to a running Kit process.

A new session ID that already exists reports `session ... already exists; use --resume`; a missing resume target reports `session ... does not exist`. Use the correct ID and mode rather than `--force` for either error.

## Automatic context compaction

Kit uses the latest provider-reported context usage for both preparation and automatic compaction. By default, it automatically compacts mutable history when that usage reaches 80% of `context_window`. Content added after the report is counted by the provider on the next request; the headroom below the model's full window accommodates this growth. Kit does not substitute serialized message sizes for reported token usage. Configure `[compaction]` in `~/.kit/config.toml` to choose a different percentage or an optional token ceiling. The apply threshold is the smaller of the percentage of the full window (rounded down) and `max_context_tokens`, if set. For example, `threshold_percent = 55` with `max_context_tokens = 200000` triggers at 55% or 200,000 tokens, whichever comes first. This is a trigger, not a strict context-size cap: usage can cross the threshold before Kit checks it, and retained instructions and recent history may exceed a small ceiling. If the provider did not report a context window, or reports zero, automatic compaction stays disabled rather than guessing a limit.

For top-level persistent sessions, Kit can prepare a checkpoint in the background before applying it. Preparation starts only when usage is within **both** `precompute_margin_percent` percentage points of the full window and `precompute_margin_tokens` tokens of the apply threshold. Defaults are 10 percentage points and 20,000 tokens. The lead is also limited to half the apply threshold, rounded down, so preparation never starts before halfway to that threshold. Equivalently: `lead = min(window * margin_percent / 100, margin_tokens, apply_threshold / 2)` and `prepare_threshold = apply_threshold - lead`, with integer rounding down. Setting either margin to zero disables the early lead.

For a 100,000-token window with default settings, Kit can prepare at 70,000 tokens and apply at 80,000. For a 1,000,000-token window with a 55% threshold and a 200,000-token ceiling, it can prepare at 180,000 and apply at 200,000. Background preparation does not itself replace canonical history. A prepared checkpoint must still match the captured conversation prefix and model selection. New messages, including media and file references, remain in the retained tail; their serialized size does not cause Kit to discard the checkpoint. Unavailable or invalidated preparation falls back to foreground compaction. Background preparation is disabled for delegated sessions, nonpersistent sessions, and when a `compact` hook observes or intercepts compaction. Preparation is best effort, not a guarantee of lower latency or model cost: a prepared summary can become unusable or fail, and foreground work may still be needed. See [compaction configuration](getting-started-and-configuration.md#configure-automatic-compaction) for accepted values and commands.

Each background preparation has a total deadline from when it starts, using `request_budget_seconds` (60 seconds by default), including on the ChatGPT subscription backend. Reaching the apply threshold does not restart that budget. If preparation expires, Kit cancels it and uses foreground compaction; a checkpoint completed within the budget remains reusable afterward. Failed or cancelled preparation can retry after a new usage report, turn, or model selection, with at most two attempts before successful compaction resets the limit. Successful prepared checkpoints are retained rather than refreshed on each usage report.

Automatic compaction drops historical reasoning, summarizes an older prefix into a structured coding checkpoint, and preserves bootstrap `System` and `Context` items plus a recent tail targeting approximately 8,000 tokens (smaller for short context windows or large bootstrap instructions). An indivisible recent item or tool round can exceed that target so call/result pairs remain valid. The split never separates a tool call from its result. Historical tool results are aggressively truncated, while the three newest results receive a larger bounded allowance. Inline media and file payloads are replaced with bounded placeholders in the summary request; durable URI and artifact references remain available to the summarizer without embedding their bytes. Each rendered item has a fixed byte limit, and the conversation prompt has a conservative byte budget that reserves at least half of the provider's context window for instructions, framing, and output. Later checkpoints fold in the previous checkpoint while preferring newer facts. Manual `/compact` uses the same retention policy; optional text after the command becomes the next user input. A successful durable-session compaction is appended as a canonical transcript replacement, so resuming uses the compacted history. The TUI shows the lifecycle and adds `context compacted` after a real replacement.

Compaction uses the selected model to produce the summary and can fail or be cancelled like other model work. An error such as `compaction agent returned an empty summary` leaves the previous transcript canonical; resolve the provider problem and retry `/compact`.

## Transcript repair and crash recovery

Kit automatically repairs one specific stranded-transcript condition: a stored tool call with no surviving tool result. On load it synthesizes an error result directly after each unanswered call. On resume, while holding the session lock, it also persists the repair so later resumes see a valid call/result pair. The synthetic result says that the work may or may not have completed, so inspect project state before retrying a tool or assuming its side effects occurred.

This repair does not hide general JSONL damage. Errors including `invalid transcript line`, `unsupported session schema version`, `invalid session identity or generation`, or `transcript line ... must contain exactly one item or replacement` indicate malformed, incompatible, reordered, or edited records. Preserve a backup of the `.jsonl` file before investigating; use the session ID and line number in the diagnostic, and do not invent generations or delete arbitrary records. If no trustworthy repair is possible, start a new session and re-establish the needed context.

While a session is open, Kit can reconstruct a transcript path deleted from disk using its still-open file and can reacquire a missing lock only if no other owner won the lock. It fails closed if another process owns recovery authority. After an abnormal TUI shutdown, restarting with `--resume` should be the first recovery attempt; add `--force` only after confirming the remaining lock is stale.

### Terminal colours and Speakeasy branding

The TUI inherits the terminal's configured foreground and background. Functional accents use ANSI colour slots, so success, warning, error, focus, and muted text follow the user's terminal palette instead of a Kit-specific dark or light theme. Selection uses the terminal's reversed style, and Kit does not probe or repaint the terminal background.

The empty starter screen adds a decorative Speakeasy rainbow line beneath the Kit name. Once a session has transcript content, the line moves beneath the header and spans the terminal width when the terminal is tall enough to show it without displacing required content. Terminals that advertise 24-bit colour receive the Speakeasy brand ramp; limited-colour terminals receive an ANSI approximation from their configured palette. No status or instruction depends on distinguishing those colours.

### TUI startup and terminal recovery

The TUI runs a `kit serve` child with ACP v2 selected explicitly for its stdio connection; ordinary `kit serve` invocations continue to default to ACP v1 on stdio. If that child exits before opening the session—for example because the root is missing, credentials are unavailable, or an A2A address is already taken—the TUI reports the child's last diagnostics. A silent or wedged child eventually reports `the agent did not answer the ACP handshake within 30 seconds`. Fix that diagnostic and restart with the same `--resume` ID when a transcript was created.

If an external hard kill leaves the shell in raw mode or mouse reporting appears as text, run `reset` (or reopen the terminal) before resuming. Prefer `Esc`, `Ctrl+C`, `Ctrl+D`, `SIGTERM`, or `SIGHUP` for normal shutdown so Kit can restore terminal modes, cancel active work, close the session, and clean up only locks proven stale.

## Provider usage

Use `/usage [provider]` to check account quotas or key spend without sending a
model prompt. See [Provider usage](provider-usage.md) for supported providers,
credentials, and the meaning of the reported limits.

## Remote terminal sessions

For an authenticated terminal attachment to a private gateway host, see [Private-network gateway](gateway.md). The gateway owns execution across terminal disconnects; provider credentials and tools stay on the host.

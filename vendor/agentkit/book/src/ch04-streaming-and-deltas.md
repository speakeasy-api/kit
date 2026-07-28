# Streaming and deltas

Models generate tokens incrementally. A production agent must handle this streaming output — rendering text to the user as it arrives, accumulating tool call arguments chunk by chunk, and folding everything into durable transcript items when the turn completes.

This chapter covers the `Delta` type and the streaming protocol.

## The problem with streaming

Streaming creates a fundamental tension: the transcript stores complete `Part` values, but the model emits fragments. You need a way to bridge these two representations without requiring every downstream consumer (reporters, compaction, persistence) to understand the streaming protocol.

```text
What the provider sends (SSE stream):

  data: {"delta":{"content":"The"}}
  data: {"delta":{"content":" answer"}}
  data: {"delta":{"content":" is"}}
  data: {"delta":{"content":" 42."}}
  data: [DONE]

What the transcript stores (after the turn):

  Item {
      kind: Assistant,
      parts: [Part::Text(TextPart { text: "The answer is 42." })]
  }
```

Everything between those two representations is the streaming layer's job.

agentkit's solution is to separate the two concerns entirely:

- **`Delta`** — transient, incremental, consumed during a turn
- **`Part`** — durable, complete, stored in the transcript after a turn

The loop folds deltas into parts. Reporters observe deltas for real-time rendering. The transcript only ever contains committed parts.

```text
Provider SSE stream
        │
        ▼
   ┌──────────┐
   │  Adapter  │  converts SSE chunks → Delta values
   └────┬─────┘
        │
        ▼
   Delta stream (transient, intra-turn)
   ┌──────────────────────────────────────────────┐
   │ BeginPart → AppendText → AppendText → Commit │
   └─────┬──────────────┬────────────────────┬────┘
         │              │                    │
         ▼              ▼                    ▼
    LoopObserver    LoopObserver        LoopDriver
    (reporter)     (usage tracker)     (folds → Part)
                                             │
                                             ▼
                                     Transcript (durable)
                                     Vec<Item> with committed Parts
```

## The delta protocol

```rust
pub enum Delta {
    BeginPart { part_id: PartId, kind: PartKind },
    AppendText { part_id: PartId, chunk: String },
    AppendBytes { part_id: PartId, chunk: Vec<u8> },
    ReplaceStructured { part_id: PartId, value: Value },
    SetMetadata { part_id: PartId, metadata: MetadataMap },
    CommitPart { part: Part },
}
```

Each variant serves a specific role in the streaming lifecycle:

| Delta variant       | When it's emitted                               | What the consumer does          |
| ------------------- | ----------------------------------------------- | ------------------------------- |
| `BeginPart`         | Model starts generating a new content block     | Allocate a buffer for `part_id` |
| `AppendText`        | A text chunk arrives (token or group of tokens) | Append to the text buffer       |
| `AppendBytes`       | A binary chunk arrives (audio, image data)      | Append to the byte buffer       |
| `ReplaceStructured` | A structured value is updated wholesale         | Replace the buffer contents     |
| `SetMetadata`       | Metadata for a part is available                | Store metadata for the part     |
| `CommitPart`        | The part is complete                            | Finalise, discard the buffer    |

### A text streaming sequence

The most common case — the model generates a text response:

```text
Adapter emits:                                       Reporter sees:     Buffer state:

1. BeginPart { id: "p1", kind: Text }                (allocate)         ""
2. AppendText { id: "p1", chunk: "The " }            print("The ")      "The "
3. AppendText { id: "p1", chunk: "answer" }          print("answer")    "The answer"
4. AppendText { id: "p1", chunk: " is " }            print(" is ")      "The answer is "
5. AppendText { id: "p1", chunk: "42." }             print("42.")       "The answer is 42."
6. CommitPart { part: Text("The answer is 42.") }    (done)             → transcript
```

The reporter prints each chunk as it arrives — the user sees text appear incrementally. The driver accumulates the same chunks but only commits the final `Part` to the transcript.

### A multi-part streaming sequence

An assistant response with both text and a tool call:

```text
1. BeginPart { id: "p1", kind: Text }
2. AppendText { id: "p1", chunk: "I'll read that file." }
3. CommitPart { part: Text("I'll read that file.") }
4. BeginPart { id: "p2", kind: ToolCall }
5. AppendText { id: "p2", chunk: "{\"path\":" }          ← JSON argument streaming
6. AppendText { id: "p2", chunk: " \"src/main.rs\"}" }
7. CommitPart { part: ToolCall { name: "fs_read_file", input: {...} } }
```

Note that `part_id` distinguishes concurrent parts. The protocol supports interleaved deltas for different parts, though most providers emit parts sequentially.

### Why not mirror Part variants in Delta?

A simpler design would be one delta variant per part type (`TextDelta`, `MediaDelta`, etc.). agentkit uses generic operations instead (`AppendText`, `AppendBytes`, `ReplaceStructured`) because:

- Multiple part types use text appending (text, reasoning, tool call arguments)
- Multiple part types use byte appending (audio, image, video)
- The operations describe _what's happening_ during streaming, not _what the final type will be_
- Adding a new part type doesn't require a new delta variant unless it has genuinely novel streaming behavior

```text
Delta operations vs Part types — the many-to-many relationship:

AppendText ────── Text          (user/assistant text)
           ├──── Reasoning      (chain-of-thought output)
           └──── ToolCall       (JSON arguments as text)

AppendBytes ───── Media(Audio)  (audio stream)
            ├──── Media(Image)  (image data)
            └──── Media(Video)  (video frames)

ReplaceStructured ─── Structured (JSON output, replaced wholesale)
```

Some OpenAI-compatible gateways return generated images as complete
`delta.images[]` entries rather than partial byte streams. Adapters can surface
those as immediate `BeginPart` / `CommitPart` media deltas while still storing
the final `Part::Media` in the transcript.

## Tool call streaming

Tool calls stream differently from text. The model emits the tool name upfront (usually in a non-streaming fashion) and then streams the JSON arguments incrementally:

```text
SSE from provider:

  data: {"delta":{"tool_calls":[{"index":0,"id":"call-7","function":{"name":"fs_read_file"}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":"}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":" \"sr"}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"c/mai"}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"n.rs\""}}]}}
  data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}

What the adapter emits:

  BeginPart { id: "tc0", kind: ToolCall }
  AppendText { id: "tc0", chunk: "{\"pa" }
  AppendText { id: "tc0", chunk: "th\": " }
  AppendText { id: "tc0", chunk: "\"src/mai" }
  AppendText { id: "tc0", chunk: "n.rs\"}" }
  CommitPart { part: ToolCall { id: "call-7", name: "fs_read_file", input: {"path":"src/main.rs"} } }
```

The loop waits for `CommitPart` before executing the tool. Partial JSON arguments are not actionable — `{"pa` is not a valid tool input. This is why tool calls use the same `AppendText` mechanism as regular text but the driver only acts on the committed `ToolCallPart`.

### Parallel tool call streaming

When the model requests multiple tool calls in a single response, the SSE stream interleaves them by index:

```text
data: {"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"fs_read_file"}}]}}
data: {"delta":{"tool_calls":[{"index":1,"id":"call-2","function":{"name":"shell_exec"}}]}}
data: {"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}
data: {"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"exec"}}]}}
...
```

The adapter maintains per-index accumulators and emits separate `BeginPart`/`AppendText`/`CommitPart` sequences for each tool call. The `part_id` field keeps them distinct.

## Reasoning block streaming

Models that expose chain-of-thought (like Claude with extended thinking) stream reasoning blocks before the final answer:

```text
1. BeginPart { id: "r1", kind: Reasoning }
2. AppendText { id: "r1", chunk: "The user wants to know..." }
3. AppendText { id: "r1", chunk: " I should consider..." }
4. CommitPart { part: Reasoning { summary: Some("The user wants..."), ... } }
5. BeginPart { id: "p1", kind: Text }
6. AppendText { id: "p1", chunk: "The answer is 42." }
7. CommitPart { part: Text("The answer is 42.") }
```

A reporter can display reasoning blocks differently (dimmed, collapsible, in a side panel), while the transcript stores them as ordinary parts that compaction can later drop to save space.

## Observer consumption

Reporters observe deltas via the `LoopObserver` trait:

```rust
pub trait LoopObserver: Send + Sync {
    fn handle_event(&self, event: ObservedEvent);
}
```

When the driver receives a `Delta` from the model turn, it wraps it as `AgentEvent::ContentDelta(delta)` inside a session-addressed `ObservedEvent` envelope and dispatches it to all registered observers synchronously, in registration order.

This is how real-time text rendering works — the `StdoutReporter` receives `AppendText` deltas and writes each chunk to the terminal immediately:

```rust
fn handle_event(&self, event: ObservedEvent) {
    if let AgentEvent::ContentDelta(Delta::AppendText { chunk, .. }) = &event.event {
        print!("{}", chunk);
        std::io::stdout().flush().ok();
    }
}
```

The ordering guarantee matters: within a single driver instance, deltas are delivered to observers in the order the adapter produces them. If the adapter emits `AppendText("Hello")` before `AppendText(", world")`, every observer sees them in that order. This is trivially satisfied because observers are called synchronously on the driver's task — there is no async fan-out or buffering between the adapter and observers.

### What observers should and shouldn't do

Observers are called inline on the driver's task. They must be fast — a slow observer blocks the entire loop. Guidelines:

- **Do:** write to stderr/stdout, increment counters, append to a `Vec`
- **Do:** send to a channel for async processing elsewhere
- **Don't:** make HTTP requests, write to databases, or do anything that might block
- **Don't:** modify the transcript or influence the loop's control flow

If you need expensive processing, use a `ChannelReporter` adapter that forwards events to another task.

## Relationship to the transcript

After a turn completes, the transcript contains only committed `Part` values inside `Item`s. Deltas are discarded. On the next turn, the model receives the transcript — not the deltas that produced it.

```text
During a turn:                        After a turn:

  Delta stream (live)                  Transcript (durable)
  ┌────────────────────┐               ┌─────────────────────────┐
  │ BeginPart          │               │ Item { kind: Assistant, │
  │ AppendText("He")   │               │   parts: [              │
  │ AppendText("llo")  │    fold ──▶   │     Text("Hello"),      │
  │ CommitPart(Text)   │               │     ToolCall { ... },   │
  │ BeginPart          │               │   ]                     │
  │ AppendText("{...") │               │ }                       │
  │ CommitPart(Tool)   │               └─────────────────────────┘
  └────────────────────┘
       (discarded)                          (persisted)
```

This separation means:

- Compaction operates on stable, complete items — it never sees partial deltas
- Persistence stores items, not delta streams — simpler storage format
- The streaming protocol can evolve independently of the transcript format — adding a new delta variant doesn't change how transcripts are stored
- Replay is possible without streaming — a transcript can be loaded from storage and fed directly to the model without reconstructing the delta sequence

> **Crate:** `Delta`, `PartId`, and `PartKind` are defined in [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core). The folding logic lives in [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop). Reporters that consume deltas are in [`agentkit-reporting`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-reporting).

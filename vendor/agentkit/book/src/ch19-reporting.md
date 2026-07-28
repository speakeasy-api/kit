# Reporting and observability

An agent that you can't observe is an agent you can't debug. This chapter covers `agentkit-reporting`: how events flow from the loop to observers, and the built-in reporter implementations.

## The observer contract

```rust
pub trait LoopObserver: Send + Sync {
    fn handle_event(&self, event: ObservedEvent);
}
```

`ObservedEvent` is a session-addressed envelope: the `session_id` plus the `AgentEvent`. Observers take `&self` and keep mutable state behind interior mutability (`Mutex`, atomics, channels). The driver shares each observer as `Arc<dyn LoopObserver>`, so a single configured `Agent` can mint multiple sessions and the same reporter sees them all — and route per session when it needs to (this is what makes shared-sink consumers like the ACP integration possible).

Observers are synchronous and called in deterministic order. This is a deliberate choice:

- **Deterministic ordering** — if event B depends on event A, observers always see A first
- **No async leakage** — the loop stays runtime-agnostic
- **Simple reasoning** — observer behavior is fully predictable

The cost is that observers must be fast. Heavy processing should happen behind a channel adapter.

```text
Event flow:

  LoopDriver
       │
       ├── emit(AgentEvent)
       │        │
       │        ├──▶ Observer 1 (StdoutReporter)    → print to terminal
       │        ├──▶ Observer 2 (JsonlReporter)      → write to log file
       │        └──▶ Observer 3 (UsageReporter)      → accumulate counters
       │
       │   Observers are called in registration order.
       │   Each observer blocks until it returns.
       │   Total time = sum of all observer handle_event() calls.
       │
       └── continue loop execution
```

## Built-in reporters

### StdoutReporter

Human-readable terminal output. Handles streaming text deltas, tool lifecycle notices, approval prompts, and turn summaries. Intentionally conservative — line-oriented output, no cursor management or advanced TUI tricks.

### JsonlReporter

One structured JSON object per event, newline-delimited. Useful for audit logs, debugging, and external system ingestion. Uses a stable envelope format with event type, timestamp, turn ID, and payload. When the reporter is shared across sessions (a shared-sink setup), opt into `with_session_id(true)` to add the observed session id to each envelope — it is opt-in so strict consumers keep the original envelope shape.

### UsageReporter

Aggregates token usage across a session: input tokens, output tokens, reasoning tokens, cached input tokens, cache write tokens, estimated cost. Exposes query methods for per-turn and cumulative totals.

### TranscriptReporter

Reconstructs an inspectable transcript from events. Useful for debugging, persistence, and testing. Important constraint: the reporter reconstructs a _derived_ view — the loop owns the authoritative working transcript.

### CompositeReporter

Fans out events to multiple child reporters:

```rust
let reporter = CompositeReporter::new()
    .with_observer(StdoutReporter::new(std::io::stderr()))
    .with_observer(JsonlReporter::new(file))
    .with_observer(UsageReporter::new());
```

## Adapter reporters

For expensive or async reporting:

- **BufferedReporter** — enqueues events for batch flushing
- **ChannelReporter** — forwards events to another thread or task via a sender
- **TracingReporter** — converts events into tracing spans and events

These adapters wrap the synchronous observer contract without changing it.

## Tracing and OpenTelemetry

agentkit emits structured `tracing` data on **two independent layers** that you can filter and route separately.

### Layer 1: internal `tracing::instrument` spans

The loop, provider adapters, and tool dispatch sites are annotated with `#[tracing::instrument]` and ad-hoc `info_span!` macros. These spans cover what the framework is doing right now — they are not user events. You see them whenever `tracing` is enabled, whether or not you wire up a reporter.

| Span name            | Source crate    | Fields                                                                                                                                                                                                                                                          |
| -------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `agent.turn`         | `agentkit_loop` | `otel.name="invoke_agent"`, `gen_ai.operation.name="invoke_agent"`, `gen_ai.conversation.id`, `gen_ai.provider.name`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `session.id`, `turn.id`, `transcript.len`, `saw_tool_call`, `finish_reason`    |
| `agent.execute_tool` | `agentkit_loop` | `otel.name="execute_tool {tool}"`, `gen_ai.operation.name="execute_tool"`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.conversation.id`, `error.type` (on failed results), `session.id`, `turn.id`, `launch_kind`                                        |
| `chat`               | `agentkit_loop` | `otel.name="chat {model}"`, `otel.kind="client"`, `gen_ai.operation.name="chat"`, `gen_ai.provider.name`, `gen_ai.conversation.id`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons`, token-usage fields |
| `mcp.call_tool`      | `agentkit_mcp`  | `otel.name="mcp.call_tool {tool}"`, `mcp.server.id`, `mcp.tool.name`, `error.type` (on protocol failures or `is_error` results)                                                                                                                                 |

Field naming follows the [OpenTelemetry GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/), so spans exported to an OTel backend slot directly into existing GenAI dashboards. Static tracing span names (`agent.turn`, `agent.execute_tool`, `chat`) stay stable for log filtering; the `otel.name` field carries the dynamic semconv span name (`invoke_agent`, `execute_tool {tool}`, `chat {model}`) for OpenTelemetry bridges that key off it.

`launch_kind` is `"plain"` for tool calls dispatched in a normal tool round, `"approved"` when the call resumes after a human-in-the-loop approval. `gen_ai.provider.name` is sourced from `ModelAdapter::provider_name()` and `gen_ai.request.model` from `ModelSession::model_name()`.

The `chat` span is emitted by the loop itself, wrapping `begin_turn` plus the full event drain, so every adapter — buffered or streaming — gets it without any adapter-side instrumentation. Because the span stays open until the `Finished` event, response attributes that streaming providers only deliver mid-stream (response id/model, usage, stop reason) are recorded from `ModelTurnEvent`s and the `ModelTurnResult` `model`/`response_id` fields. `mcp.call_tool` wraps the MCP server round-trip and parents under `agent.execute_tool`, separating wire time from dispatch overhead.

### Layer 2: `TracingReporter`

`TracingReporter` is a `LoopObserver` that converts each `AgentEvent` into a single `tracing` event:

| Agent event                                                                                                                                                                                           | Level   |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| `RunStarted`, `TurnStarted`, `TurnFinished`, `ToolCallRequested`, `ToolExecutionStarted`, `ToolExecutionProgress`, `ToolResultReceived`, `ApprovalRequired`, `ApprovalResolved`, `ToolCatalogChanged` | `INFO`  |
| `InputAccepted`, `UsageUpdated`, `MutationStarted`, `MutationFinished`                                                                                                                                | `DEBUG` |
| `ContentDelta`                                                                                                                                                                                        | `TRACE` |
| `Warning`                                                                                                                                                                                             | `WARN`  |
| `RunFailed`                                                                                                                                                                                           | `ERROR` |

Reporter events are emitted under the `agentkit_reporting` target so they filter independently of the internal spans:

```bash
# Internal spans + reporter events
RUST_LOG=agentkit_loop=debug,agentkit_reporting=info cargo run

# Reporter events only (treat agentkit as a black box)
RUST_LOG=agentkit_reporting=info cargo run

# One specific provider's HTTP traffic + everything else at info
RUST_LOG=info,agentkit_provider_anthropic=trace cargo run
```

The `TracingReporter` target is fixed to `agentkit_reporting` because the underlying `tracing` macros require compile-time-constant targets. To route reporter output into your application's own log namespace, implement `LoopObserver` directly and call `tracing::*!` macros with your own `target:` literal.

### Enabling the reporter

The reporter is gated behind the `tracing` cargo feature on `agentkit-reporting`:

```toml
[dependencies]
agentkit-reporting = { version = "...", features = ["tracing"] }
```

Then register it with the agent like any other observer:

```rust,ignore
use agentkit_reporting::TracingReporter;

let agent = Agent::builder()
    .model(adapter)
    .observer(TracingReporter::new())
    .build()?;
```

### Wiring a `tracing` subscriber

For CLI applications, the standard `tracing-subscriber` setup with `EnvFilter` is enough:

```rust,ignore
use tracing_subscriber::{EnvFilter, fmt};

fmt()
    .with_env_filter(EnvFilter::from_default_env())  // honours RUST_LOG
    .with_target(true)                                // show the target column
    .init();
```

### Exporting to OpenTelemetry

For OTLP export, layer `tracing-opentelemetry` on top of an OTLP exporter and add it to a `tracing-subscriber::Registry`:

```rust,ignore
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt};

let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic().with_endpoint("http://localhost:4317"))
    .install_batch(opentelemetry_sdk::runtime::Tokio)?;

let subscriber = Registry::default()
    .with(EnvFilter::from_default_env())
    .with(OpenTelemetryLayer::new(tracer))
    .with(tracing_subscriber::fmt::layer());

tracing::subscriber::set_global_default(subscriber)?;
```

With this in place, the `agent.turn`, `chat`, `agent.execute_tool`, and `mcp.call_tool` spans become OTel spans in your trace backend (Jaeger, Tempo, Honeycomb, Datadog, etc.) with the GenAI semantic-convention fields preserved as span attributes.

### Two layers, one filter

The split between internal spans and the reporter exists so the two concerns evolve independently:

- **Internal spans** track _what the framework is doing right now_ — useful for performance investigations, deadlocks, and missing instrumentation. They emit unconditionally when `tracing` is enabled in your subscriber, with no host-side wiring.
- **Reporter events** track _what the agent is reporting back to the host_ — useful for UX, audit, and product analytics. They only fire when you register a reporter, and they have stable categorical levels for log routing.

A coding-agent CLI typically wants both. A library embedding agentkit may want only the reporter events to keep the framework's internal noise out of its own logs.

## Failure policy

Reporter failures are non-fatal by default. A broken log writer shouldn't crash the agent. Hosts can configure stricter behavior:

- `Ignore` — swallow errors
- `Log` — log errors to stderr
- `Accumulate` — collect errors for later inspection
- `FailFast` — abort on first error

## Writing a custom observer

The trait is simple enough that custom observers are straightforward:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};

struct ToolCallCounter {
    count: AtomicUsize,
}

impl LoopObserver for ToolCallCounter {
    fn handle_event(&self, event: ObservedEvent) {
        if matches!(event.event, AgentEvent::ToolCallRequested(_)) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

A more practical example — a reporter that writes tool calls to a structured log:

```rust
use std::sync::Mutex;

struct AuditLogger {
    writer: Mutex<BufWriter<File>>,
}

impl LoopObserver for AuditLogger {
    fn handle_event(&self, event: ObservedEvent) {
        let mut writer = self.writer.lock().unwrap();
        match &event.event {
            AgentEvent::ToolCallRequested(call) => {
                writeln!(writer, "TOOL_CALL: {} input={}", call.name,
                    serde_json::to_string(&call.input).unwrap_or_default()
                ).ok();
            }
            AgentEvent::ApprovalRequired(req) => {
                writeln!(writer, "APPROVAL_REQUIRED: {} reason={:?}",
                    req.summary, req.reason
                ).ok();
            }
            _ => {}
        }
    }
}
```

## AgentEvent categories

| Category   | Events                                                                                                           |
| ---------- | ---------------------------------------------------------------------------------------------------------------- |
| Lifecycle  | `RunStarted`, `TurnStarted`, `TurnFinished`, `RunFailed`                                                         |
| Input      | `InputAccepted`                                                                                                  |
| Streaming  | `ContentDelta`                                                                                                   |
| Tools      | `ToolCallRequested`, `ToolExecutionStarted`, `ToolExecutionProgress`, `ToolResultReceived`, `ToolCatalogChanged` |
| Approval   | `ApprovalRequired`, `ApprovalResolved`                                                                           |
| Mutators   | `MutationStarted`, `MutationFinished`                                                                            |
| Usage      | `UsageUpdated`                                                                                                   |
| Diagnostic | `Warning`                                                                                                        |

For loss-free transcript reconstruction, register a `TranscriptObserver` alongside `LoopObserver`. It fires a session-addressed `TranscriptEvent` once per `Item` appended, in transcript order — including the synthetic placeholder and the eventual real result for background-detached tools, correlated by `call_id` through the matching `ToolExecutionProgress` (placeholder) and `ToolResultReceived` (terminal result) events.

### Event timeline for a typical turn

```text
RunStarted { session_id }
│
├── InputAccepted { items: [User("Fix the bug")] }
├── TurnStarted { session_id, turn_id: "turn-1" }
│   ├── ContentDelta(BeginPart { kind: Text })
│   ├── ContentDelta(AppendText { chunk: "I'll " })
│   ├── ContentDelta(AppendText { chunk: "read the file." })
│   ├── ContentDelta(CommitPart { part: Text("I'll read the file.") })
│   ├── ToolCallRequested(ToolCallPart { name: "fs_read_file", ... })
│   └── UsageUpdated(Usage { input: 1500, output: 200 })
│
├── TurnStarted { session_id, turn_id: "turn-2" }  ← automatic tool roundtrip
│   ├── ContentDelta(...)                            ← model response after reading file
│   ├── ToolCallRequested(ToolCallPart { name: "fs_replace_in_file", ... })
│   └── UsageUpdated(Usage { ... })
│
└── TurnFinished(TurnResult { finish_reason: Completed, ... })
```

> **Example:** [`openrouter-agent-cli`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-agent-cli) uses a composite reporter with stdout and usage reporting.
>
> **Crate:** [`agentkit-reporting`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-reporting) — depends on [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop) for event types.

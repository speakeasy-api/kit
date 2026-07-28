# agentkit-reporting

<p align="center">
  <a href="https://crates.io/crates/agentkit-reporting"><img src="https://img.shields.io/crates/v/agentkit-reporting.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-reporting"><img src="https://img.shields.io/docsrs/agentkit-reporting?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-reporting.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Observers for turning loop events into logs, summaries, and transcript views.

This crate provides `LoopObserver` implementations for [`agentkit-loop`](https://crates.io/crates/agentkit-loop).
Instead of baking reporting into the driver, you attach one or more reporters
to the loop and they react to every `AgentEvent` that flows through it.

## Included reporters

| Reporter             | Purpose                                                    |
| -------------------- | ---------------------------------------------------------- |
| `StdoutReporter`     | Human-readable bracketed log lines (`[turn] started ...`)  |
| `JsonlReporter`      | Machine-readable newline-delimited JSON envelopes          |
| `UsageReporter`      | Aggregated token counts and cost totals                    |
| `TranscriptReporter` | Growing snapshot of conversation items                     |
| `CompositeReporter`  | Fan-out wrapper that forwards events to multiple reporters |

## Quick start

Compose several reporters with `CompositeReporter` and hand it to the loop:

```rust
use agentkit_reporting::{
    CompositeReporter, JsonlReporter, StdoutReporter,
    TranscriptReporter, UsageReporter,
};

// Build a composite reporter that fans out to all four reporters.
let reporter = CompositeReporter::new()
    .with_observer(StdoutReporter::new(std::io::stderr()).with_usage(false))
    .with_observer(JsonlReporter::new(Vec::new()).with_flush_each_event(false))
    .with_observer(UsageReporter::new())
    .with_observer(TranscriptReporter::new());

// Pass `reporter` as the observer when constructing the agent loop.
```

## Accessing outputs after the loop

Reporters that accumulate state (`UsageReporter`, `TranscriptReporter`,
`JsonlReporter`) expose accessors for reading back data once the loop
finishes:

```rust
use agentkit_reporting::{UsageReporter, TranscriptReporter, JsonlReporter};

// Usage totals
let reporter = UsageReporter::new();
// ...run the loop...
let summary = reporter.summary();
println!(
    "tokens: {} in / {} out, turns: {}",
    summary.totals.input_tokens,
    summary.totals.output_tokens,
    summary.turn_results_seen,
);

// Transcript items
let reporter = TranscriptReporter::new();
// ...run the loop...
for item in &reporter.transcript().items {
    println!("{:?}: {} parts", item.kind, item.parts.len());
}

// JSONL buffer
let mut reporter = JsonlReporter::new(Vec::new());
// ...run the loop...
let jsonl = String::from_utf8(reporter.writer().clone()).unwrap();
let errors = reporter.take_errors();
assert!(errors.is_empty(), "reporting errors: {:?}", errors);
```

## Writing to a file

`JsonlReporter` and `StdoutReporter` accept any `std::io::Write`
implementation, so you can point them at files, network sockets, or
in-memory buffers:

```rust,no_run
use agentkit_reporting::JsonlReporter;
use std::io::BufWriter;
use std::fs::File;

let file = File::create("events.jsonl").expect("open file");
let reporter = JsonlReporter::new(BufWriter::new(file));
```

## Adapter reporters

For expensive or async reporting, wrap an inner reporter in one of the
provided adapters:

| Adapter            | Purpose                                              |
| ------------------ | ---------------------------------------------------- |
| `BufferedReporter` | Enqueues events and flushes in batches               |
| `ChannelReporter`  | Forwards events to another thread via `mpsc::Sender` |
| `TracingReporter`  | Emits `tracing` events (requires `tracing` feature)  |

```rust
use agentkit_reporting::{BufferedReporter, JsonlReporter};

// Flush to the JSONL writer every 128 events instead of one-at-a-time.
let reporter = BufferedReporter::new(
    JsonlReporter::new(Vec::new()).with_flush_each_event(false),
    128,
);
```

```rust
use agentkit_reporting::ChannelReporter;

let (reporter, rx) = ChannelReporter::pair();

std::thread::spawn(move || {
    while let Ok(event) = rx.recv() {
        println!("{event:?}");
    }
});

// Pass `reporter` to the agent loop.
```

### TracingReporter

`TracingReporter` bridges agent events into the `tracing` ecosystem. It
is gated behind the `tracing` feature to keep the dependency opt-in:

```toml
agentkit-reporting = { version = "0.10.2", features = ["tracing"] }
```

```rust,ignore
use agentkit_reporting::TracingReporter;

let reporter = TracingReporter::new();
```

Events are emitted under the `"agentkit_reporting"` target at levels that match
their severity (INFO for lifecycle, DEBUG for usage/compaction, TRACE for
content deltas, WARN/ERROR for problems).

## Failure policy

Reporter failures are non-fatal by default. For reporters that can fail
(I/O, channel sends), implement `FallibleObserver` and wrap it in a
`PolicyReporter` to choose how errors are handled:

| Policy       | Behaviour                           |
| ------------ | ----------------------------------- |
| `Ignore`     | Silently discard errors (default)   |
| `Log`        | Print errors to stderr              |
| `Accumulate` | Collect errors for later inspection |
| `FailFast`   | Panic on first error                |

```rust
use agentkit_reporting::{ChannelReporter, FailurePolicy, PolicyReporter};

let (reporter, rx) = ChannelReporter::pair();
let reporter = PolicyReporter::new(reporter, FailurePolicy::Log);
```

## Error handling

`JsonlReporter` and `StdoutReporter` never panic on write failures.
Errors are collected internally and can be drained after the loop with
`take_errors()`. This keeps the `LoopObserver::handle_event` signature
infallible while still giving you full visibility into any I/O issues.

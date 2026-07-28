# agentkit-reporting design

## Purpose

`agentkit-reporting` is the observability crate.

It should provide reusable implementations and adapters for consuming loop events:

- human-readable terminal output
- structured JSON logs
- usage aggregation
- transcript capture
- optional buffering or fanout adapters

It should make it easy for a host application to observe agent behavior without reimplementing the same event plumbing.

## Non-goals

`agentkit-reporting` should not own:

- the canonical event model
- the loop driver
- provider integrations
- tool execution
- persistent storage systems
- telemetry vendor SDK lock-in
- control flow decisions

The loop defines what happened. Reporting decides how to consume and present it.

## Dependency direction

The dependency direction should be:

- `agentkit-core` at the bottom
- `agentkit-loop` defines `AgentEvent` and `LoopObserver`
- `agentkit-reporting` depends on `agentkit-loop`

Not the reverse.

That means:

- loop events remain meaningful even if no reporting crate is used
- hosts can implement their own observers without depending on `agentkit-reporting`
- the reporting crate stays optional

## Design principles

### 1. Reporting is observational only

A reporter should never decide whether the loop continues.

It may:

- render
- serialize
- aggregate
- forward

It may not:

- request approval
- block the loop on user interaction
- alter transcript state

### 2. The base contract stays synchronous

Because `agentkit-loop` uses synchronous observer delivery, the reporting layer should treat that as the primary contract.

That means the simplest reporters implement:

```rust
pub trait LoopObserver: Send + Sync {
    fn handle_event(&self, event: ObservedEvent);
}
```

`ObservedEvent` carries the `session_id` plus the `AgentEvent`. Observers take `&self` and keep mutable state behind interior mutability so the driver can share each one as `Arc<dyn LoopObserver>` across multiple sessions started from the same `Agent`.

This keeps event ordering deterministic and avoids async leakage into the loop.

### 3. Expensive reporting should be opt-in

Some reporting sinks are cheap:

- write one line to stderr
- increment counters
- append JSON to a file buffer

Some are not:

- network export
- async file pipelines
- cross-thread fanout

The reporting crate should support both, but the expensive path should be explicit and feature-gated.

### 4. Reporting should compose

Hosts will often want more than one behavior at once:

- pretty terminal output
- transcript capture
- usage accounting
- JSON audit log

The crate should make composition trivial.

## What belongs in reporting

## 1. Observer implementations

The base crate should provide concrete observer implementations for common tasks.

Recommended v1 implementations:

- `StdoutReporter`
- `JsonlReporter`
- `UsageReporter`
- `TranscriptReporter`
- `CompositeReporter`

### `StdoutReporter`

Purpose:

- render concise human-readable output for local terminal apps

Should handle:

- text deltas
- tool start/end notices
- approval-required notices
- warnings
- final turn summaries

Should not try to be a full TUI framework.

### `JsonlReporter`

Purpose:

- emit one structured JSON object per event

Use cases:

- audit logs
- debugging
- local replay tooling
- ingestion by external systems

Recommended format:

- one line per `AgentEvent`
- stable top-level envelope
- include event type, timestamp, turn ID, and event payload
- include the observed session ID when the reporter is configured for shared-sink routing

### `UsageReporter`

Purpose:

- aggregate usage across a session or process

Should track:

- input tokens
- output tokens
- reasoning tokens if present
- estimated or provider-supplied cost
- per-turn and cumulative totals

It should expose query methods rather than just side effects.

### `TranscriptReporter`

Purpose:

- reconstruct an inspectable transcript view from events

This is useful for:

- debugging
- host-managed persistence
- testing

Important constraint:

- it should consume normalized events and deltas
- it should not become the canonical transcript owner for loop correctness

The loop still owns the active working transcript.

### `CompositeReporter`

Purpose:

- fan out one event to multiple reporters synchronously

This should be the standard composition primitive for v1.

Recommended shape:

```rust
pub struct CompositeReporter {
    children: Vec<Arc<dyn LoopObserver>>,
}
```

This lets hosts attach multiple behaviors without building their own dispatcher.

## 2. Shared reporting envelopes

Even though `AgentEvent` is defined by `agentkit-loop`, the reporting crate may want helper serialization types.

Recommended helper:

```rust
pub struct EventEnvelope<'a> {
    pub timestamp: SystemTime,
    pub session_id: Option<&'a SessionId>,
    pub event: &'a AgentEvent,
}
```

This is useful because raw `AgentEvent` may not carry every transport concern a reporter wants.

The helper envelope should remain thin. Optional routing fields, such as
`session_id`, should be explicit reporter configuration so strict consumers can
keep the original envelope shape.

## 3. Usage aggregation types

If `Usage` lives in `agentkit-core`, reporting can define derived views such as:

- `SessionUsageSummary`
- `TurnUsageSummary`
- `UsageBreakdownByTool`

These are reporting artifacts, not core protocol types.

## 4. Reporter adapters

This is where buffered or async behavior belongs.

Recommended v1 split:

- base reporters are synchronous
- adapters can bridge synchronous loop events into buffered pipelines

Possible adapters:

- `BufferedReporter`
- `ChannelReporter`
- `TracingReporter`

### `BufferedReporter`

Purpose:

- enqueue events for later flushing

Use cases:

- batch file writes
- reducing direct IO in the loop thread

Important caveat:

- buffering introduces drop, flush, and backpressure policies

That complexity belongs here, not in `agentkit-loop`.

### `ChannelReporter`

Purpose:

- forward events to another thread or task via a sender

This is the cleanest path to async processing without changing the loop contract.

This should likely be behind an optional feature if it depends on a specific channel implementation.

### `TracingReporter`

Purpose:

- convert `AgentEvent`s into spans, events, or metrics for tracing systems

This should be an adapter, not a requirement.

## Failure policy

The default failure policy should be non-fatal.

Rationale:

- observability should not usually break the agent
- terminal rendering failures should not abort a coding session
- metrics exporters should not stop tool execution

Recommended reporter result model:

```rust
pub enum ReportOutcome {
    Ok,
    Dropped,
    Error(ReportError),
}
```

At the loop boundary, failures should be swallowed or downgraded by default.

Hosts that want stricter behavior can opt into a policy like:

- `Ignore`
- `Log`
- `Accumulate`
- `FailFast`

The important point is that strictness is host-configured policy, not the default assumption.

## Event ordering

Because the loop calls observers synchronously, event ordering should be deterministic within one driver instance.

That gives reporters a strong guarantee:

- if event B depends on event A, observers will see A before B

This matters especially for:

- text delta rendering
- usage aggregation
- transcript reconstruction
- tool lifecycle reporting

Buffered adapters may weaken real-time visibility, but they should preserve logical order unless explicitly documented otherwise.

## Serialization strategy

The reporting crate should not invent multiple event schemas in v1.

Recommended strategy:

- one canonical Rust event model from `agentkit-loop`
- one straightforward JSON serialization of that model
- optional thin envelopes for timestamping or sink-specific metadata

Avoid:

- separate "pretty" and "machine" event taxonomies
- sink-specific event structs for each destination
- provider-specific serialization branches

If the base event schema is not good enough to serialize directly, the problem is probably in `AgentEvent`, not in reporting.

## Terminal output policy

The terminal reporter should be intentionally conservative.

V1 should prefer:

- readable line-oriented output
- streamed text support
- clear tool lifecycle messages
- compact warnings and summaries

V1 should avoid:

- heavy cursor management
- advanced layout systems
- platform-specific terminal tricks

That belongs in host applications or a future TUI crate.

## Transcript reconstruction

`TranscriptReporter` deserves special care because it can easily duplicate loop state in a confusing way.

Recommended rule:

- the reporter reconstructs a derived transcript view from events
- the loop owns the authoritative active transcript used for execution

That means:

- transcript reporting is useful for display, persistence, and tests
- it should not be the data source the loop mutates in place

This keeps the dependency direction clean and avoids hidden coupling.

## Suggested public API shape

Recommended first-pass APIs:

```rust
pub struct StdoutReporter { /* config */ }
pub struct JsonlReporter<W> { /* writer */ }
pub struct UsageReporter { /* counters */ }
pub struct TranscriptReporter { /* derived transcript */ }
pub struct CompositeReporter { /* children */ }

impl LoopObserver for StdoutReporter { /* ... */ }
impl<W> LoopObserver for JsonlReporter<W> { /* ... */ }
impl LoopObserver for UsageReporter { /* ... */ }
impl LoopObserver for TranscriptReporter { /* ... */ }
impl LoopObserver for CompositeReporter { /* ... */ }
```

Optional adapters:

```rust
pub struct BufferedReporter<R> { /* ... */ }
pub struct ChannelReporter<S> { /* ... */ }
pub struct TracingReporter { /* ... */ }
```

These optional adapters can be layered on top of the same observer contract.

## Suggested feature flags

At the crate level:

- `std`
- `json`
- `terminal`
- `usage`
- `transcript`
- `buffered`
- `tracing`

Recommended baseline:

- keep the base crate small
- make JSON and terminal output optional if needed
- put runtime- or ecosystem-specific integrations behind separate features

If a reporter requires `tokio`, it should live behind an explicitly named optional feature or a sibling crate.

## What does not belong here

These concerns should stay elsewhere:

- `AgentEvent` definitions: `agentkit-loop`
- `Usage` protocol types: `agentkit-core`
- approval interrupts: `agentkit-loop`
- log storage backends: host application or integration crates
- hosted analytics exporters: optional adapters or external crates

The reporting crate is where reuse lives, not where every sink-specific integration gets hardcoded.

## Suggested module layout

```text
agentkit-reporting/
  src/
    lib.rs
    composite.rs
    stdout.rs
    jsonl.rs
    usage.rs
    transcript.rs
    envelope.rs
    error.rs
    policy.rs
    buffered.rs
    tracing.rs
```

Module intent:

- `composite.rs`: synchronous fanout to child observers
- `stdout.rs`: terminal-focused human-readable reporter
- `jsonl.rs`: newline-delimited JSON reporter
- `usage.rs`: usage accumulation and summaries
- `transcript.rs`: derived transcript reconstruction
- `envelope.rs`: timestamped or sink-specific helper wrappers
- `error.rs`: reporter-specific error types
- `policy.rs`: failure handling configuration
- `buffered.rs`: optional queueing/buffering adapters
- `tracing.rs`: optional tracing integration

## What we should validate early

Before locking the v1 reporting API, prove:

1. `StdoutReporter` can render a streamed assistant response from deltas cleanly
2. `JsonlReporter` can serialize every `AgentEvent` without provider-specific branches
3. `UsageReporter` can aggregate incremental and final usage correctly
4. `TranscriptReporter` can reconstruct a useful transcript from events alone
5. `CompositeReporter` preserves deterministic event order across children
6. a buffered adapter can exist without changing the `LoopObserver` contract

If any of those fail, the event model or the crate split probably needs adjustment.

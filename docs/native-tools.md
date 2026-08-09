# Native coding tools

M004-W10 defines the complete native model-facing coding surface. It contains exactly five
versioned tools: `kit.discover`, `kit.search`, `kit.read`, `kit.edit`, and `kit.run`.
Their canonical Draft 2020-12 JSON Schemas, digests, annotations, finite result bounds, capability
identities, grants, reservations, and retry classifications are owned by `NativeCatalog`.

The daemon supplies the project root through `DaemonConfig::with_project_root`; no native schema
accepts a host or workspace root. Agent attempts bind that trusted root, project, workspace,
principal, fence, and effective configuration into M001 capability grants. Provider calls are
accepted only by `ToolExecutorAdapter`, which persists intent before invoking the crate-private
native dispatcher and persists a terminal outcome or `outcome_unknown` before returning.

Read tools require `WorkspaceRead` and an expected revision. Search cursors are bound to the same
revision, index, query, and options. Edit requires `WorkspaceWrite` and takes the DR-0008
hunk-anchored input (v2): `{"version": 2, "operations": [...]}` where an operation is either
`{op: "edit", path, hunks: [{context_before, old, new, context_after}]}` over exact UTF-8 text
lines, `{op: "add_file", path, content, executable}` with `content` a plain string, or
`{op: "delete_file", path}`. Each hunk resolves against the current file content: exactly one
occurrence of `context_before + old + context_after` replaces `old` with `new`; zero occurrences
fail typed as `edit_anchor_not_found` (the caller's view is outdated — re-read the file);
several fail as `edit_anchor_ambiguous` (add context lines). An empty `old` inserts `new`
between the contexts; an empty `new` deletes `old`. Matching is exact per line after the file's
CRLF sequences are normalized to LF (mixed newlines and bare CR are rejected); the file's
newline flavor and final-newline state are preserved on write. There is no `expected_revision`,
per-operation digest, or byte range in the input — the file content itself is the concurrency
token. Resolved hunks lower to the canonical edit IR and enter the same production
`EditOrchestrator` as grammar output: authorization, validation, private staging
with syntax passes, materialization, and recovery. Edits work with no `.kit/native.json` present;
the trusted config only tunes the edit validation wall time, approval policy, and the optional
staged LSP diagnostics pass. When the config declares an `lsp` server and a changed file matches
its languages, a bounded shadow LSP session runs against the staged view between staging and
materialization; error and warning diagnostics ride along in the result under `diagnostics`
(or `diagnostics_unavailable` with a reason), and never block the edit. Its result identifies
the committed revision, diff artifact, and edit trace.

Run accepts argv, never a shell string. Working directory, mounts, scrubbed environment, network
policy, and finite resource limits are explicit. It uses only the executor selected by trusted
configuration. Isolation unavailability is a typed failure. Host compatibility is not a fallback
and additionally requires `HostProcessCompatibility`.

Restricted-container runs require a daemon-trusted pinned image configured with
`KIT_NATIVE_CONTAINER_IMAGE` or `DaemonConfig::with_native_container_image`. Runs are foreground
only and are durably registered with the attempt cancellation coordinator before release.

Every canonical result is at most 64 KiB and carries an explicit artifact-reference list. M001
copies that list onto the durable outcome event. Larger content remains in authenticated artifacts.
The dispatch module is crate-private, so release consumers can inspect descriptors but cannot
construct native dispatch authority.

Conformance evidence for KIT-TOOL-001 through KIT-TOOL-015 is in
`tests/conformance/native_tools.rs`. The M004-W10 bypass matrix is in
`tests/adversarial/native_tool_bypass.rs` and exercises 40 forged bindings with zero authorized
effects, in addition to the shared kernel bypass and fault suites.

Route-level evidence uses loopback-only HTTP fixtures and the deterministic run-conformance fake;
it performs no external or billable model call. Run the focused evidence with:

```sh
cargo test --lib capabilities::native::dispatch::tests
cargo test --test conformance native_tools
cargo test --test integration agent_run_tests::workspace_acquisition_failure_before_agent_route_has_zero_native_effects
```

The release sweep is `cargo test --test conformance`, `cargo test --test fault`,
`cargo test --test integration`, `cargo test --test adversarial`, `cargo fmt --check`, and
`cargo clippy --all-targets --all-features -- -D warnings`.

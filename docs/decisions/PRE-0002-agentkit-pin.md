# PRE-0002: Agentkit Vendored Snapshot Pin

- Unit: `0.02`
- Requirement: `KIT-AGENTKIT`
- Gate: `G00`
- Evidence type: `operational_assertion`
- Status: `CLOSED`

## Source Identity

The snapshot in `vendor/agentkit/` was built without modifying
`/Users/danielkov/projects/agentkit` from:

- commit: `c3926f1c4f3c945d400c8b6ef039da1f84826fcd`
- tree: `5befb5676ea31703f4485e2d4b5869c39a39cb0f`
- tracked files: `356`
- tracked dirty modifications: `23`
- dirty binary patch SHA-256:
  `92178443493858a217a04387442b56ecd2499e86b05b699aa76b685be146abd1`

The adjacent checkout was not modified, staged, cleaned, stashed, or otherwise
changed. The captured payload was subsequently normalized only for the compose
catalog regression described below.

## Exclusions

All `490` untracked files were generated benchmark outputs under
`benchmarks/compose-bench/results/2026-07-runlet-toon/`. Their 34 top-level
paths remain recorded in `docs/compatibility/pins/agentkit-snapshot.yaml`; the
sorted, newline-terminated path-list SHA-256 is
`6013053000cc27b0e77ed61964266ff38e246bee41fee9ef829c0d8763ecd3ae`.
Repository metadata and generated build caches are absent from the snapshot.

## Normalization

The prior crates.io substitution was removed. The captured tracked declaration
and lock semantics are restored exactly:

```toml
runlet = { path = "../../../runlet", optional = true }
```

The corresponding `Cargo.lock` package has no `source` or `checksum`, as Cargo
requires for a path package.

The full workspace exposed a compose catalog regression: compact object-shape
rendering preserved the dynamic child but dropped JSON quotes around its field
names, violating the integration contract for `"value"`. Production rendering
now JSON-quotes and sorts object property names in
`crates/agentkit-tool-compose/src/lib.rs`; its focused unit expectation was
updated in `src/tests.rs`. No integration assertion was weakened.

The dependency graph resolves entirely inside Kit:

```text
vendor/agentkit/crates/agentkit-tool-compose
  -- ../../../runlet --> vendor/runlet/Cargo.toml (runlet 0.1.0)
```

Cargo metadata's `source=null` is expected path-source semantics, not a
floating source. The Runlet bytes are repository-vendored and digest-verified.

## Snapshot Identity

- payload files: `357`
- manifest: `vendor/agentkit/SNAPSHOT-MANIFEST.sha256`
- aggregate SHA-256:
  `0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`
- license preserved: `vendor/agentkit/LICENSE`

`vendor/agentkit/SNAPSHOT-METADATA.yaml` records the source identity, patch,
exclusions, dependency graph, and final digest.

Two reconstructed files were normalized by the pinned Rust 1.94.0 rustfmt.
The formatter roundtrip is byte-identical to the payload, and its deterministic
patch SHA-256 is
`6e598fadf82872029ac172008e03f7d9d2795e54d985d545509ea55cbaea5ef4`.
Only line wrapping changed; tokens, behavior, schemas, and public API did not.

## Verification

- `cargo metadata --format-version 1 --locked`: pass; `runlet 0.1.0` resolves
  from `/Users/danielkov/projects/kit/vendor/runlet/Cargo.toml`, `source=null`.
- `CARGO_TARGET_DIR=/tmp/kit-agentkit-target cargo test --locked -p
  agentkit-tool-compose --features runlet`: pass; 23
  unit tests and one doctest passed.
- `CARGO_TARGET_DIR=/tmp/kit-runlet-target cargo test --locked --workspace`
  from `vendor/runlet`: pass; 75 tests and one doctest passed.
- Source reconstruction and both deterministic manifests: pass. The Runlet
  digest is `fef525f0008de628b1aff655d2e5685d2c826c76c8517c50e1ce8a88cfcbb8ef`;
  the normalized Agentkit digest is
  `0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`.
- `CARGO_TARGET_DIR=/tmp/kit-agentkit-target cargo test --locked --workspace
  --all-features`: pass, including
  `compose_source_tracks_dynamic_child_catalog`.
- The adjacent Runlet and Agentkit checkout status and diff digests are
  unchanged by normalization and verification.

## Decision

The vendored Agentkit snapshot is deterministic, its dependency graph is fully
repository-relative, the Runlet API mismatch is resolved, and every required
suite and digest check passes. `BLK-02` is closed.

## Timestamp

2026-08-02 (UTC)

## Current Overlay

M009-W01 retains this historical source capture and adds the pinned
`m009-post-validation-checkpoint` overlay; M006-W07a adds the pinned
`m006-mcp-protocol-revision-pin` overlay and produces aggregate
`0b10acaf53d52a4aa6cbfd183366de7bb401cf2d194efb239fddeed585c419c2`.
The local-only M006-W07b `m006-mcp-feature-adaptation` overlay produced
`3cc4569be6990cd88265f9e3d5d2c057c1cfd4eefad5da4ff0ece4150d758077`;
the complete payload after the local M006-W08 URL-capability overlay is
`5bf963f65dcab767a1585a45bf4fbdd21c56dbbb57ea936727825d5809e11dc4`;
exact changed-file hashes are
recorded in `src/agent/agentkit_patch/manifest.yaml` and
`src/protocols/mcp/agentkit_patch/manifest.yaml`, and the local-only scope and
complete chain are recorded in `requirements/reports/m006-w07c.md`.

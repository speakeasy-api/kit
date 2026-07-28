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
- tracked dirty modifications: `10`
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

- payload files: `356`
- manifest: `vendor/agentkit/SNAPSHOT-MANIFEST.sha256`
- aggregate SHA-256:
  `7a04d34e1509a0325bba5bd804f4d76afb6662ee7754d4ee903aa59b51867d0a`
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
  `7a04d34e1509a0325bba5bd804f4d76afb6662ee7754d4ee903aa59b51867d0a`.
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

2026-07-21 (UTC)

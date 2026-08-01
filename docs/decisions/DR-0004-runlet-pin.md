# DR-0004: Runlet Vendored Tree-Digest Pin

- Unit: `0.04`
- Blocker: `BLK-01`
- Gate: `G00`, `G07`
- Evidence type: `operational_assertion`
- Status: `CLOSED`

## Selection

RFC-approved option (c), a repository-vendored tree-digest pin, replaces the
incompatible crates.io `runlet 0.1.0` pin:

- package/version: `runlet 0.1.0`
- source commit: `188688891499386d19d225676d80a2274213f27f`
- source tree: `b71cc1a1298a9f08697495f415a1563a74f60c3f`
- tracked dirty patch SHA-256:
  `1a5ef63c2c4c1774425415fa807f3b6b8ef184c9fb6c5f1d18f2caedbad8cec5`
- final payload digest:
  `fef525f0008de628b1aff655d2e5685d2c826c76c8517c50e1ce8a88cfcbb8ef`
- license: `MIT OR Apache-2.0`; both license files are preserved

The superseded crates.io candidate was `runlet 0.1.0` with checksum
`ad9ae17555a7a7995252c892dc0655dea6c428a1cc39209cd9021b3dd858d43d`.
It remains historical registry evidence only and is not the application source.

`vendor/runlet/SNAPSHOT-METADATA.yaml` records the immutable base, dirty
overlay, expanded grammar gitlink, exclusions, normalization, and final digest.
`vendor/runlet/SNAPSHOT-MANIFEST.sha256` contains the sorted per-file hashes.

## Materialization

The snapshot was reconstructed from `git archive` of the source commit plus
the current tracked binary patch. The dirty grammar gitlink was expanded from
its own commit `bad1c07973884c2d46675e3ebff4a70c322b0e14`, tree
`0bfeebce27528bf8cf0a40313712817b92702ccd`, and tracked dirty patch
`78e5eef79df8e645e8b798db53bf4e354bbd87839622e43002ec53710b3c5f70`.
No `.git` directory was copied.

Six ignored generated parser files under `editors/tree-sitter-runlet/src/`
were excluded. Their sorted path-list SHA-256 is
`dd404aeb0b02394d2b7dc5c562df84a1a41c8b5cf40af05770f54a6498b5ca3e`.
There were no ordinary untracked files. The adjacent
`/Users/danielkov/projects/runlet` checkout was not modified, staged, cleaned,
or otherwise changed.

One stale corpus expectation was normalized after reconstruction. The exact
change in `tests/programs/p08_json_time_pipeline.rnlt` is:

```diff
-#! expect: { "n": 5, "yesterday": "2026-06-14T12:00:00.000Z", "encoded": "{\"n\":5}" }
+#! expect: { "n": 5, "yesterday": "2026-06-14T12:00:00Z", "encoded": "{\"n\":5}" }
```

The formatter contract is `time.format(ms) -> RFC 3339 UTC`. Its implementation
explicitly omits a zero fractional part, the semantic-model test pins `Z` for
whole seconds and `.250Z` for nonzero milliseconds, and RFC 3339 defines
`time-secfrac` as optional. Agentkit's calendar contract and examples also use
whole-second `Z`. The formatter is canonical and the corpus expectation was
stale.

Two reconstructed Rust files were also normalized by the pinned Rust 1.94.0
rustfmt. The formatter roundtrip is byte-identical to the payload, and its
deterministic patch SHA-256 is
`da354fef9418f6f54bcb95349e7f5da6da92d2970e2ae8620a1a9a9ca14ad12e`.
Only line wrapping changed; tokens, behavior, schemas, and public API did not.

## Scope Boundary and Current Cargo Metadata

The only application edge is repository-relative:

```text
vendor/agentkit/crates/agentkit-tool-compose
  -- path=../../../runlet --> vendor/runlet (runlet 0.1.0)
```

Cargo reports `source=null` for this package because path dependencies do not
have a registry or Git source identifier. This is expected and is not a
floating dependency: all resolved source bytes are inside this repository and
the complete payload is checked by the pinned SHA-256 manifest and aggregate
digest.

## Verification

Run from `vendor/agentkit`:

```text
$ cargo metadata --format-version 1 --locked
runlet  0.1.0  /Users/danielkov/projects/kit/vendor/runlet/Cargo.toml  source=null

$ CARGO_TARGET_DIR=/tmp/kit-agentkit-target cargo test --locked -p agentkit-tool-compose --features runlet
23 passed; 0 failed
doc-tests: 1 passed; 0 failed
```

The complete vendored Runlet suite passes with
`CARGO_TARGET_DIR=/tmp/kit-runlet-target cargo test --locked --workspace` from
`vendor/runlet`: `75` tests and one doctest. The
Agentkit Runlet-enabled compose suite passes: `23` tests and one doctest. Cargo
metadata resolves `runlet 0.1.0` from `vendor/runlet/Cargo.toml` with
`source=null`, as required for the repository-relative path dependency. Both
payload manifests have validated counts, ordering, file hashes, and final
digests; Agentkit's independently normalized payload digest is
`7a04d34e1509a0325bba5bd804f4d76afb6662ee7754d4ee903aa59b51867d0a`.

## Decision

The Runlet API mismatch is resolved by the immutable vendored tree-digest pin,
the stale corpus expectation is explicitly normalized, and all required suites
and digest checks pass. `BLK-01` is closed.

The Agentkit digest above is the historical payload used to close this Runlet decision. M009-W01
later adds a separately recorded Agentkit hook overlay; it does not modify the pinned Runlet payload.

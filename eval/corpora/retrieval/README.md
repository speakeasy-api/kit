# M005-W07 Preregistered Retrieval Protocol

This crate freezes an honest Rust retrieval corpus from published crates that are already present in
the root `Cargo.lock`. It contains no generated repositories, target symbols, answer-bearing paths,
or measured result. The retained report is `NOT_RUN_PRECOMMIT` and makes no C-L or G05 claim.
The v3 preregistration binds the sanitized `INVALID_HARNESS` incident from the v2 worker-abort run;
that incident contains no retrieval observation or machine-local path.

## Frozen Corpus

`prepare` consumes a fresh temporary `cargo vendor --locked --versioned-dirs` tree. It verifies each
generated `.cargo-checksum.json`, requires a valid `.cargo_vcs_info.json` 40-hex commit and
`path_in_vcs`, normalizes upstream repository URLs, keeps one package per repository, and requires
four real public documented Rust items. It sorts eligible snapshots by physical non-comment Rust
SLOC with a deterministic package tie break and takes the first 24, next 24, and last 24. Fewer than
72 unique eligible upstream repositories after all filters is a hard blocker.

For each selected snapshot, the minimum preregistered SHA-256 over package/source identity and every
eligible symbol/doc location is the target; the next three are wrong decoys. The task query is the
first sentence of the target's existing documentation. The reference operation inserts a harmless
`#[doc]` attribute at the exact registered target after retrieved context authorizes localization;
the expected complete post-edit tree digest is pinned. Registry trees are reproduced on demand and
are not committed.

## Trial Boundary

Every non-oracle arm is a fresh process with a fresh cache. It receives only a fresh source snapshot,
task query, arm configuration, and one arm-specific output path. `O` is not a worker; it is hidden
grader input. `F-S` is dependency-closed to lexical, filesystem metadata, parse-free Cargo metadata,
and Git path history, and must not construct a syntax index.

Trusted evidence must use the existing M004 production isolated executor. The deny-default macOS
`sandbox-exec` helper in this crate is explicitly `LOCAL_SANDBOX_NOT_TRUSTED`; it cannot satisfy G03,
G04, or G05. Raw arm records retain complete API candidates/ranges/snippets/provenance, timings,
truncation, and errors. Verification must reconstruct top-k from raw records before hidden-oracle
grading. Public receipts use the preregistered Ed25519 key, signed chained entries, and exact ledger
to table reconciliation; signing and public verification use the exactly locked Rust Ed25519
implementation, not OpenSSL or a PATH-resolved verifier.

Lexical candidates are actual fixed contexts around literal matches: at most two lines and 2048
bytes on each side, with the candidate range equal to the retained snippet. A candidate localizes an
item only when it is an exact item candidate or a lexical context containing that item's declaration
start. Wrong-decoy and downstream mechanical checks are treatment-arm guardrails for `C`, `F`, and
the preregistered `F` ablations; they are not requirements on `L`. Downstream grading uses relevant
retrieved context only to authorize the frozen edit at the exact hidden target in a fresh validated
tree.

## Commands

Reproduce the corpus before the preregistration commit only when intentionally changing the roster:

```sh
vendor="$(mktemp -d)"
cargo vendor --locked --versioned-dirs "$vendor" >/dev/null
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- prepare "$vendor"
rm -rf "$vendor"
```

After changing any pinned runner, schema, lane, or root execution/statistics input and before the
preregistration commit, refresh only the preregistration pins and retained `NOT_RUN` report:

```sh
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- refresh-frozen
```

The exact precommit/CI verification command does not regenerate or run the measured corpus:

```sh
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- verify
```

After committing all pinned files, create the caller-owned registry source directory and invoke the
macOS calibration route with the ignored 0600 private key. Before measurement, the command uses the
preregistered absolute Git executable to clone every upstream at depth 100, detach at the pinned VCS
commit, and validate HEAD, remote, subdirectory, and all pinned Rust bytes against the package
snapshot. Materialization or history failures are terminal; package snapshots never substitute for
upstream history. The run records Git clone receipts, runs 72 x 7 arms, and is intentionally not a
G05 or production-gate result:

```sh
vendor="$(mktemp -d)"
cargo vendor --locked --versioned-dirs "$vendor" >/dev/null
KIT_M005_W07_SIGNING_KEY=.kit/m005-w07-ed25519-private.pem \
  cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- run-local "$vendor"
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- verify "$vendor"
rm -rf "$vendor"
```

If setup or materialization fails before a measured report is produced, remove only the guarded
failed run directory before retrying:

```sh
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- cleanup-failed
```

The trusted command never falls back to local evidence. Until a pinned M004 adapter and G03/G04 are
available, it atomically writes the schema-valid zero-trial `BLOCKED_G03_G04` report; `verify`
accepts and validates that frozen blocked state:

```sh
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- run-trusted
```

# Release Rust API Verification

The release Rust API excludes test harnesses and direct control-plane mutation paths.

The pinned inspection tool is `cargo-public-api 0.52.0`, recorded in
`docs/compatibility/build-manifest.yaml`. Run:

```sh
cargo public-api --release
```

When that tool is unavailable, the repository's compile-fail proof is authoritative:

```sh
cargo test --test release_surface
```

The fixture compiles an external path consumer against the release profile and requires
`Service` construction, `NoopRuntime`, raw SQLite open/append, unregistered backup creation,
and `test_support` access all to fail.

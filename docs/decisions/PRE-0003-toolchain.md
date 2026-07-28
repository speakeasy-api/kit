# PRE-0003: Toolchain Fact

Unit `0.03` (`BLK-04`). Evidence type `O` (operational_assertion).

## Recorded facts

- `rustc --version` → `rustc 1.94.0 (4a4ef493e 2026-03-02)`
- `cargo --version` → `cargo 1.94.0 (85eff7c80 2026-01-15)`
- Timestamp (UTC): `2026-07-21T16:34:46Z`
- Pin source: `agentkit/Cargo.toml:58` → `rust-version = "1.92"`
- Active Kit pin: `rust-toolchain.toml` → `channel = "1.94.0"`

## Semver comparison

Installed/pinned `1.94.0` vs required `>=1.92`: `1.94.0 >= 1.92.0` → **PASS**.

## Gate

`CR` → `G00`.

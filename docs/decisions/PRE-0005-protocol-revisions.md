# PRE-0005: Protocol Revision Facts

Unit `0.05` (`BLK-05`, `BLK-06`). Evidence type `O` (operational_assertion).
Requirements: `KIT-MCP`, `KIT-ACP`.

## BLK-06 — ACP duplicate set

### Command

```
cd /Users/danielkov/projects/agentkit && cargo tree -d -p agent-client-protocol
```

### Output

```
error: specificationm `agent-client-protocol` is ambiguous
help: re-run this command with one of the following specifications
  agent-client-protocol@0.11.1
  agent-client-protocol@1.0.1
```

`cargo tree -d -p <spec>` requires a disambiguated spec once two versions are
linked; the ambiguity error is itself confirmation of duplication. Equivalent
command used to enumerate the full duplicate set (`cargo tree -d` with no
`-p`, filtered to the crate name):

```
cd /Users/danielkov/projects/agentkit && cargo tree -d 2>&1 | grep -A4 '^agent-client-protocol v'
```

### Exact output

```
agent-client-protocol v0.11.1
└── agent-client-protocol-tokio v0.11.1
    └── agentkit-acp v0.10.2 (/Users/danielkov/projects/agentkit/crates/agentkit-acp)
        └── openrouter-acp-trio v0.10.2 (/Users/danielkov/projects/agentkit/examples/openrouter-acp-trio)

agent-client-protocol v1.0.1
├── agentkit-acp v0.10.2 (/Users/danielkov/projects/agentkit/crates/agentkit-acp) (*)
└── openrouter-acp-trio v0.10.2 (/Users/danielkov/projects/agentkit/examples/openrouter-acp-trio)
```

### Recorded duplicate set

| Crate | Version | Path to root | Cargo.lock lines |
| --- | --- | --- | --- |
| `agent-client-protocol` | `0.11.1` | direct dep of `agent-client-protocol-tokio 0.11.1` (optional, feature `stdio`, default-on) → `agentkit-acp 0.10.2` (`crates/agentkit-acp/Cargo.toml:18` declares `agent-client-protocol-tokio = { version = "0.11.1", optional = true }` under `default = ["stdio"]`) | `Cargo.lock:12-15` |
| `agent-client-protocol` | `1.0.1` | direct dep of `agentkit-acp 0.10.2` (`crates/agentkit-acp/Cargo.toml:18` — `agent-client-protocol = "1.0.1"`) | `Cargo.lock:36-39` |

Two copies of `agent-client-protocol` (major `0` and major `1`) are linked
simultaneously through `agentkit-acp`'s default feature set, exactly as
`BLK-06` states. Companion duplicates confirmed in the same tree walk:
`agent-client-protocol-derive` (`0.11.1` + `1.0.1`), `agent-client-protocol-schema`
(`0.12.0` + `1.1.0`).

### Binary criterion status

`cargo tree -d -p agent-client-protocol` → does **not** show one copy; two
copies recorded above. Criterion (`11.02` row: "blocked-by `BLK-06` until
`0.05` yields a single `agent-client-protocol` version") is **not yet met** —
`BLK-06` remains open. This unit's obligation is the recorded fact only, not
the fix; resolution (upgrade helper / drop `stdio` default / pin one major) is
`11.02`'s dependency to close before it can proceed.

## BLK-05 — `rmcp_model::ProtocolVersion::LATEST` resolution

### Pinned version

`Cargo.lock:2929-2932` (agentkit workspace lock):

```
name = "rmcp"
version = "1.5.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "67d69668de0b0ccd9cc435f700f3b39a7861863cf37a15e1f304ea78688a4826"
```

Call site: `crates/agentkit-mcp/src/lib.rs:1324` —
`.with_protocol_version(rmcp_model::ProtocolVersion::LATEST)`.

### Probe method

Installed source for the pinned version, resolved by cargo's own registry
cache (no network probe needed — the crate is already fetched by the
workspace lock):

```
find ~/.cargo/registry/src -maxdepth 2 -iname "rmcp-1.5.0"
```

```
/Users/danielkov/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rmcp-1.5.0
```

```
grep -n "pub const LATEST" -r /Users/danielkov/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rmcp-1.5.0/
```

```
/Users/danielkov/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rmcp-1.5.0/src/model.rs:159:    pub const LATEST: Self = Self::V_2025_11_25;
```

Source excerpt (`rmcp-1.5.0/src/model.rs:154-159`):

```rust
pub const V_2025_11_25: Self = Self(Cow::Borrowed("2025-11-25"));
pub const V_2025_06_18: Self = Self(Cow::Borrowed("2025-06-18"));
pub const V_2025_03_26: Self = Self(Cow::Borrowed("2025-03-26"));
pub const V_2024_11_05: Self = Self(Cow::Borrowed("2024-11-05"));
pub const LATEST: Self = Self::V_2025_11_25;
```

### Resolved value

`rmcp_model::ProtocolVersion::LATEST` in pinned `rmcp 1.5.0` resolves to the
string `"2025-11-25"`.

### Comparison to RFC pin

RFC pin (`RFC.md:567`): MCP revision `2025-11-25`.

Resolved `LATEST` (`rmcp 1.5.0`): `2025-11-25`.

**Match: MATCH** — current pinned dependency's `LATEST` equals the RFC-required
revision today.

### Risk recorded (why `BLK-05` stays open despite the current match)

`LATEST` is a moving alias re-exported by upstream `rmcp`, not an
explicit constant Kit's adapter owns — a future `rmcp` version bump silently
changes what `LATEST` resolves to, with no compile-time or review signal at
the call site `crates/agentkit-mcp/src/lib.rs:1324`. The match recorded above
holds only for `rmcp 1.5.0` as currently pinned in
`agentkit/Cargo.lock:2929-2932`. `BLK-05`'s required action (replace `LATEST`
with an explicit `ProtocolVersion::V_2025_11_25` constant, or an equivalent
adapter-owned pin, and assert negotiated revision `== 2025-11-25` in the
conformance suite) is unresolved by this unit; this unit only supplies the
measured fact the fix and the conformance assertion (`7.05`) depend on.

## Timestamp

2026-07-21T16:34:46Z (UTC)

## Gate

`CR` → `G06`, `G10`.

---

## Structured return

```
status:   accepted
changed:  docs/decisions/PRE-0005-protocol-revisions.md
criteria:
  1 (cargo tree -d -p agent-client-protocol -> recorded duplicate set): pass
    - observed: agent-client-protocol v0.11.1 (via agent-client-protocol-tokio
      0.11.1, default feature "stdio") and v1.0.1 (direct dep), both under
      agentkit-acp 0.10.2 (Cargo.lock:12-15, :36-39). Two copies confirmed.
  2 (rmcp_model::ProtocolVersion::LATEST resolved and compared to 2025-11-25):
    pass
    - observed: LATEST = "2025-11-25" (rmcp 1.5.0, model.rs:159) == RFC.md:567
      pin "2025-11-25" -> MATCH
evidence:
  EV-0.05-O-001 -> job local-probe -> ACP duplicate set recorded (2 copies of
    agent-client-protocol: 0.11.1, 1.0.1); rmcp LATEST resolved = 2025-11-25,
    matches RFC pin
blockers:
  BLK-05 (still open): rmcp_model::ProtocolVersion::LATEST is an upstream
    floating alias, not an explicit Kit-owned pin; matches RFC.md:567 today
    only incidentally to rmcp 1.5.0 being pinned. Owner: Protocol owner.
    Action: replace LATEST with explicit ProtocolVersion::V_2025_11_25 (or
    adapter-level constant) at agentkit-mcp/src/lib.rs:1324, then assert
    negotiated revision == 2025-11-25 in conformance suite (unit 7.05).
    Verification: adapter call site no longer references LATEST; conformance
    suite (M010-W09) rejects any non-2025-11-25 negotiation.
  BLK-06 (still open): two agent-client-protocol majors (0.11.1, 1.0.1)
    linked simultaneously via agentkit-acp's default "stdio" feature pulling
    in agent-client-protocol-tokio 0.11.1 alongside the direct 1.0.1 dep.
    Owner: Protocol owner. Action: upgrade agent-client-protocol-tokio to an
    ACP-1.x-compatible release, or drop the "stdio" default feature and own
    the transport directly, or pin both paths to one ACP major. Verification:
    cargo tree -d -p agent-client-protocol@<one-version> shows single
    resolution (no ambiguity error, one version only).
notes: |
  This unit records facts only; it does not modify agentkit or fix either
  blocker. 11.02 and 7.05 remain blocked on BLK-06 and BLK-05 respectively
  until the owning fixes land and are verified per the criteria above.
```

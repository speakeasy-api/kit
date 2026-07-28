# PRE-0006: TOON WD 3.3 Conformance Probe (serde_toon2 0.2.0)

- Unit: `0.06`
- Work package: n/a (`BLK-10`)
- Requirement: `KIT-ENCODE`
- Gate: `CR` → `G07`
- Evidence type: `conformance` (`C`)
- Evidence id: `EV-0.06-C-001`

## Obligation

`RFC.md:687` — "Kit initially pins TOON Working Draft 3.3 and its conformance
tests." `RFC.md:1516` — TOON specification source: `https://github.com/toon-format/spec/blob/main/SPEC.md`.
Implementation under pin: `serde_toon2 0.2.0`, declared at
`/Users/danielkov/projects/agentkit/crates/agentkit-tool-compose/Cargo.toml:20`
(`serde_toon2 = { version = "0.2.0", optional = true }`), resolved at
`/Users/danielkov/projects/agentkit/Cargo.lock:3349-3355` with checksum
`2fd2fdb12173bc1bcd03dc729a9b50c7ac7f7a5f35223c5a7c7100917b0d7273`.

plan.md row `0.06` criterion: vector set fetched from spec source `RFC.md:1516`,
executed against `serde_toon2 0.2.0`, pass count == vector count, `0` skipped;
`pass < total` → `BLK-10` stays open.

## Spec source pin resolution

`RFC.md:1516` points at the `main` branch of `toon-format/spec`, a moving
target. `RFC.md:687` requires pinning Working Draft **3.3** specifically, not
whatever `main` currently contains. The repository's own `CHANGELOG.md`
records spec version `3.3` as a single dated entry (`## [3.3] - 2026-05-21`);
per `VERSIONING.md`, published npm/tag versions use full `MAJOR.MINOR.PATCH`
where "PATCH releases are packaging or editorial-only and do not change the
specification version" and "Implementations targeting a spec version should
pin to the `MAJOR.MINOR` line" (default: latest patch). Three tags exist
under the `3.3` line: `v3.3.0`, `v3.3.1`, `v3.3.2`. This probe pins the latest,
`v3.3.2`, as the representative artifact of WD 3.3.

```
git ls-remote --tags https://github.com/toon-format/spec.git v3.3.2
f95445954f444cf093aef3d701becf766aab19fa	refs/tags/v3.3.2
```

Tag object resolves (via GitHub API) to commit:

```
gh api repos/toon-format/spec/tags
-> v3.3.2: commit.sha = 87146b38292d5b71c04b5fcb9496c20fe1647b05
```

Source URL: `https://github.com/toon-format/spec/tree/v3.3.2` (SPEC.md at this
ref: `https://github.com/toon-format/spec/blob/v3.3.2/SPEC.md`).

Fetch command and tarball digest:

```
gh api repos/toon-format/spec/tarball/refs/tags/v3.3.2 > spec-v3.3.2.tar.gz
shasum -a 256 spec-v3.3.2.tar.gz
a419235bba97b8da69e6358a3162889ec78d826c16bf027e16240123165e6096  spec-v3.3.2.tar.gz
```

Vector set used: `tests/fixtures/{encode,decode}/*.json` from this tarball —
22 files (9 encode, 13 decode), exactly matching the file list in
`tests/README.md` at this ref. Per-file digests and their own manifest digest:

```
cd spec-tests/fixtures && find . -type f -name "*.json" | sort | xargs shasum -a 256 > fixture-manifest.sha256
shasum -a 256 fixture-manifest.sha256
e577d51433cd449d37d307b1563d26c9c37344c9299cf0ab31a2c8db5fa45d97  fixture-manifest.sha256
```

(Full 22-line per-file manifest retained at
`/var/folders/t9/w5z6dqrs68nfb_spxn62h32w0000gn/T/opencode/toon-conformance-probe/fixture-manifest.sha256`,
a temp probe artifact, not part of this repo.)

## Probe harness

A standalone Rust binary crate was built under
`/var/folders/t9/w5z6dqrs68nfb_spxn62h32w0000gn/T/opencode/toon-conformance-probe`
(outside repo write scope, per this worker's temp-probe allowance). It pins
`serde_toon2 = "=0.2.0"` from crates.io (not the vendored copy inside the
`serde_toon2` crate's own repo) and drives it through the fixture schema
documented in the fetched `tests/README.md`: for each `encode/*.json` fixture,
`serde_toon2::to_string_with_options(<input>, <options>)` is compared against
`expected` (or asserted to error, for `shouldError: true`); for each
`decode/*.json` fixture, `serde_toon2::from_str_with_options::<serde_json::Value>(<input>, <options>)`
is compared against `expected`. Decoder defaults not overridden by a test's
`options` object are set to the fixture-documented defaults
(`indent: 2, strict: true, expandPaths: "off"`) per `tests/README.md`, not the
crate's own `DecoderOptions::default()` (`strict: false`) — the two differ,
and the fixture-documented default is the one the conformance obligation
binds to.

Cargo resolution confirms the exact same published artifact as the one
already pinned in the kit/agentkit workspace:

```
grep -n "serde_toon2" -A4 Cargo.lock   # (probe's own lockfile)
name = "serde_toon2"
version = "0.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2fd2fdb12173bc1bcd03dc729a9b50c7ac7f7a5f35223c5a7c7100917b0d7273"
```

Toolchain on the verification rerun: `rustc 1.94.0 (4a4ef493e 2026-03-02)`,
`cargo 1.94.0 (85eff7c80 2026-01-15)`.

## Exact commands

```
cargo build --release
./target/release/probe ./spec-tests/fixtures
```

## Exact result (verbatim summary; 0 skipped — every fixture in every one of
the 22 files was executed and scored pass or fail, no test excluded)

```
decode/arrays-nested.json: 19/22
decode/arrays-primitive.json: 16/19
decode/arrays-tabular.json: 8/8
decode/blank-lines.json: 14/14
decode/delimiters.json: 28/28
decode/indentation-errors.json: 12/12
decode/numbers.json: 22/22
decode/objects.json: 39/43
decode/path-expansion.json: 12/12
decode/primitives.json: 26/28
decode/root-form.json: 4/5
decode/validation-errors.json: 16/28
decode/whitespace.json: 6/6
encode/arrays-nested.json: 11/13
encode/arrays-objects.json: 14/17
encode/arrays-primitive.json: 10/12
encode/arrays-tabular.json: 6/6
encode/delimiters.json: 22/22
encode/key-folding.json: 12/12
encode/objects.json: 26/28
encode/primitives.json: 39/40
encode/whitespace.json: 3/3
TOTAL: 365/400
FILES: 22
```

`365/400` pass, `0` skipped, `35` failures, exit code `1`.

### Failures (all 35, verbatim from probe stdout)

```
decode/arrays-nested.json:
  FAIL parses list arrays for non-uniform objects (spec 9.4): error Blank lines are not allowed inside arrays at line 4, column 1
  FAIL parses list arrays with deeply nested objects (spec 10): error Blank lines are not allowed inside arrays at line 5, column 1
  FAIL parses root-level array mixing primitive, object, and array of objects in list format (spec 9.4): error Blank lines are not allowed inside arrays at line 5, column 1
decode/arrays-primitive.json:
  FAIL decodes canonical empty array key: [] (spec 9.1): got {"items":"[]"}
  FAIL decodes canonical empty array with quoted key (spec 9.1): got {"x-custom":"[]"}
  FAIL decodes canonical empty array with empty-string key (spec 9.1): got {"":"[]"}
decode/objects.json:
  FAIL parses unquoted value shaped like an inline array header after the key (spec 6): error Expected 2 values, got 1 at line 1, column 1
  FAIL parses unquoted value shaped like a tabular array header after the key (spec 6): error Expected 2 values, got 3 at line 1, column 1
  FAIL decodes \uXXXX in quoted key (U+0004 control character) (spec 7.1): error Invalid escape sequence: \u at line 1, column 1
  FAIL decodes \uXXXX in quoted key (case-insensitive hex) (spec 7.1): error Invalid escape sequence: \u at line 1, column 1
decode/primitives.json:
  FAIL decodes \uXXXX escape (U+0004) (spec 7.1): error Invalid escape sequence: \u at line 1, column 1
  FAIL decodes \uXXXX with mixed-case hex digits (spec 7.1): error Invalid escape sequence: \u at line 1, column 1
decode/root-form.json:
  FAIL parses literal [] at root as empty array (spec 5): got "[]"
decode/validation-errors.json:
  FAIL throws on extra brackets between bracket segment and colon in strict mode (spec 6): got {"foo[1][bar]":10}
  FAIL throws on text between bracket segment and colon in strict mode (spec 6): got {"foo[2]extra":"a,b"}
  FAIL throws on non-integer bracket segment in strict mode (spec 6): got {"foo[bar]":10}
  FAIL throws on duplicate sibling keys in strict mode (spec 14.4): got {"name":"Bob"}
  FAIL throws on bracket length with leading zeros in strict mode (spec 6): got {"items":["a","b","c"]}
  FAIL throws on negative bracket length in strict mode (spec 6): got {"items[-1]":"a,b,c"}
  FAIL throws on decimal bracket length in strict mode (spec 6): got {"x[3.7]":"a,b,c"}
  FAIL throws on bracket length with plus sign in strict mode (spec 6): got {"x":["a","b","c"]}
  FAIL throws on bracket length in exponent form in strict mode (spec 6): got {"x[1e1]":"1,2,3,4,5,6,7,8,9,10"}
  FAIL throws on whitespace between bracket segment and fields segment in strict mode (spec 6): got {"items":[{"a":1,"b":2},{"a":3,"b":4}]}
  FAIL throws on nested duplicate sibling keys in strict mode (spec 14.4): got {"outer":{"name":"Bob"}}
  FAIL throws on duplicate keys within a list-item object in strict mode (spec 14.4): got {"items":[{"id":2}]}
encode/arrays-nested.json:
  FAIL encodes empty root-level array (spec 9.1): got "[0]:"
  FAIL encodes complex nested structure (spec 8): got "user:\n  id: 123\n  name: Ada\n  tags[2]: reading,gaming\n  active: true\n  prefs[0]:"
encode/arrays-objects.json:
  FAIL encodes objects with empty arrays in list format (spec 10): got "items[1]:\n  - name: Ada\n    data[0]:"
  FAIL places empty arrays on hyphen line when first (spec 10): got "items[1]:\n  - data[0]:\n    name: x"
  FAIL uses expanded list for arrays containing empty objects (spec 9.4): got "items[2]{}:\n  \n  "
encode/arrays-primitive.json:
  FAIL encodes empty arrays (spec 9.1): got "items[0]:"
  FAIL encodes empty string keys for empty arrays (spec 9.1): got "\"\"[0]:"
encode/objects.json:
  FAIL escapes U+0004 control character in key via \uXXXX (spec 7.1): got "\"ab\": 1"
  FAIL escapes U+001F control character in key via \uXXXX (spec 7.1): got "\"xy\": 2"
encode/primitives.json:
  FAIL encodes string with U+0004 control character via \uXXXX (spec 7.1): got "val: ab"
```

Failure clusters observed (recorded as fact, not remediated by this unit):
canonical empty-array form `key: []` (§9.1, decode + encode both sides);
`\uXXXX` control-character escaping in both directions (§7.1); several
strict-mode error cases that `serde_toon2 0.2.0` accepts instead of rejecting
(§6, §14.4); blank-line handling inside non-uniform list arrays (§9.4/§10);
root-level literal `[]` decode (§5).

## Decision

**Non-conformant. `BLK-10` stays open.** `365/400 != 400/400`; the plan.md
row `0.06` criterion ("pass count == vector count, `0` skips") is not met.

Per plan.md row `0.06` and `BLK-10`'s recorded action (plan.md:167): the
choice between (a) pinning a conformant encoder or (b) registering a
not-selected disposition with fallback = canonical compact JSON
(`IMPLEMENTATION_PLAN.md:802`) is `BLK-10`'s owner decision, not this unit's —
this unit's obligation is the measured conformance fact above, not the fix.
No encoder substitution, patch, or disposition registration was made by this
worker; write scope for this unit is this file only.

## Match status

NON-CONFORMANT — `365/400` (`91.25%`), `0` skipped, `35` failed, `BLK-10` open

## Timestamp

2026-07-21T16:36:36Z (UTC)

## Gate

`CR` → `G07`

---

## Structured return

```
status:   blocked
changed:  docs/decisions/PRE-0006-toon.md
criteria:
  1 (vector set fetched from spec source RFC.md:1516, pinned to WD 3.3):
    pass
    - observed: toon-format/spec tag v3.3.2 (tag ref f95445954f444cf093aef3d701becf766aab19fa,
      commit 87146b38292d5b71c04b5fcb9496c20fe1647b05), tarball sha256
      a419235bba97b8da69e6358a3162889ec78d826c16bf027e16240123165e6096,
      22 fixture files (9 encode, 13 decode), manifest sha256
      e577d51433cd449d37d307b1563d26c9c37344c9299cf0ab31a2c8db5fa45d97
  2 (vectors executed against serde_toon2 0.2.0): pass
    - observed: crates.io serde_toon2 0.2.0, checksum
      2fd2fdb12173bc1bcd03dc729a9b50c7ac7f7a5f35223c5a7c7100917b0d7273,
      identical to agentkit/Cargo.lock:3352 pin; probe ran
      `cargo build --release && ./target/release/probe ./spec-tests/fixtures`
  3 (pass count == vector count, 0 skips): fail
    - observed: 365/400 pass, 0 skipped, 35 failed -> BLK-10 stays open
evidence:
  EV-0.06-C-001 -> job local-probe -> 365/400 pass, 0 skipped, 35 failed,
    against toon-format/spec v3.3.2 (commit 87146b38292d5b71c04b5fcb9496c20fe1647b05)
    and serde_toon2 0.2.0 (checksum 2fd2fdb12173bc1bcd03dc729a9b50c7ac7f7a5f35223c5a7c7100917b0d7273)
blockers:
  BLK-10 (still open): serde_toon2 0.2.0 fails 35/400 WD 3.3 conformance
    vectors (empty-array canonical form §9.1, \uXXXX control-char escaping
    §7.1, several strict-mode rejections not enforced §6/§14.4, blank lines
    inside non-uniform list arrays §9.4/§10, root literal [] decode §5).
    Owner: Encoding owner. Action: per plan.md row 0.06 / BLK-10 (plan.md:167),
    either pin a conformant encoder or register a not-selected disposition
    with fallback = canonical compact JSON (IMPLEMENTATION_PLAN.md:802).
    Verification: re-run this probe's vector set (or its CI-landed successor,
    8.04 / tests/conformance/encode_toon.rs) against the replacement/disposition
    and confirm pass count == 400, 0 skips.
notes: |
  This unit records the measured conformance fact only; it does not select a
  disposition, patch serde_toon2, or modify agentkit/RFC.md/IMPLEMENTATION_PLAN.md/
  the superworkflow plan. 8.04 (TOON WD 3.3 presentation binding) stays gated on
  BLK-10 per plan.md:643 until a conformant encoder or a registered
  not-selected disposition closes this blocker.
```

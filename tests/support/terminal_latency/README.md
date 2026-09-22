# Real TUI terminal-output latency probe

This opt-in probe uses only existing Rust dependencies and Python's standard
library. It calls the actual public TUI entry point, including crossterm input,
the real event loop, HyperlinkBackend, ANSI output, and a controlling PTY. The
TUI's normal `current_exe() serve ...` child launch is dispatched by a test-only
executable to a local ACP v2 fixture. There is no provider, credential, or network
service. No production source, manifest, or stored session needs modification.

This is **key write → arrival of rendered glyph bytes and their synchronized-frame end at the PTY master**,
including scheduler and reader overhead. It is not terminal-emulator paint or
screen-photon latency. It bypasses CLI configuration parsing and the real ACP
server/provider, not the production TUI loop. It is not a TestBackend benchmark.

## Run

Use detached temporary worktrees for each revision. Copy `driver.rs` into each
worktree as `examples/terminal_latency_probe.rs` (Cargo auto-discovers examples).
In that worktree, use the pinned toolchain:

```sh
mise run build:release -- --example terminal_latency_probe
python3 /absolute/path/to/tests/support/terminal_latency/probe.py \
  /absolute/path/to/target/release/examples/terminal_latency_probe \
  --output /tmp/latency-current --samples 100 --history 1000
```

Trust the temporary worktree's mise configuration if required. To reuse build
artifacts, set `CARGO_TARGET_DIR` to an existing target directory, build revisions
sequentially, and copy each finished example executable to a revision-specific
path before building the next. **The build task also rebuilds `target/release/kit`;
a shared target can therefore leave that binary at a baseline or experimental
revision. After all timed comparisons, run `mise run build:release` from the
original checkout to restore its binary. Do not replace an installed binary or
restart live sessions for this probe.** The executable uses the probe script supplied by
the runner, so both revisions execute the same fixture. Never run comparative
measurements concurrently with compilation or another measurement.

The runner isolates HOME and cwd in a temporary directory and constructs an
environment allowlist. It asserts raw/no-echo terminal mode to reject accidental
kernel-echo measurements. The ACP fixture contains only generated text. Idle
samples alternate Q/Z insertion and backspace. The fixture never emits these
uppercase glyphs. Hot samples begin 100 ms after submit, during a deterministic burst of `--history` distinct markdown messages, then
one markdown chunk every 5 ms until the runner finishes all hot samples. The runner
also signals the fixture to stop on measurement failure; there is no fixed stream
duration or chunk limit that can turn a long hot run into idle samples. Both paths use a 120×40 PTY. Each insertion waits
for its corresponding printable output bytes and the following synchronized-output
end marker (`CSI ? 2026 l`); terminal escape sequences are
removed before matching. A 30 ms drain follows each deletion. This is deliberately
paced typing, not a saturated keyboard-throughput test.

`summary.json` records the binary SHA-256, nearest-rank p50/p95/p99/max, and
per-mode elapsed time, output bytes, and completed synchronized frames.
`events.jsonl` retains monotonic
key/read timestamps and individual latency samples; `terminal.bin` retains raw
ANSI output. `agent-requests.jsonl` contains only requests to the synthetic fixture.
The runner also requires that streaming text was actually rendered during the hot run. A timeout is a failure, not a dropped sample. Keep the logs local.
The percentile includes transport and Python scheduling delay and is an upper
bound on the backend's write completion; there is no physical renderer in this
probe. Repeat runs and vary history depth before drawing causal conclusions.

## Event-reader contention

Each binary executes its revision's production event-reader architecture.
Comparisons with older revisions can expose the effect of replacing EventStream
and synchronous UI-thread polling with a dedicated reader/channel. A revision
comparison alone does not isolate reader effects from other changes: use a
separately labeled, single-change variant when investigating causality.

The support checks can run without launching a TUI:

```sh
python3 -m unittest discover -s tests/support/terminal_latency -p 'test_*.py'
```

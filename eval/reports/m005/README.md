# M005 Retrieval Reports

`source-semantics/corpus-manifest.json` freezes 72 repository-unique crates.io source snapshots,
their upstream VCS commits/subdirectories, and deterministically selected tasks/oracles.
`source-semantics/retrieval-report.json` is the exact signed v6 measured report. It has not been
edited post hoc. The local, non-production calibration completed all 72/72 preregistered primary
C/L pairs and returned `FAIL_LOCAL_CALIBRATION` / `FAIL`:

| Rust SLOC class | Pairs | L successes | C successes | C-L estimate | 98.3333% interval | Passed |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| small | 24 | 21 | 24 | 0.1250 | [-0.1905, 0.3938] | no |
| medium | 24 | 14 | 16 | 0.0833 | [-0.2002, 0.3389] | no |
| large | 24 | 18 | 18 | 0.0000 | [-0.2042, 0.2042] | no |

The four secondary full-arm raw-bound rows below are descriptive and censored: raw `COMPLETE`,
graded `terminal_success`, and `latency_success` are distinct counts. Terminal/source errors remain
failures, with no imputation or inferential claim. The raw trial file is approximately 461 MiB
(483,489,120 bytes).

| F scope | Trials | Raw `COMPLETE` | `terminal_success` | Localized | `latency_success` | Graph errors | History errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| small | 24 | 24 | 23 | 2 | 21 | 1 | 0 |
| medium | 24 | 24 | 24 | 1 | 16 | 0 | 0 |
| large | 24 | 23 | 16 | 0 | 4 | 2 | 6 |
| all | 72 | 71 | 63 | 3 | 41 | 3 | 6 |

The graph errors were one deterministic-bound and two invalid-contract results. The history errors
were two deterministic-bound, three invalid-contract, and one invalid-request result. These local
negative results make no equivalence, harm, or production claim. G05 remains `NONE`; G03, G04,
BLK-14, and EXT-15 remain external blockers.

The lossless archive is [evidence/m005-w07/v6/manifest.json](../../../evidence/m005-w07/v6/manifest.json)
and is bound to source commit `c6f00fe6dcd51ccfc6d571708d65da6d8fbb0dab`. Structural and full
historical verification commands are:

```sh
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- \
  archive-check evidence/m005-w07/v6/manifest.json
cargo run --locked --manifest-path eval/corpora/retrieval/Cargo.toml -- \
  archive-verify evidence/m005-w07/v6/manifest.json VENDOR_DIR
```

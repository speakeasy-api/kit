# Research brief: blazing-fast `@`-file completion in Kit

_Researched 2026-08-31 from primary sources only: project source trees, first-party documentation, checked-in benchmarks, repository metadata APIs, and crate metadata APIs. Published benchmark values are reported as their authors scoped them; no cross-project numbers were normalized or extrapolated._

## Executive recommendation

Use an in-process, streaming architecture modeled most closely on **Codex’s public `codex_file_search` implementation** and **Helix’s file picker**:

1. Start a parallel, ignore-aware walk with [`ignore::WalkBuilder::build_parallel`](https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html#method.build_parallel).
2. Stream relative paths directly into a long-lived [`nucleo::Nucleo`](https://docs.rs/nucleo/latest/nucleo/struct.Nucleo.html) index via its injector.
3. Update only the Nucleo pattern on each keystroke; do **not** rescan or spawn `fd`, `rg`, `fzf`, or `skim` per query.
4. Publish partial top-N results while the initial walk is still running.
5. Keep the index alive for at least the lifetime of the TUI/workspace, rather than dropping it whenever the `@` token is dismissed.
6. Initially rebuild on explicit invalidation or a bounded TTL. Add filesystem watching later if profiling shows stale results matter.
7. Preserve a real `PathBuf` for opening files and use a separate UTF-8 display/match string so Unix paths containing invalid UTF-8 are not silently lost.

This separates the two latency regimes:

- **Cold scan/index-build:** filesystem traversal, ignore parsing, path conversion, and candidate insertion.
- **Warm query:** Nucleo pattern update and top-N ranking over the existing index.

The best available primary-source matcher benchmark reports **2.12–9.53 ms** for Nucleo over Linux-kernel paths versus **16.85–35.46 ms** for skim, depending on query. The author explicitly calls this a work-in-progress matcher benchmark, not a cold filesystem benchmark. On roughly three million already-loaded items, the same README reports an uncontrolled demonstration of about **1 second for fzf versus one 1/30-second frame for Nucleo**.  
Source: [Nucleo benchmark README](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/README.md#benchmarks).

---

## 1. Latency: cold scan versus warm query

### Published primary-source numbers

| Source | What it measures | Candidate state | Result | Interpretation |
|---|---|---:|---:|---|
| [Nucleo matcher microbenchmarks](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/README.md#matcher-micro-benchmarks) | Matching various patterns against all paths in a Linux-kernel checkout | Paths already collected; **no filesystem scan** | Nucleo: 2.12–9.53 ms; skim: 16.85–35.46 ms | Relevant to **warm per-query latency only** |
| [Nucleo/fzf demonstration](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/README.md#comparison-with-fzf) | Querying about 3 million loaded items | Warm corpus | About 1 s for fzf versus about 1/30 s for Nucleo | Useful directionally, but the author labels the comparison unscientific |
| [fd benchmark](https://github.com/sharkdp/fd/blob/cdea7f56331ebc7bb799aa9f718a6eee9a4a1cfc/README.md#benchmark) | Full traversal of ~750,000 directories and ~4 million files, with regex filtering | Warm/pre-filled OS disk cache | `fd -u`: 854.8 ms ± 10.0 ms | A **scan-per-invocation** measurement, not a reusable index; far too slow for each keystroke |
| [ripgrep README benchmarks](https://github.com/BurntSushi/ripgrep/blob/3fce3b5bb0236da2df6d99672afb8a719642eca7/README.md#quick-examples-comparing-tools) | Recursive **content search**, including file reads and regex matching | Mixed | E.g. 0.0817 s on its Linux-kernel benchmark | Not a file-completion/index benchmark; do not use it as evidence for `@` warm latency |
| [skim benchmark harness](https://github.com/skim-rs/skim/blob/4b962af9ede6f3fdda608bdce8031c352a4effb4/benches/cli.rs) | Interactive ingestion and matching, default 1 million generated items | Harness available | No stable checked-in comparative result table found | Useful for Kit-specific evaluation, but not primary-source published numbers |

### What primary sources do **not** establish

I found no maintained primary-source end-to-end benchmark that fairly measures all of:

1. process start,
2. ignore-aware project traversal,
3. index construction,
4. first visible top-N result,
5. completed initial scan,
6. subsequent one-character query updates,
7. memory/RSS.

VS Code, Zed, Helix, Codex, Gemini CLI, and the Rust matcher libraries generally have architecture and tests in source, but not comparable published cold/warm benchmark tables. **No primary source found substantiates sub-millisecond end-to-end querying on a large repository.** Sub-millisecond work is feasible as an engineering target for token parsing, pattern publication, cache hits, small corpora, and UI-thread handling; the measured full-corpus ranking target should initially be p50 below one frame and p95 below roughly 10 ms, with a separate aspirational `<1 ms` bucket by corpus size. Any selection should therefore be validated in Kit with separate metrics such as:

- `time_to_first_result`
- `time_to_walk_complete`
- `warm_query_p50/p95`
- candidate count
- bytes held by candidate/index storage
- rebuild count
- result correctness under ignore and Unicode fixtures

---

## 2. Matching engines

### Nucleo / `nucleo-matcher` — strongest fit

**Architecture**

- `nucleo-matcher` is the lower-level stateless matcher; `nucleo` adds a managed, multithreaded, streaming index.
- The high-level crate exposes an [`Injector`](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/src/lib.rs#L59-L86), [`restart`](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/src/lib.rs#L365-L393), [`tick`](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/src/lib.rs#L395-L430), and snapshots.
- It uses fzf’s scoring model but a more faithful two-matrix Smith–Waterman implementation, according to its [README](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/README.md#nucleo).
- It supports streaming additions while matching runs.

**Unicode**

- Nucleo matches graphemes through a representative code point and returns grapheme-oriented indices; its README contrasts this with fzf and skim operating on Unicode code points.
- Candidate storage has a compact ASCII representation and a Unicode `Box<[char]>` representation: [`Utf32String`](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/matcher/src/utf32_str.rs#L302-L408).
- Pattern configuration exposes case matching and normalization choices.

**Memory**

- ASCII candidates use roughly one payload byte per character in the matching column.
- Non-ASCII candidates use Rust `char` storage—normally four payload bytes per represented grapheme/code point—plus allocation overhead.
- An application typically also retains the original candidate/path object, so path text can be duplicated between application storage and the matching column.
- No primary-source RSS benchmark was found.

**Updates**

- Excellent for additions and pattern changes.
- It is not a complete filesystem database with first-class arbitrary deletion/rename semantics. A simple implementation should rebuild on invalidation, or layer tombstones/generations around indexed items.

**Signals and license**

- MPL-2.0: [workspace manifest](https://github.com/helix-editor/nucleo/blob/8c16d47cdfa9607d3e44df5f81c635c6f43c65ee/Cargo.toml).
- Crates.io snapshot: [`nucleo-matcher`](https://crates.io/crates/nucleo-matcher) 0.3.1, about 3.72M total downloads; [`nucleo`](https://crates.io/crates/nucleo) 0.5.0, about 1.29M.
- Used directly by Helix and Codex.
- Repository activity and popularity: [GitHub API](https://api.github.com/repos/helix-editor/nucleo).

**Verdict:** best warm matcher and best streaming integration for Kit.

---

### `fuzzy-matcher` / skim matcher

**Architecture**

- Provides SkimMatcherV1/V2-style library matching.
- Straightforward per-candidate API without Nucleo’s managed streaming corpus.
- The repository README and source are available at [`skim-rs/fuzzy-matcher`](https://github.com/skim-rs/fuzzy-matcher).

**Performance and Unicode**

- Nucleo’s primary benchmark reports skim/fuzzy-matcher taking about 4–8× longer for the tested Linux-kernel path queries.
- Nucleo’s README says skim’s bonuses and case-insensitivity are ASCII-specific and that skim operates on Unicode code points rather than graphemes.

**Signals**

- MIT.
- [`fuzzy-matcher` on crates.io](https://crates.io/crates/fuzzy-matcher): about 29.9M total downloads, but the latest published version is 0.3.7 from 2020.
- The current GitHub repository is archived: [GitHub API](https://api.github.com/repos/skim-rs/fuzzy-matcher).

**Verdict:** historically popular but weaker on latency, Unicode, and maintenance.

---

### `sublime_fuzzy`

**Architecture and behavior**

- A Rust implementation **based on** Sublime Text’s string search, not Sublime Text’s proprietary implementation.
- Scoring favors word starts, consecutive characters, exact case, and short gaps: [official crate README](https://github.com/Schlechtwetterfront/fuzzy-rs/blob/a0f4092a3afea0671dd2d698542134ef490c4c91/README.md).
- Its source lowercases Unicode strings and includes Unicode tests: [parsing implementation](https://github.com/Schlechtwetterfront/fuzzy-rs/blob/a0f4092a3afea0671dd2d698542134ef490c4c91/src/parsing.rs).

**Signals**

- [`sublime_fuzzy`](https://crates.io/crates/sublime_fuzzy) 0.7.0, about 2.96M total downloads.
- Last release and repository commit were in 2020.
- License is the repository’s included MIT-style [`LICENSE`](https://github.com/Schlechtwetterfront/fuzzy-rs/blob/a0f4092a3afea0671dd2d698542134ef490c4c91/LICENSE).

**Sublime primary-source limitation**

Sublime Text itself is proprietary. I found no public, maintained first-party source for its production fuzzy file finder or a first-party benchmark suitable for this comparison. The Rust crate should therefore be described only as “Sublime-style,” not as the authoritative Sublime implementation.

**Verdict:** good scoring reference, but too stale and insufficiently benchmarked for Kit’s primary engine.

---

### fzf

**Architecture**

- Standalone Go application with streaming input, parallel matching, and an interactive UI.
- Default v2 is its optimal-scoring algorithm; v1 trades quality for lower resource use.
- It has an explicit [`--scheme=path`](https://github.com/junegunn/fzf/blob/f7ae439ff5b2f8298716d39783a546db793b0625/README.md#fuzzy-completion-for-bash-and-zsh) ranking mode.
- Its own completion documentation recommends using `fd` to generate path candidates: [fzf path source example](https://github.com/junegunn/fzf/blob/f7ae439ff5b2f8298716d39783a546db793b0625/README.md#customizing-completion-source-for-paths-and-directories).

**Integration implications**

- Good product and scoring baseline.
- Calling the binary from Kit would add process startup, pipe parsing, cancellation, and lifecycle complexity.
- Embedding its Go implementation in a Rust TUI is unattractive.
- Nucleo intentionally preserves fzf-like scores while offering a Rust library API and better measured warm throughput.

**Signals**

- MIT: [license](https://github.com/junegunn/fzf/blob/f7ae439ff5b2f8298716d39783a546db793b0625/LICENSE).
- About 82.7k GitHub stars in the observed API snapshot: [GitHub API](https://api.github.com/repos/junegunn/fzf).
- Actively maintained.

**Verdict:** benchmark/ranking reference, not the best integration dependency.

---

### skim

**Architecture**

- Rust interactive fuzzy finder with a reusable library API.
- Uses reader, matcher, and UI stages connected through channels; current source includes a million-item ingestion/matching benchmark harness: [benchmark source](https://github.com/skim-rs/skim/blob/4b962af9ede6f3fdda608bdce8031c352a4effb4/benches/cli.rs).

**Signals**

- MIT.
- [`skim`](https://crates.io/crates/skim) approximately 1.95M total downloads.
- Current repository is active: [GitHub API](https://api.github.com/repos/skim-rs/skim).

**Verdict:** viable if Kit wanted a full picker framework, but Kit already has a Ratatui editor/UI and Nucleo is a better isolated matching core.

---

## 3. Filesystem walking and indexing

| Option | Parallel | Ignore semantics | Incremental/watcher | Integration assessment |
|---|---:|---|---|---|
| [`ignore`](https://docs.rs/ignore/latest/ignore/) | Yes, `build_parallel` | `.gitignore`, `.git/info/exclude`, global gitignore, `.ignore`, custom ignores, hidden filtering, overrides | No watcher; stream each walk | **Best choice** |
| [`walkdir`](https://docs.rs/walkdir/latest/walkdir/) | No built-in parallel walker | No gitignore semantics | No | Small/simple, but Kit would need to build both ignore handling and parallelism |
| [`jwalk`](https://docs.rs/jwalk/latest/jwalk/) | Yes, Rayon | No built-in gitignore semantics | No | Fast traversal, but reimplementing correct ignore precedence is not worthwhile |
| [`fd`](https://github.com/sharkdp/fd) | Yes | Uses `ignore`; hidden and ignored entries excluded by default | No persistent index | Good external candidate generator; process-per-query is wrong |
| [`ripgrep` / `rg --files`](https://github.com/BurntSushi/ripgrep) | Yes | Authoritative `ignore` implementation | No persistent index | Fine fallback executable, but use its `ignore` crate directly in Kit |

### `ignore` details

Ripgrep states that it uses:

- a `RegexSet` to test a path against multiple ignore globs together, and
- a lock-free parallel recursive iterator built with crossbeam.

Source: [ripgrep performance explanation](https://github.com/BurntSushi/ripgrep/blob/3fce3b5bb0236da2df6d99672afb8a719642eca7/README.md#is-it-really-faster-than-everything-else).

`WalkBuilder` exposes controls for:

- hidden paths,
- parent ignore discovery,
- `.ignore`,
- `.gitignore`,
- git global ignores,
- `.git/info/exclude`,
- links,
- filesystem boundaries,
- custom ignore files,
- thread count.

Source: [`WalkBuilder` implementation](https://github.com/BurntSushi/ripgrep/blob/3fce3b5bb0236da2df6d99672afb8a719642eca7/crates/ignore/src/walk.rs).

Popularity snapshot:

- [`ignore`](https://crates.io/crates/ignore): about 165M total downloads.
- [`walkdir`](https://crates.io/crates/walkdir): about 591M.
- [`jwalk`](https://crates.io/crates/jwalk): about 11.3M.

`walkdir`’s download count reflects its broad general-purpose use, not suitability for git-aware completion.

### Ignore-policy recommendation for Kit

Use Codex’s explicit policy as the starting point:

- `.hidden(false)` so paths such as `.github/workflows/...` remain selectable.
- `require_git(true)` so parent `.gitignore` files above the repository do not unexpectedly suppress an entire project.
- Respect `.gitignore`, global gitignore, `.git/info/exclude`, and `.ignore`.
- Exclude `.git/` and likely Kit-generated/runtime directories explicitly.
- Do not follow symlinks by default in Kit unless there is a clear product requirement; Codex currently follows them, but that creates cycle, duplicate, and escape-from-root concerns.

Codex’s rationale and exact builder configuration are in [`walker_worker`](https://github.com/openai/codex/blob/a9519cbcdd2d664530edb2469224ee03c1056799/codex-rs/file-search/src/lib.rs#L389-L455).

---

## 4. Editor implementations

### VS Code

**Architecture**

- Quick Open uses a workspace file-search service and then fuzzy-scores label/path data.
- File search supports caches and emits telemetry distinguishing cached lookup/filter time and entry count: [search service cache metrics](https://github.com/microsoft/vscode/blob/718038e170df9c66a15087cebda424d9c7f051ff/src/vs/workbench/services/search/common/searchService.ts#L370-L408).
- The fuzzy scorer caches results by item/query and separately weights labels/descriptions: [`scoreItemFuzzy`](https://github.com/microsoft/vscode/blob/718038e170df9c66a15087cebda424d9c7f051ff/src/vs/base/common/fuzzyScorer.ts#L396-L424).

**Matching and memory**

- The core scorer builds dynamic-programming `scores` and `matches` arrays over query × target: [`doScoreFuzzy`](https://github.com/microsoft/vscode/blob/718038e170df9c66a15087cebda424d9c7f051ff/src/vs/base/common/fuzzyScorer.ts#L35-L120).
- JavaScript strings are UTF-16; matching lowercases the target and indexes code units, not Unicode grapheme clusters.
- Persistent score caching improves repeated comparisons but can increase memory with query/item combinations.

**Verdict for Kit:** useful lessons—cache the corpus, separate basename/path ranking, cap results—but do not port the TypeScript DP scorer.

---

### Zed

**Architecture**

- Zed maintains worktree entries as part of its project model, then runs fuzzy path matching against those in-memory candidates.
- Its path matcher accepts structured `PathMatchCandidate` values rather than rescanning disk per query: [fuzzy path implementation](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/fuzzy/src/paths.rs).
- The file finder observes project/worktree changes and recomputes candidates/results: [file finder implementation](https://github.com/zed-industries/zed/blob/399258feeaf90ad8a3a208c99221ee87b6452f38/crates/file_finder/src/file_finder.rs).
- Its fuzzy implementation is custom rather than Nucleo.

**Verdict for Kit:** best long-term architecture if Kit eventually gains a general workspace model. It is too large a model to introduce solely for `@` completion.

---

### Helix

**Architecture**

- Helix’s picker uses Nucleo.
- The file picker runs `ignore::WalkBuilder::build_parallel`, configures hidden/parent/git ignore/link/depth behavior, and pushes entries into the picker injector as they arrive: [file picker walk](https://github.com/helix-editor/helix/blob/079a789e8cb08ead67f19e1971a1b7438b37354b/helix-term/src/commands.rs#L2638-L2695).
- The picker consumes Nucleo snapshots while the corpus is still being populated: [picker implementation](https://github.com/helix-editor/helix/blob/079a789e8cb08ead67f19e1971a1b7438b37354b/helix-term/src/ui/picker.rs).

**Updates**

- Streaming initial scan, but no durable watcher-backed file index for each picker invocation.

**Verdict for Kit:** very close to the desired UI/runtime pattern.

---

## 5. Public coding-agent implementations

### Cursor — product behavior is public; implementation is not

Cursor's first-party documentation describes `@Files` as adding a whole file to context, but the research found no public Cursor editor source or first-party architecture/benchmark disclosure for candidate discovery, indexing, ranking, ignore handling, watchers, or memory. Accordingly, this report does not infer Cursor's implementation from its VS Code ancestry or from product behavior. Source: [Cursor `@Files` documentation](https://cursor.com/docs/context/mentions).

### OpenAI Codex — closest direct blueprint

Codex has a dedicated, public Rust file-search crate:

- [README](https://github.com/openai/codex/blob/a9519cbcdd2d664530edb2469224ee03c1056799/codex-rs/file-search/README.md)
- [implementation](https://github.com/openai/codex/blob/a9519cbcdd2d664530edb2469224ee03c1056799/codex-rs/file-search/src/lib.rs)
- [TUI manager](https://github.com/openai/codex/blob/a9519cbcdd2d664530edb2469224ee03c1056799/codex-rs/tui/src/file_search.rs)

Key properties:

- `ignore::WalkBuilder` + Nucleo.
- A session owns one corpus for the current root.
- Every `@token` change updates the existing query rather than rewalking.
- Partial snapshots include:
  - matches,
  - total match count,
  - scanned file count,
  - whether the walk is complete.
- Default top-N is 20 and default walker thread count is 2.
- Result updates are debounced/ticked while the walk continues.
- It stores entry type from the walker to avoid restatting results.
- The TUI currently drops the session when the query becomes empty and recreates it later.

The last behavior is the main opportunity for Kit to improve: retain the session for the TUI/workspace lifetime or at least an idle TTL.

Codex converts relative paths through `Path::to_str()`, so non-UTF-8 Unix paths are omitted: [`get_file_path`](https://github.com/openai/codex/blob/a9519cbcdd2d664530edb2469224ee03c1056799/codex-rs/file-search/src/lib.rs#L377-L387). Kit should make that policy explicit rather than inherit it accidentally.

---

### Gemini CLI

Gemini’s current implementation is more feature-rich but heavier:

- Crawls the project into an `allFiles: Set<string>`.
- Uses JavaScript [`AsyncFzf`](https://www.npmjs.com/package/fzf).
- Maintains a prefix-aware in-memory result cache.
- Has an optional filesystem watcher that updates the candidate set.
- Applies custom tie-breakers for shorter paths, basename-prefix matches, and matches near the end of a path.

Sources:

- [file search implementation](https://github.com/google-gemini/gemini-cli/blob/0bd1d439751478771c45d3d0895a6a9760554bf4/packages/core/src/utils/filesearch/fileSearch.ts)
- [result cache](https://github.com/google-gemini/gemini-cli/blob/0bd1d439751478771c45d3d0895a6a9760554bf4/packages/core/src/utils/filesearch/result-cache.ts)
- [watcher](https://github.com/google-gemini/gemini-cli/blob/0bd1d439751478771c45d3d0895a6a9760554bf4/packages/core/src/utils/filesearch/fileWatcher.ts)
- [`@` command processing](https://github.com/google-gemini/gemini-cli/blob/0bd1d439751478771c45d3d0895a6a9760554bf4/packages/core/src/utils/atCommandUtils.ts)

It defaults to a configurable maximum—currently 20,000 in the constructor path shown in source—so very large projects can return a deliberately incomplete corpus.

**Lesson for Kit:** watcher and tie-break architecture are useful; per-prefix result arrays can consume substantial memory and are unnecessary if Nucleo’s warm latency is already adequate.

---

### OpenCode

OpenCode’s TUI recognizes `@` mentions in its autocomplete component and now delegates file ordering to its file-finder service (“fff”), whose returned order already includes frecency, fuzzy score, and filename bonuses:

- [autocomplete implementation](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/tui/src/component/prompt/autocomplete.tsx)

It also uses `fuzzysort` for other autocomplete resources. This is a useful product-ranking reference—especially frecency—but less directly reusable for Kit than Codex’s self-contained Rust crate.

---

### Continue CLI

Continue’s CLI has a full in-memory file index:

- Walks with `fdir`.
- Uses a fixed ignore-pattern list rather than complete gitignore semantics.
- Chooses maximum depth 10 in Git repositories and 3 otherwise.
- Abandons automatic indexing after a one-second timeout.
- Builds a JavaScript `AsyncFzf` instance.
- Explicitly has **no watcher or subscription**; refresh is a full rebuild.

Source: [FileIndexService](https://github.com/continuedev/continue/blob/5522c6f44ca0ac3528b37244818fbfa39b5af470/extensions/cli/src/services/FileIndexService.ts).

**Lesson for Kit:** the one-second guard avoids UI hangs, but fixed depth and ad hoc ignores can make lookup incomplete in ways that are hard to explain.

---

### Aider

Aider’s public implementation is completion-oriented rather than a dedicated `@file` fuzzy index:

- Builds completion candidates from repository-relative files.
- Caches command completion lists.
- Uses substring or prefix filtering and `prompt_toolkit.PathCompleter` for path-like commands.
- Rejects gitignored files unless configured otherwise.

Sources:

- [command completion](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/commands.py#L258-L275)
- [path/read-only completion](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/commands.py#L702-L759)
- [interactive completer](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/io.py#L91-L225)

**Verdict:** not a latency/quality model for Kit’s proposed finder.

---

## 6. Maintenance, popularity, and licenses

Observed through the projects’ GitHub APIs and crates.io APIs at research time:

| Project | Approximate signal | Activity | License |
|---|---:|---|---|
| [Nucleo](https://api.github.com/repos/helix-editor/nucleo) | 1.5k GitHub stars; `nucleo-matcher` ~3.72M crate downloads | Active | MPL-2.0 |
| [ignore/ripgrep](https://api.github.com/repos/BurntSushi/ripgrep) | ripgrep ~67.7k stars; `ignore` ~165M downloads | Very active | `ignore`: MIT OR Unlicense |
| [fd](https://api.github.com/repos/sharkdp/fd) | ~44.3k stars | Very active | MIT OR Apache-2.0 |
| [fzf](https://api.github.com/repos/junegunn/fzf) | ~82.7k stars | Very active | MIT |
| [skim](https://api.github.com/repos/skim-rs/skim) | ~6.9k stars | Active | MIT |
| [fuzzy-matcher](https://api.github.com/repos/skim-rs/fuzzy-matcher) | ~29.9M crate downloads | Archived/stale despite high historical use | MIT |
| [sublime_fuzzy](https://crates.io/api/v1/crates/sublime_fuzzy) | ~2.96M crate downloads | Last release 2020 | MIT-style repository license |
| [VS Code](https://api.github.com/repos/microsoft/vscode) | ~190k stars | Very active | MIT |
| [Zed](https://api.github.com/repos/zed-industries/zed) | ~89.5k stars | Very active | Mixed repository licensing; inspect crate-specific terms before copying code |
| [Helix](https://api.github.com/repos/helix-editor/helix) | ~46k stars | Very active | MPL-2.0 |
| [Codex](https://api.github.com/repos/openai/codex) | ~120k stars | Very active | Apache-2.0 |
| [Gemini CLI](https://api.github.com/repos/google-gemini/gemini-cli) | ~107k stars | Very active | Apache-2.0 |
| [OpenCode](https://api.github.com/repos/anomalyco/opencode) | ~203k stars | Very active | MIT |
| [Continue](https://api.github.com/repos/continuedev/continue) | ~35.7k stars | Very active | Apache-2.0 |
| [Aider](https://api.github.com/repos/Aider-AI/aider) | ~48.6k stars | Active | Apache-2.0 |

Popularity is not a performance result. In particular, `fuzzy-matcher` and `sublime_fuzzy` have large cumulative download counts but stale implementations.

---

## 7. Kit repository observations

Repository state inspected at Kit commit `e9c159ac0353a21ac3a9cb578af9a26905781529`. No production files were edited.

### Current integration points

- The prompt editor is a compact, custom `String` plus byte cursor in [`src/tui/editor.rs`](https://github.com/speakeasy-api/kit/blob/e9c159ac0353a21ac3a9cb578af9a26905781529/src/tui/editor.rs).
- Character insertion currently flows through [`App::handle_key`](https://github.com/speakeasy-api/kit/blob/e9c159ac0353a21ac3a9cb578af9a26905781529/src/tui/app.rs#L2409-L2643), with ordinary characters inserted near line 2637.
- The outer TUI loop turns events into `Action` values and handles them in [`src/tui/mod.rs`](https://github.com/speakeasy-api/kit/blob/e9c159ac0353a21ac3a9cb578af9a26905781529/src/tui/mod.rs#L563-L865).
- Prompt rendering is centralized in [`draw_prompt_editor`](https://github.com/speakeasy-api/kit/blob/e9c159ac0353a21ac3a9cb578af9a26905781529/src/tui/ui.rs#L1669).
- `App` already owns a canonical workspace `root`, making one finder session per `App` natural.
- Kit already depends on Tokio and has an event-driven TUI, but it does not currently depend directly on `ignore`, Nucleo, Rayon, Crossbeam, or `notify`.
- Kit already depends on `unicode-segmentation` and `unicode-width`. The editor cursor is byte-based and character-boundary safe, but `@` token parsing must be careful not to confuse byte offsets, character positions, and display columns.

### Minimal integration shape

A low-churn design would add:

- `FileFinder` state owned by `App` or the TUI runtime:
  - root,
  - generation,
  - current query/token range,
  - latest top-N,
  - walk progress,
  - selected row,
  - cancellation/shutdown handle.
- A small event/action vocabulary:
  - `StartOrUpdateFileSearch { query, token_range }`
  - `FileSearchSnapshot`
  - `DismissFileSearch`
  - `AcceptFileSearch`
- Token parsing after each editor mutation, not only after `Char` events, because paste, deletion, history recall, cursor movement, and programmatic insertions can all change the active token.
- A popup drawn adjacent to or above `draw_prompt_editor`.
- Acceptance that replaces exactly the active token range, rather than appending blindly.
- Unit tests in the existing inline TUI/editor test style.

Do not put filesystem traversal directly in `Editor`; it should remain a pure text-editing abstraction.

### Suggested lifecycle

1. On the first non-empty active `@token`, create a finder session and start the walk.
2. Stream snapshots immediately; show “scanning” and scanned count without blocking input.
3. On subsequent keystrokes, update only the pattern.
4. On dismissal, hide the popup but retain the corpus.
5. On root/session change, shut down and discard the corpus.
6. On explicit refresh, overflow/error, or watcher invalidation, start a new generation and atomically replace old results.
7. Ignore late snapshots whose generation or query no longer matches.

### Memory policy

For each candidate, retain:

- original `PathBuf`,
- one normalized workspace-relative display string,
- Nucleo’s matching column,
- optionally compact entry flags.

Avoid:

- per-query full result vectors cached indefinitely,
- absolute and relative string copies simultaneously,
- computing match highlight indices for every candidate,
- storing file metadata not needed for rendering/acceptance.

Compute indices only for the visible top-N. Codex exposes this as a `compute_indices` option for the same reason.

### Documentation/research convention

- The repository has user documentation under `docs/user/`.
- I found no existing ADR or research-notes directory and no established research-brief file convention.
- This report therefore uses `docs/research/`; if implemented later, user-facing behavior belongs naturally in `docs/user/tui-and-sessions.md`.

---

## 8. Concrete option ranking

### Option A — recommended

**`ignore` + high-level `nucleo`, persistent per workspace**

- Best warm performance evidence.
- Fully in-process.
- Streams cold results.
- Good Unicode/grapheme behavior.
- Correct ignore machinery.
- Closely validated by Codex and Helix.
- Main costs: MPL-2.0 dependency review, corpus memory, and rebuild strategy for deletions.

### Option B

**`ignore` + `nucleo-matcher`, custom top-N parallel scorer**

- More control and potentially less retained index memory.
- Consider only if high-level Nucleo memory or update semantics become a measured problem.
- More scheduling, cancellation, sorting, and incremental-work code for Kit to own.

### Option C

**Spawn `fd` or `rg --files` once, feed output into Nucleo**

- Fast prototype and useful fallback.
- Still separate cold scan from warm matching if spawned only once.
- Adds executable discovery/version/platform behavior, process cancellation, output encoding, and IPC.
- Inferior to linking `ignore` for a bundled Rust application.

### Option D

**Watch-backed durable workspace index**

- Zed/Gemini-like freshness and near-zero repeated cold scans.
- Adds `notify`-style platform behavior, rename coalescing, ignore-file invalidation, overflow recovery, and race handling.
- Defer until profiling shows index rebuilds are material.

### Options not recommended

- Spawn `fzf`, `skim`, `fd`, or `rg` on every keystroke.
- Use `walkdir` and reimplement gitignore precedence.
- Adopt stale `fuzzy-matcher` or `sublime_fuzzy` solely because of cumulative downloads.
- Cache complete result vectors for every typed prefix before measuring whether Nucleo needs such a cache.

---

## Bottom line

The strongest primary-source-supported design is:

> **Codex’s session model + Helix’s streaming picker + a longer-lived corpus:** parallel `ignore` walk, streamed into Nucleo, partial cold results, Nucleo-only warm queries, top-N snapshots, explicit generation/cancellation, and conservative rebuilds.

That gives Kit a quick first result during cold startup and single-digit-to-low-double-digit millisecond warm matching on the only directly relevant published Rust benchmark, without importing a second TUI framework or relying on subprocesses.
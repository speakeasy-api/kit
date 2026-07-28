# Edit validation transaction boundary

`workspace::edit::validate::validate` accepts a complete canonical `EditIr`; it does not parse or apply a partial stream. It derives the requested `RevisionId`, acquires the exclusive managed-workspace mutation guard under the request deadline, and only then reconciles the revision, epoch, and workspace digest. A stale request therefore fails after lock contention rather than validating against pre-lock state.

Every source and destination passes through the guard-bound `PathAuthorizer`. Existing files are regular, single-link files reached by exact no-follow descriptor traversal. Destinations are authorized absent names under retained parent descriptors. Case or normalization aliases, symlinks, hardlinks, special files, mount crossings, private paths, and unsafe lexical paths fail closed. Move destinations must be absent and the canonical IR must already have a cycle-free move graph.

Validation reads each existing source from its authorized descriptor and verifies its exact base digest, type, identity, and mode. `ReplaceRange` files must be non-binary UTF-8 with coherent LF or CRLF semantics. Every byte range is in bounds and on UTF-8 boundaries, expected bytes match exactly, non-empty anchors occur exactly once, ranges do not overlap, and final-newline semantics match. All ranges for one file are applied in sorted order to an in-memory result only.

The eight edge classes have stable safe outcomes:

| Edge class | Typed outcome |
| --- | --- |
| duplicate exact anchor | `AmbiguousAnchor` |
| stale request, including stale after lock wait | `StaleRevision` |
| concurrent external edit during or after a read | `ExternalEdit` or `StaleRevision` |
| invalid UTF-8 or a range splitting a Unicode scalar | `InvalidUnicode` |
| CRLF/LF mismatch, mixed LF/CRLF, or bare CR | `NewlineMismatch` |
| missing or unexpected final newline | `FinalNewlineMismatch` |
| NUL-bearing content | `BinaryFile` |
| symlink, special file, hardlink, case/normalization alias, mount, private path, or lexical escape | `UnsafePath` |

Other exact mismatches use `AnchorMismatch`, `BaseDigestMismatch`, `RangeOutsideFile`, or `PathStateMismatch`. Resource failures use `LimitExceeded`; unsupported safe primitives use `Unavailable`. Public errors contain only fixed text, bounded root-relative IR paths, and closed enums. They do not include raw I/O diagnostics or untrusted filesystem names.

The result is a deterministic `ValidatedTransaction` containing ordered effects, before identities/digests/modes, resulting paths/content/digests/modes, and a sorted changed-file set. Its digest excludes the random guard nonce but includes the reconciled revision, epoch, workspace digest, filesystem identities, and all planned results. Validation never creates, writes, renames, chmods, or removes a filesystem entry.

The transaction owns its mutation guard and opaque accepted descriptor capabilities. The transaction cannot be serialized or forged, and neither the guard nor capabilities can be extracted through the public API. The [staging layer](edit-staging.md) consumes the plan only under the same guard nonce and revision; dropping the plan releases all authority.

`EditLimits::max_validation_read_bytes`, `max_validation_memory_bytes`, and `max_validation_time` bound aggregate reads, retained/generated content, and the whole operation starting before lock acquisition. Parsing and IR construction remain bounded by the input, operation, path, and content limits in `EditLimits`. Reads and hashing are streaming or bounded; no unchecked full-file allocation is used.

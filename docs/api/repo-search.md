# Repository lexical search

Lexical search is bound to one metadata-index revision and one end-to-end deadline. The deadline covers initial revision validation, matching, ranking, response construction, and final revision validation.

`max_result_bytes` bounds the exact compact JSON serialization returned by `SearchResponse::to_canonical_json`, including the response envelope, counters, cursor, punctuation, and JSON escaping. Candidate and final response sizes are measured with the same serializer through a non-allocating counting writer; a bound too small for response metadata is invalid.

A cursor is returned only when a complete index and scan can resume at a strictly advancing deterministic ranked-match frontier. File-count, scanned-byte, time, index, excerpt, or individually unrepresentable-match truncation is non-resumable and returns no cursor. Following cursors therefore terminates without unchanged empty pages, duplicates, or omissions within `max_cursor_offset`.

Metadata indexing applies ordered root and nested `.gitignore` rules. Unescaped trailing spaces are stripped, escaped spaces are literal, ignored directories cannot be re-included by a descendant rule, and all ignore compilation and matching allocations, rules, bytes, components, workspace, and time are bounded.

`discover` bounds query/filter collections, scanned entries and bytes, ranking work, retained candidate bytes, per-path diversity, output bytes, cursor frontier, and one end-to-end deadline. Diversity is enforced while candidates are retained, so repeated high-scoring records for one path cannot evict lower-ranked paths from later pages. Omission counts include candidates dropped by rank, diversity, or representation bounds; a cursor is emitted only for a complete, resumable ranked frontier.

Focused reads use the revision manager's bounded byte/line stream and never retain an entire large file for a small range. UTF-8 classification applies to the returned fragment, so a byte range that splits a code point is binary. Required binary, large, and full-log artifacts fail with a typed size error when the exact envelope exceeds its bound.

Workspace artifacts are staged privately, serialized into the response, and revision-validated before atomic promotion. Their opaque handle identifies the envelope rather than the payload digest and binds principal, project, workspace epoch, revision, canonical path, range, media types, retention, and payload digest. Resolution requires the authorization context, streams verification under caller caps, returns only payload bytes, and uses one generic denial for malformed, tampered, cross-principal, and cross-project handles.

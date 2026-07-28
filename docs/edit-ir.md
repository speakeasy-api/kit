# Canonical edit IR v1

The public `EditIr` and `CanonicalOperation` types are serialization-only. Decoders use private wire types, parse paths with the request's active `EditLimits`, and rebuild every request through `EditIr::new`. Canonical decoding rejects supplied operation IDs or order values that differ from the rebuilt values.

The v1 operation vocabulary is exactly `add_file`, `delete_file`, `move_file`, and `replace_range`. Operations against the same path must have compatible base digests, disjoint ordered ranges, and one executable-mode outcome. Adds, deletes, moves, move cycles, and range edits cannot claim the same path incompatibly.

Root-relative paths use forward slashes and NFC Unicode. They reject absolute paths, drive paths, empty, `.` or `..` components, backslashes, controls, Windows alternate-data-stream punctuation, device names, reserved characters, and components ending in a dot or space on every platform.

JSON and unified-diff normalization enforce operation, path, decoded-content, rendered-content, and input limits. Parsers check limits before retained copies or rendering and use fallible allocation. A larger custom path limit remains active through structured and canonical wire decoding.

Unified-diff hunks consume their declared old and new line counts before another hunk or file section can begin. Old and new coordinates must form consistent forward sequences. A `\ No newline at end of file` marker applies only to the final line of each projection in which its preceding line participates. Git, file-header, and rename paths must agree. Mode-only sections produce an empty `replace_range` carrying the mode outcome; incomplete or conflicting mode metadata is rejected.

`GIT binary patch` and `Binary files <old> and <new> differ` are binary metadata only when they appear as exact lines outside a hunk. The same text prefixed by a hunk line marker is ordinary text content. Binary patches remain unsupported.

The optional grammar-constrained provider path uses this same structured input and normalizer; it does not decode canonical operation IDs and cannot construct a validated plan. See `docs/operations/grammar-edit-output.md`.

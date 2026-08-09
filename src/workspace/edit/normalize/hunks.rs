//! DR-0008 hunk-anchored edit input (kit_edit input v2).
//!
//! Operations are `{path, hunks: [{context_before, old, new, context_after}]}`
//! over exact UTF-8 text lines. Resolution finds the unique occurrence of
//! `context_before + old + context_after` in the current file content and
//! lowers each hunk to the existing `ReplaceRange` edit IR (byte ranges and
//! base digests computed here), so validate/syntax/stage/materialize consume
//! the same replacement plan as before.
//!
//! Newline normalization: the file's CRLF sequences are normalized to LF for
//! matching (mixed newlines and bare CR are rejected, as everywhere in the
//! edit IR); hunk lines must not contain `\r` or `\n` at all. The file's
//! newline flavor and final-newline state are preserved on write.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{BaseFile, NormalizationContext, NormalizeError};
use crate::workspace::edit::ir::{
    ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, IrError, Newline,
    RootRelativePath, TextContent, preflight_json,
};

pub const HUNK_EDIT_VERSION: u32 = 2;
pub const MAX_HUNKS_PER_OPERATION: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HunkEnvelope {
    version: u32,
    operations: Vec<HunkOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum HunkOperation {
    Edit { path: String, hunks: Vec<Hunk> },
    AddFile {
        path: String,
        content: String,
        executable: bool,
    },
    DeleteFile { path: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hunk {
    context_before: Vec<String>,
    old: Vec<String>,
    new: Vec<String>,
    context_after: Vec<String>,
}

impl HunkEnvelope {
    /// Root-relative paths whose current content resolution needs (edit and
    /// delete targets), deduplicated by filesystem identity.
    pub(crate) fn base_paths(
        &self,
        limits: EditLimits,
    ) -> Result<Vec<RootRelativePath>, NormalizeError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut paths = Vec::new();
        for operation in &self.operations {
            let path = match operation {
                HunkOperation::Edit { path, .. } | HunkOperation::DeleteFile { path } => path,
                HunkOperation::AddFile { .. } => continue,
            };
            let path = RootRelativePath::parse(path.clone(), limits.max_path_bytes)?;
            if seen.insert(crate::workspace::edit::ir::identity_key(
                &path,
                limits.identity_policy,
            )) {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

/// The JSON Schema for the v2 hunk envelope (without the preview_token arm).
pub(crate) fn hunk_edit_schema(limits: EditLimits) -> Value {
    let line = json!({"pattern": "^[^\\n\\r]*$", "type": "string"});
    let lines = |description: &str| {
        json!({
            "description": description,
            "items": line,
            "type": "array"
        })
    };
    let path = json!({"maxLength": limits.max_path_bytes, "minLength": 1, "type": "string"});
    json!({
        "additionalProperties": false,
        "properties": {
            "operations": {
                "items": {"oneOf": [
                    {
                        "additionalProperties": false,
                        "properties": {
                            "hunks": {
                                "items": {
                                    "additionalProperties": false,
                                    "properties": {
                                        "context_before": lines("Exact lines immediately before old."),
                                        "context_after": lines("Exact lines immediately after old."),
                                        "new": lines("Replacement lines; empty deletes old."),
                                        "old": lines("Exact lines to replace; empty inserts new between the contexts.")
                                    },
                                    "required": ["context_before", "old", "new", "context_after"],
                                    "type": "object"
                                },
                                "maxItems": MAX_HUNKS_PER_OPERATION,
                                "minItems": 1,
                                "type": "array"
                            },
                            "op": {"const": "edit"},
                            "path": path
                        },
                        "required": ["op", "path", "hunks"],
                        "type": "object"
                    },
                    {
                        "additionalProperties": false,
                        "properties": {
                            "content": {"type": "string"},
                            "executable": {"type": "boolean"},
                            "op": {"const": "add_file"},
                            "path": path
                        },
                        "required": ["op", "path", "content", "executable"],
                        "type": "object"
                    },
                    {
                        "additionalProperties": false,
                        "properties": {"op": {"const": "delete_file"}, "path": path},
                        "required": ["op", "path"],
                        "type": "object"
                    }
                ]},
                "maxItems": limits.max_operations,
                "type": "array"
            },
            "version": {"const": HUNK_EDIT_VERSION}
        },
        "required": ["version", "operations"],
        "type": "object"
    })
}

pub(crate) fn parse(input: &[u8], limits: EditLimits) -> Result<HunkEnvelope, NormalizeError> {
    if input.len() > limits.max_input_bytes {
        return Err(NormalizeError::InputLimit {
            actual: input.len(),
            limit: limits.max_input_bytes,
        });
    }
    preflight_json(input, limits)?;
    let envelope: HunkEnvelope = serde_json::from_slice(input)
        .map_err(|error| NormalizeError::MalformedJson(error.to_string()))?;
    if envelope.version != HUNK_EDIT_VERSION {
        return Err(NormalizeError::UnsupportedVersion(envelope.version));
    }
    if envelope.operations.len() > limits.max_operations {
        return Err(IrError::OperationLimit {
            actual: envelope.operations.len(),
            limit: limits.max_operations,
        }
        .into());
    }
    for operation in &envelope.operations {
        let HunkOperation::Edit { path, hunks } = operation else {
            continue;
        };
        if hunks.is_empty() || hunks.len() > MAX_HUNKS_PER_OPERATION {
            return Err(NormalizeError::MalformedJson(format!(
                "edit operation for {path} must carry between 1 and \
                 {MAX_HUNKS_PER_OPERATION} hunks"
            )));
        }
        for hunk in hunks {
            for line in hunk
                .context_before
                .iter()
                .chain(&hunk.old)
                .chain(&hunk.new)
                .chain(&hunk.context_after)
            {
                if line.contains(['\n', '\r', '\0']) {
                    return Err(NormalizeError::MalformedJson(format!(
                        "hunk lines for {path} must be single lines without \
                         newline or NUL characters"
                    )));
                }
            }
            if hunk.old.is_empty() && hunk.new.is_empty() {
                return Err(NormalizeError::MalformedJson(format!(
                    "a hunk for {path} has empty old and new: it changes nothing"
                )));
            }
        }
    }
    Ok(envelope)
}

/// Lower a parsed hunk envelope into the existing edit IR against the base
/// files recorded in `context`. Every edit/delete target must have been
/// inserted into the context by the caller (from the run's current workspace
/// snapshot); a missing entry is reported as an outdated view.
pub(crate) fn lower(
    envelope: &HunkEnvelope,
    context: &NormalizationContext,
) -> Result<EditIr, NormalizeError> {
    let limits = context.limits();
    let mut operations = Vec::new();
    operations
        .try_reserve(envelope.operations.len())
        .map_err(|_| IrError::Allocation)?;
    for operation in &envelope.operations {
        match operation {
            HunkOperation::AddFile {
                path,
                content,
                executable,
            } => {
                let path = RootRelativePath::parse(path.clone(), limits.max_path_bytes)?;
                operations.push(EditOperation::AddFile {
                    path,
                    content: TextContent::from_bytes(content.as_bytes())?,
                    executable: *executable,
                });
            }
            HunkOperation::DeleteFile { path } => {
                let parsed = RootRelativePath::parse(path.clone(), limits.max_path_bytes)?;
                let base = context
                    .file(&parsed)
                    .ok_or_else(|| NormalizeError::BaseFileMissing(path.clone()))?;
                operations.push(EditOperation::DeleteFile {
                    path: parsed,
                    base_digest: base.digest().clone(),
                });
            }
            HunkOperation::Edit { path, hunks } => {
                let parsed = RootRelativePath::parse(path.clone(), limits.max_path_bytes)?;
                let base = context
                    .file(&parsed)
                    .ok_or_else(|| NormalizeError::BaseFileMissing(path.clone()))?;
                let file = FileLines::new(base);
                for (index, hunk) in hunks.iter().enumerate() {
                    let (start, end) = resolve_hunk(&file, hunk, path, index)?;
                    let (range, expected, replacement) =
                        lower_region(&file, start, end, &hunk.new)?;
                    operations.push(EditOperation::ReplaceRange {
                        path: parsed.clone(),
                        base_digest: base.digest().clone(),
                        range,
                        expected,
                        replacement,
                        executable: ExecutableMode::Preserve,
                    });
                }
            }
        }
    }
    EditIr::new(context.expected_revision().clone(), operations, limits).map_err(Into::into)
}

/// A base file decomposed into normalized lines plus the byte offset of each
/// line start in the *rendered* file (original newline flavor).
struct FileLines<'a> {
    lines: Vec<&'a str>,
    /// offsets[k] = rendered byte offset of the start of line k;
    /// offsets[lines.len()] = rendered file length.
    offsets: Vec<usize>,
    newline: Newline,
    final_newline: bool,
}

impl<'a> FileLines<'a> {
    fn new(base: &'a BaseFile) -> Self {
        let content = base.content();
        let text = content.text();
        let final_newline = content.has_final_newline();
        let lines: Vec<&str> = if text.is_empty() && !final_newline {
            Vec::new()
        } else {
            text.split('\n').collect()
        };
        let separator = match content.newline() {
            Newline::Lf => 1,
            Newline::Crlf => 2,
        };
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        let mut offset = 0_usize;
        offsets.push(0);
        for (index, line) in lines.iter().enumerate() {
            offset += line.len();
            if index + 1 < lines.len() || final_newline {
                offset += separator;
            }
            offsets.push(offset);
        }
        debug_assert_eq!(offset, content.rendered_len());
        Self {
            lines,
            offsets,
            newline: content.newline(),
            final_newline,
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Find the unique line region `[start, end)` selected by the hunk.
fn resolve_hunk(
    file: &FileLines<'_>,
    hunk: &Hunk,
    path: &str,
    index: usize,
) -> Result<(usize, usize), NormalizeError> {
    let pattern_len = hunk.context_before.len() + hunk.old.len() + hunk.context_after.len();
    let line_count = file.lines.len();
    if pattern_len == 0 {
        // Anchorless pure insertion: unique only in an empty file.
        return if line_count == 0 {
            Ok((0, 0))
        } else {
            Err(NormalizeError::AnchorAmbiguous {
                path: path.to_owned(),
                hunk: index,
                matches: line_count + 1,
            })
        };
    }
    if pattern_len > line_count {
        return Err(NormalizeError::AnchorNotFound {
            path: path.to_owned(),
            hunk: index,
        });
    }
    let needle = {
        let mut parts = Vec::with_capacity(pattern_len);
        parts.extend(hunk.context_before.iter().map(String::as_str));
        parts.extend(hunk.old.iter().map(String::as_str));
        parts.extend(hunk.context_after.iter().map(String::as_str));
        parts.join("\n")
    };
    let text = file.text();
    // Line-boundary-aligned substring matches; str::find is worst-case linear
    // (two-way algorithm), so total cost stays linear per candidate sweep.
    let mut matches: Vec<usize> = Vec::new();
    let mut from = 0_usize;
    while from <= text.len() {
        let Some(found) = text[from..].find(&needle) else {
            break;
        };
        let start = from + found;
        let end = start + needle.len();
        let starts_line = start == 0 || text.as_bytes()[start - 1] == b'\n';
        let ends_line = end == text.len() || text.as_bytes()[end] == b'\n';
        if starts_line && ends_line {
            let line_index = text[..start].bytes().filter(|byte| *byte == b'\n').count();
            matches.push(line_index);
            if matches.len() > 1 {
                break;
            }
        }
        from = start + 1;
    }
    match matches.as_slice() {
        [] => Err(NormalizeError::AnchorNotFound {
            path: path.to_owned(),
            hunk: index,
        }),
        [line] => {
            let start = line + hunk.context_before.len();
            Ok((start, start + hunk.old.len()))
        }
        _ => Err(NormalizeError::AnchorAmbiguous {
            path: path.to_owned(),
            hunk: index,
            matches: matches.len(),
        }),
    }
}

/// Canonical text content whose rendered bytes are `text` joined by the
/// flavor separator; folds the `…\n` + no-final-newline form into the
/// equivalent final-newline form the IR requires.
fn content(mut text: String, newline: Newline, final_newline: bool) -> Result<TextContent, IrError> {
    if text.is_empty() && !final_newline {
        return Ok(TextContent::empty(newline));
    }
    if !final_newline && text.ends_with('\n') {
        text.pop();
        return TextContent::new(text, newline, true);
    }
    TextContent::new(text, newline, final_newline)
}

/// Lower a resolved line region to a byte range plus expected/replacement
/// content in the file's own newline flavor.
fn lower_region(
    file: &FileLines<'_>,
    start: usize,
    end: usize,
    new: &[String],
) -> Result<(ByteRange, TextContent, TextContent), IrError> {
    let line_count = file.lines.len();
    let total = *file.offsets.last().expect("offsets always holds the total");
    if end > start {
        // Replacement or deletion of existing lines. The region carries its
        // trailing separator except when it ends the file without a final
        // newline (then, like patch(1), a pure deletion leaves the preceding
        // separator in place).
        let trailing_separator = end < line_count || file.final_newline;
        let byte_start = file.offsets[start];
        let byte_end = if end < line_count {
            file.offsets[end]
        } else {
            total
        };
        let expected = content(
            file.lines[start..end].join("\n"),
            file.newline,
            trailing_separator,
        )?;
        let replacement = if new.is_empty() {
            TextContent::empty(file.newline)
        } else {
            content(new.join("\n"), file.newline, trailing_separator)?
        };
        Ok((ByteRange::new(byte_start, byte_end)?, expected, replacement))
    } else {
        // Insertion before line `start` (or at end of file).
        let at_unterminated_end = start == line_count && line_count > 0 && !file.final_newline;
        let byte_position = if start < line_count {
            file.offsets[start]
        } else {
            total
        };
        let replacement = if at_unterminated_end {
            // Append after a last line with no trailing newline: lead with a
            // separator and keep the file unterminated.
            let mut text = String::from("\n");
            text.push_str(&new.join("\n"));
            content(text, file.newline, false)?
        } else {
            content(new.join("\n"), file.newline, true)?
        };
        Ok((
            ByteRange::new(byte_position, byte_position)?,
            TextContent::empty(file.newline),
            replacement,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::edit::ir::RevisionToken;

    fn revision() -> RevisionToken {
        RevisionToken::parse(format!("r:{}", "0".repeat(64))).unwrap()
    }

    fn context_with(files: &[(&str, &[u8])]) -> NormalizationContext {
        let mut context = NormalizationContext::new(revision(), EditLimits::default());
        for (path, bytes) in files {
            context.insert_file(*path, bytes, false).unwrap();
        }
        context
    }

    fn envelope(value: serde_json::Value) -> HunkEnvelope {
        parse(&serde_json::to_vec(&value).unwrap(), EditLimits::default()).unwrap()
    }

    fn apply(base: &[u8], hunks: serde_json::Value) -> Result<Vec<u8>, NormalizeError> {
        let context = context_with(&[("file.txt", base)]);
        let envelope = envelope(serde_json::json!({
            "version": 2,
            "operations": [{"op": "edit", "path": "file.txt", "hunks": hunks}]
        }));
        let ir = lower(&envelope, &context)?;
        // Re-render by applying the lowered ranges to the base bytes.
        let mut ranges = Vec::new();
        for operation in ir.operations() {
            let EditOperation::ReplaceRange {
                range,
                expected,
                replacement,
                ..
            } = operation.operation()
            else {
                panic!("expected replace_range");
            };
            let start = usize::try_from(range.start).unwrap();
            let end = usize::try_from(range.end).unwrap();
            assert_eq!(
                &base[start..end],
                expected.render().as_slice(),
                "expected content must equal the base slice"
            );
            ranges.push((start, end, replacement.render()));
        }
        ranges.sort_by_key(|(start, end, _)| (*start, *end));
        let mut output = Vec::new();
        let mut cursor = 0_usize;
        for (start, end, replacement) in ranges {
            output.extend_from_slice(&base[cursor..start]);
            output.extend_from_slice(&replacement);
            cursor = end;
        }
        output.extend_from_slice(&base[cursor..]);
        Ok(output)
    }

    #[test]
    fn unique_match_replaces() {
        let result = apply(
            b"fn main() {\n    old();\n}\n",
            serde_json::json!([{
                "context_before": ["fn main() {"],
                "old": ["    old();"],
                "new": ["    new();"],
                "context_after": ["}"]
            }]),
        )
        .unwrap();
        assert_eq!(result, b"fn main() {\n    new();\n}\n");
    }

    #[test]
    fn zero_matches_is_anchor_not_found() {
        let error = apply(
            b"alpha\nbeta\n",
            serde_json::json!([{
                "context_before": [],
                "old": ["gamma"],
                "new": ["delta"],
                "context_after": []
            }]),
        )
        .unwrap_err();
        assert!(matches!(error, NormalizeError::AnchorNotFound { hunk: 0, .. }));
    }

    #[test]
    fn multiple_matches_is_anchor_ambiguous() {
        let error = apply(
            b"x\ny\nx\ny\n",
            serde_json::json!([{
                "context_before": [],
                "old": ["x"],
                "new": ["z"],
                "context_after": []
            }]),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NormalizeError::AnchorAmbiguous { matches: 2, .. }
        ));
    }

    #[test]
    fn partial_line_match_does_not_anchor() {
        // "eta" occurs inside "beta" but never as a whole line.
        let error = apply(
            b"alpha\nbeta\n",
            serde_json::json!([{
                "context_before": [],
                "old": ["eta"],
                "new": ["x"],
                "context_after": []
            }]),
        )
        .unwrap_err();
        assert!(matches!(error, NormalizeError::AnchorNotFound { .. }));
    }

    #[test]
    fn insertion_between_context_lines() {
        let result = apply(
            b"one\nthree\n",
            serde_json::json!([{
                "context_before": ["one"],
                "old": [],
                "new": ["two"],
                "context_after": ["three"]
            }]),
        )
        .unwrap();
        assert_eq!(result, b"one\ntwo\nthree\n");
    }

    #[test]
    fn deletion_removes_lines_and_separator() {
        let result = apply(
            b"one\ntwo\nthree\n",
            serde_json::json!([{
                "context_before": ["one"],
                "old": ["two"],
                "new": [],
                "context_after": ["three"]
            }]),
        )
        .unwrap();
        assert_eq!(result, b"one\nthree\n");
    }

    #[test]
    fn multi_hunk_same_file_applies_in_document_order() {
        let result = apply(
            b"a\nb\nc\nd\n",
            serde_json::json!([
                {"context_before": ["c"], "old": ["d"], "new": ["D"], "context_after": []},
                {"context_before": [], "old": ["a"], "new": ["A"], "context_after": ["b"]}
            ]),
        )
        .unwrap();
        assert_eq!(result, b"A\nb\nc\nD\n");
    }

    #[test]
    fn overlapping_hunks_are_rejected() {
        let error = apply(
            b"a\nb\nc\n",
            serde_json::json!([
                {"context_before": ["a"], "old": ["b"], "new": ["x"], "context_after": []},
                {"context_before": [], "old": ["b"], "new": ["y"], "context_after": ["c"]}
            ]),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NormalizeError::Ir(IrError::OverlappingRanges(_))
        ));
    }

    #[test]
    fn crlf_files_match_lf_hunk_lines_and_keep_flavor() {
        let result = apply(
            b"one\r\ntwo\r\n",
            serde_json::json!([{
                "context_before": ["one"],
                "old": ["two"],
                "new": ["deux", "zwei"],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"one\r\ndeux\r\nzwei\r\n");
    }

    #[test]
    fn anchor_at_file_start_with_empty_context_before() {
        let result = apply(
            b"first\nrest\n",
            serde_json::json!([{
                "context_before": [],
                "old": ["first"],
                "new": ["FIRST"],
                "context_after": ["rest"]
            }]),
        )
        .unwrap();
        assert_eq!(result, b"FIRST\nrest\n");
    }

    #[test]
    fn anchor_at_file_end_with_empty_context_after() {
        let result = apply(
            b"head\nlast\n",
            serde_json::json!([{
                "context_before": ["head"],
                "old": ["last"],
                "new": ["LAST"],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"head\nLAST\n");
    }

    #[test]
    fn append_after_unterminated_final_line() {
        let result = apply(
            b"head\ntail",
            serde_json::json!([{
                "context_before": ["tail"],
                "old": [],
                "new": ["more"],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"head\ntail\nmore");
    }

    #[test]
    fn replace_unterminated_final_line() {
        let result = apply(
            b"head\ntail",
            serde_json::json!([{
                "context_before": ["head"],
                "old": ["tail"],
                "new": ["TAIL"],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"head\nTAIL");
    }

    #[test]
    fn delete_unterminated_final_line_keeps_preceding_separator() {
        // patch(1) semantics: the survivor keeps its own newline.
        let result = apply(
            b"head\ntail",
            serde_json::json!([{
                "context_before": ["head"],
                "old": ["tail"],
                "new": [],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"head\n");
    }

    #[test]
    fn insert_into_empty_file_with_empty_anchors() {
        let result = apply(
            b"",
            serde_json::json!([{
                "context_before": [],
                "old": [],
                "new": ["hello"],
                "context_after": []
            }]),
        )
        .unwrap();
        assert_eq!(result, b"hello\n");
    }

    #[test]
    fn empty_anchor_in_nonempty_file_is_ambiguous() {
        let error = apply(
            b"a\n",
            serde_json::json!([{
                "context_before": [],
                "old": [],
                "new": ["x"],
                "context_after": []
            }]),
        )
        .unwrap_err();
        assert!(matches!(error, NormalizeError::AnchorAmbiguous { .. }));
    }

    #[test]
    fn add_and_delete_operations_lower() {
        let context = context_with(&[("gone.txt", b"bye\n")]);
        let envelope = envelope(serde_json::json!({
            "version": 2,
            "operations": [
                {"op": "add_file", "path": "new.txt", "content": "text\n", "executable": false},
                {"op": "delete_file", "path": "gone.txt"}
            ]
        }));
        let ir = lower(&envelope, &context).unwrap();
        assert_eq!(ir.operations().len(), 2);
        assert!(matches!(
            ir.operations()[0].operation(),
            EditOperation::AddFile { .. }
        ));
        let EditOperation::DeleteFile { base_digest, .. } = ir.operations()[1].operation() else {
            panic!("expected delete_file");
        };
        assert_eq!(
            base_digest.to_string(),
            format!("blake3:{}", blake3::hash(b"bye\n").to_hex())
        );
    }

    #[test]
    fn missing_base_file_reports_outdated_view() {
        let context = context_with(&[]);
        let envelope = envelope(serde_json::json!({
            "version": 2,
            "operations": [{"op": "edit", "path": "absent.txt", "hunks": [
                {"context_before": [], "old": ["x"], "new": ["y"], "context_after": []}
            ]}]
        }));
        assert!(matches!(
            lower(&envelope, &context).unwrap_err(),
            NormalizeError::BaseFileMissing(path) if path == "absent.txt"
        ));
    }

    #[test]
    fn old_format_fields_are_rejected() {
        for value in [
            serde_json::json!({"version": 1, "operations": []}),
            serde_json::json!({"version": 2, "expected_revision": format!("r:{}", "0".repeat(64)), "operations": []}),
            serde_json::json!({"version": 2, "operations": [{
                "op": "replace_range", "path": "x",
                "base_digest": format!("blake3:{}", "a".repeat(64)),
                "range": {"start": 0, "end": 0},
                "expected": {"encoding": "utf8", "newline": "lf", "text": "", "final_newline": false},
                "replacement": {"encoding": "utf8", "newline": "lf", "text": "", "final_newline": false},
                "executable": "preserve"
            }]}),
        ] {
            assert!(parse(&serde_json::to_vec(&value).unwrap(), EditLimits::default()).is_err());
        }
    }

    #[test]
    fn noop_hunks_and_multiline_hunk_lines_are_rejected() {
        for hunks in [
            serde_json::json!([{ "context_before": ["a"], "old": [], "new": [], "context_after": [] }]),
            serde_json::json!([{ "context_before": [], "old": ["a\nb"], "new": ["c"], "context_after": [] }]),
        ] {
            let value = serde_json::json!({
                "version": 2,
                "operations": [{"op": "edit", "path": "file.txt", "hunks": hunks}]
            });
            assert!(matches!(
                parse(&serde_json::to_vec(&value).unwrap(), EditLimits::default()).unwrap_err(),
                NormalizeError::MalformedJson(_)
            ));
        }
    }

    #[test]
    fn outdated_view_cannot_anchor() {
        // Hunk built from an older snapshot of the file no longer matches.
        let old = b"value = 1\n";
        let current = b"value = 2\n";
        let error = apply(
            current,
            serde_json::json!([{
                "context_before": [],
                "old": ["value = 1"],
                "new": ["value = 3"],
                "context_after": []
            }]),
        )
        .unwrap_err();
        let _ = old;
        assert!(matches!(error, NormalizeError::AnchorNotFound { .. }));
    }
}

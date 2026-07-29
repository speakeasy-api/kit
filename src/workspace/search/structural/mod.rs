use std::{
    collections::BTreeSet,
    fmt,
    mem::size_of,
    path::{Component, PathBuf},
    time::{Duration, Instant},
};

use ast_grep_core::{
    AstGrep, Pattern, matcher::MatcherExt, replacer::TemplateFix, tree_sitter::StrDoc,
};
use ast_grep_language::Rust;
use serde::{Serialize, ser::SerializeStruct};

use crate::workspace::{
    edit::{
        diff::{ChangeDiffError, render_whole_file_replacement},
        ir::{
            ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, RevisionToken,
            RootRelativePath, TextContent,
        },
        normalize::BaseFile,
    },
    index::meta::{ContentState, MetadataIndex},
    revision::{EntryKind, LimitKind, ManagedWorkspace, RevisionError, RevisionId},
    search::lexical::SkippedFiles,
    syntax::{RUST_GRAMMAR_VERSION, SyntaxError, SyntaxIndex, SyntaxLanguage},
};

pub const AST_GREP_CORE_VERSION: &str = "0.40.1";
pub const AST_GREP_LANGUAGE_VERSION: &str = "0.40.1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralQuery {
    pub pattern: String,
    pub rewrite: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralOptions {
    pub path_prefixes: Vec<PathBuf>,
    pub languages: Vec<String>,
    pub max_pattern_bytes: usize,
    pub max_replacement_bytes: usize,
    pub max_source_bytes: usize,
    pub max_scanned_files: usize,
    pub max_scanned_bytes: u64,
    pub max_matches: usize,
    pub max_capture_bytes: usize,
    pub max_rewrite_bytes: usize,
    pub max_change_diff_bytes: usize,
    pub max_output_bytes: usize,
    pub max_time: Duration,
}

impl Default for StructuralOptions {
    fn default() -> Self {
        Self {
            path_prefixes: Vec::new(),
            languages: Vec::new(),
            max_pattern_bytes: 4 * 1024,
            max_replacement_bytes: 4 * 1024,
            max_source_bytes: 2 * 1024 * 1024,
            max_scanned_files: 100_000,
            max_scanned_bytes: 256 * 1024 * 1024,
            max_matches: 1_000,
            max_capture_bytes: 256 * 1024,
            max_rewrite_bytes: 16 * 1024 * 1024,
            max_change_diff_bytes: 256 * 1024,
            max_output_bytes: 256 * 1024,
            max_time: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralCapture {
    pub name: String,
    pub range: StructuralRange,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralProvenance {
    pub revision: RevisionId,
    pub language: &'static str,
    pub parser: &'static str,
    pub grammar: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralMatch {
    pub path: PathBuf,
    pub range: StructuralRange,
    pub text: String,
    pub captures: Vec<StructuralCapture>,
    pub provenance: StructuralProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralRewrite {
    pub ir: EditIr,
    pub ir_digest: String,
    pub change_diff: String,
    pub change_diff_digest: String,
    pub changed: bool,
}

impl Serialize for StructuralRewrite {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("StructuralRewrite", 4)?;
        state.serialize_field("ir_digest", &self.ir_digest)?;
        state.serialize_field("change_diff", &self.change_diff)?;
        state.serialize_field("change_diff_digest", &self.change_diff_digest)?;
        state.serialize_field("changed", &self.changed)?;
        state.end()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralResponse {
    pub revision: RevisionId,
    pub matches: Vec<StructuralMatch>,
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub malformed_files: usize,
    pub skipped: SkippedFiles,
    pub omitted: usize,
    pub omitted_complete: bool,
    pub truncated: bool,
    pub result_bytes: usize,
    pub rewrite: Option<StructuralRewrite>,
}

#[derive(Debug)]
pub enum StructuralError {
    Revision(RevisionError),
    InvalidQuery(String),
    InvalidOptions(&'static str),
    MalformedSource(PathBuf),
    IncompleteRewrite(&'static str),
    AmbiguousRewrite(PathBuf),
    EditIr(String),
    TimeLimit,
    Syntax(SyntaxError),
    Serialization(serde_json::Error),
}

impl fmt::Display for StructuralError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::InvalidQuery(reason) => write!(formatter, "invalid structural query: {reason}"),
            Self::InvalidOptions(reason) => {
                write!(formatter, "invalid structural options: {reason}")
            }
            Self::MalformedSource(path) => {
                write!(formatter, "malformed Rust source at {}", path.display())
            }
            Self::IncompleteRewrite(reason) => {
                write!(formatter, "structural rewrite is incomplete: {reason}")
            }
            Self::AmbiguousRewrite(path) => write!(
                formatter,
                "overlapping structural matches at {}",
                path.display()
            ),
            Self::EditIr(reason) => write!(formatter, "structural edit IR failed: {reason}"),
            Self::TimeLimit => formatter.write_str(
                "structural search time limit cooperatively exceeded between bounded operations",
            ),
            Self::Syntax(error) => error.fmt(formatter),
            Self::Serialization(error) => {
                write!(formatter, "serialize structural response: {error}")
            }
        }
    }
}

impl std::error::Error for StructuralError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Syntax(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RevisionError> for StructuralError {
    fn from(error: RevisionError) -> Self {
        match error {
            RevisionError::LimitExceeded(LimitKind::Time) => Self::TimeLimit,
            error => Self::Revision(error),
        }
    }
}

pub fn search(
    workspace: &ManagedWorkspace,
    index: &MetadataIndex,
    syntax: &mut SyntaxIndex,
    query: &StructuralQuery,
    options: &StructuralOptions,
) -> Result<StructuralResponse, StructuralError> {
    validate(query, options)?;
    let deadline = Instant::now()
        .checked_add(options.max_time)
        .ok_or(StructuralError::TimeLimit)?;
    workspace.validate_revision_until(index.revision(), deadline)?;
    if query.rewrite.is_some() && index.source_truncated() {
        return Err(StructuralError::IncompleteRewrite(
            "metadata index is truncated",
        ));
    }

    let pattern = Pattern::try_new(&query.pattern, Rust)
        .map_err(|error| StructuralError::InvalidQuery(error.to_string()))?;
    if pattern.has_error() {
        return Err(StructuralError::InvalidQuery(
            "pattern contains invalid Rust syntax".to_owned(),
        ));
    }
    let defined = pattern.defined_vars().into_iter().collect::<BTreeSet<_>>();
    let replacement = query
        .rewrite
        .as_deref()
        .map(|source| {
            let replacement_pattern = Pattern::try_new(source, Rust)
                .map_err(|error| StructuralError::InvalidQuery(error.to_string()))?;
            if replacement_pattern.has_error() {
                return Err(StructuralError::InvalidQuery(
                    "replacement contains invalid Rust syntax".to_owned(),
                ));
            }
            let template = TemplateFix::try_new(source, &Rust)
                .map_err(|error| StructuralError::InvalidQuery(error.to_string()))?;
            if template
                .used_vars()
                .iter()
                .any(|name| !defined.contains(name))
            {
                return Err(StructuralError::InvalidQuery(
                    "replacement uses a variable not defined by the pattern".to_owned(),
                ));
            }
            Ok(template)
        })
        .transpose()?;
    let replacement_source = query.rewrite.as_deref();

    let mut matches = Vec::new();
    let mut operations = Vec::new();
    let mut change_diff = Vec::new();
    let mut scanned_files = 0;
    let mut scanned_bytes = 0_u64;
    let mut malformed_files = 0;
    let mut skipped = SkippedFiles::default();
    let mut omitted = usize::from(index.source_truncated());
    let mut truncated = index.source_truncated();
    if index.source_truncated() {
        skipped.index_limited = 1;
    }
    let mut capture_bytes = 0;
    let mut rewrite_bytes = 0_usize;
    let mut retained_rewrite_bytes = 0_usize;
    let mut output = OutputBudget::new(options.max_output_bytes, replacement.is_some())?;
    let edit_limits = EditLimits::default();

    'entries: for entry in index.entries() {
        check_deadline(deadline)?;
        if entry.kind != EntryKind::File
            || entry.language.as_deref() != Some("rust")
            || !selected(entry.path.as_path(), options)
        {
            continue;
        }
        if scanned_files == options.max_scanned_files
            || scanned_bytes.saturating_add(entry.size) > options.max_scanned_bytes
        {
            if replacement.is_some() {
                return Err(StructuralError::IncompleteRewrite(
                    "source scan bound was exhausted",
                ));
            }
            truncated = true;
            break;
        }
        scanned_files += 1;
        scanned_bytes += entry.size;
        let Some(source) = entry.text() else {
            match entry.content_state {
                ContentState::Binary => skipped.binary += 1,
                ContentState::InvalidUtf8 => skipped.invalid_utf8 += 1,
                ContentState::TooLarge => skipped.too_large += 1,
                ContentState::IndexLimit => skipped.index_limited += 1,
                ContentState::Directory | ContentState::Text => {}
            }
            omitted = omitted.saturating_add(1);
            truncated = true;
            if replacement.is_some() {
                return Err(StructuralError::IncompleteRewrite(
                    "selected Rust source is unavailable",
                ));
            }
            continue;
        };
        if source.len() > options.max_source_bytes {
            skipped.too_large += 1;
            omitted = omitted.saturating_add(1);
            truncated = true;
            if replacement.is_some() {
                return Err(StructuralError::IncompleteRewrite(
                    "selected Rust source exceeds the source bound",
                ));
            }
            continue;
        }
        let cached = syntax
            .ensure_cached_rust_tree_before(
                index.revision(),
                &entry.path,
                source.as_bytes(),
                options.max_source_bytes,
                deadline,
            )
            .map_err(map_syntax_error)?;
        check_deadline(deadline)?;
        let ast = AstGrep::doc(StrDoc {
            src: cached.source.as_ref().clone(),
            lang: Rust,
            tree: cached.tree,
        });
        check_deadline(deadline)?;
        if ast.root().get_inner_node().has_error() {
            malformed_files += 1;
            if replacement.is_some() {
                return Err(StructuralError::MalformedSource(entry.path.clone()));
            }
        }

        let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        let root = ast.root();
        let mut cursor = root.get_inner_node().walk();
        let mut work = DeadlineWork::new(deadline);
        loop {
            work.tick()?;
            let candidate = ast.adopt(cursor.node());
            if let Some(found) = pattern.match_node(candidate) {
                work.tick()?;
                if matches.len() == options.max_matches {
                    if replacement.is_some() {
                        return Err(StructuralError::IncompleteRewrite(
                            "match bound was exhausted",
                        ));
                    }
                    omitted = omitted.saturating_add(1);
                    truncated = true;
                    break 'entries;
                }
                let found_text = found.text();
                let mut pending_output = match_output_charge(&entry.path, found_text.len())?;
                if !output.fits(pending_output) {
                    if replacement.is_some() {
                        return Err(StructuralError::IncompleteRewrite(
                            "output byte bound was exhausted",
                        ));
                    }
                    omitted = omitted.saturating_add(1);
                    truncated = true;
                    break 'entries;
                }
                let range = node_range(&found, source, deadline)?;
                let mut captures = Vec::new();
                let mut pending_capture_bytes = 0_usize;
                let mut capture_cut = false;
                for name in &defined {
                    if let Some(node) = found.get_env().get_match(name) {
                        capture_cut = !push_capture(
                            &mut captures,
                            name,
                            node,
                            source,
                            capture_bytes,
                            &mut pending_capture_bytes,
                            &output,
                            &mut pending_output,
                            options,
                            deadline,
                        )?;
                    } else {
                        for node in found.get_env().get_multiple_matches(name) {
                            if !push_capture(
                                &mut captures,
                                name,
                                &node,
                                source,
                                capture_bytes,
                                &mut pending_capture_bytes,
                                &output,
                                &mut pending_output,
                                options,
                                deadline,
                            )? {
                                capture_cut = true;
                                break;
                            }
                        }
                    }
                    if capture_cut {
                        break;
                    }
                }
                if capture_cut {
                    if replacement.is_some() {
                        return Err(StructuralError::IncompleteRewrite(
                            "capture or output byte bound was exhausted",
                        ));
                    }
                    omitted = omitted.saturating_add(1);
                    truncated = true;
                    break 'entries;
                }
                output.charge(pending_output)?;
                capture_bytes = capture_bytes
                    .checked_add(pending_capture_bytes)
                    .ok_or(StructuralError::InvalidOptions("capture byte overflow"))?;
                matches.push(StructuralMatch {
                    path: entry.path.clone(),
                    range,
                    text: found_text.into_owned(),
                    captures,
                    provenance: StructuralProvenance {
                        revision: index.revision(),
                        language: SyntaxLanguage::Rust.as_str(),
                        parser: "ast-grep-core@0.40.1",
                        grammar: RUST_GRAMMAR_VERSION,
                    },
                });
                if let Some(replacement) = &replacement {
                    let expansion = replacement_upper_bound(
                        replacement_source.expect("replacement source exists"),
                        &found,
                        source,
                    )?;
                    let rewrite_remaining = options.max_rewrite_bytes.saturating_sub(rewrite_bytes);
                    let ir_charge = source
                        .len()
                        .checked_mul(2)
                        .and_then(|bytes| bytes.checked_add(expansion));
                    let ir_remaining = edit_limits
                        .max_content_bytes
                        .saturating_sub(retained_rewrite_bytes);
                    let output_charge = ir_charge
                        .and_then(|bytes| bytes.checked_mul(6))
                        .and_then(|bytes| bytes.checked_add(1_024));
                    if expansion > rewrite_remaining
                        || ir_charge.is_none_or(|bytes| bytes > ir_remaining)
                        || output_charge.is_none_or(|bytes| bytes > output.remaining())
                    {
                        return Err(StructuralError::IncompleteRewrite(
                            "replacement expansion exceeds a rewrite, output, or IR bound",
                        ));
                    }
                    let edit = found.replace_by(replacement);
                    check_deadline(deadline)?;
                    rewrite_bytes = rewrite_bytes
                        .checked_add(edit.inserted_text.len())
                        .filter(|bytes| *bytes <= options.max_rewrite_bytes)
                        .ok_or(StructuralError::IncompleteRewrite(
                            "replacement byte bound was exhausted",
                        ))?;
                    edits.push((
                        edit.position,
                        edit.position + edit.deleted_length,
                        edit.inserted_text,
                    ));
                }
            }
            work.tick()?;
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                continue;
            }
            loop {
                work.tick()?;
                if !cursor.goto_parent() {
                    break;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
            if cursor.node() == root.get_inner_node() {
                break;
            }
        }

        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|edit| (edit.0, edit.1));
        if edits
            .windows(2)
            .any(|pair| pair[1].0 < pair[0].1 || pair[1].0 == pair[0].0)
        {
            return Err(StructuralError::AmbiguousRewrite(entry.path.clone()));
        }
        let final_len = edits
            .iter()
            .try_fold(source.len(), |length, (start, end, inserted)| {
                length
                    .checked_sub(*end - *start)?
                    .checked_add(inserted.len())
            })
            .ok_or(StructuralError::IncompleteRewrite(
                "rewritten source size overflow",
            ))?;
        if final_len > options.max_rewrite_bytes {
            return Err(StructuralError::IncompleteRewrite(
                "rewritten source bound was exhausted",
            ));
        }
        let mut rewritten = String::new();
        rewritten.try_reserve_exact(final_len).map_err(|_| {
            StructuralError::IncompleteRewrite("rewritten source allocation failed")
        })?;
        let mut cursor = 0_usize;
        for (start, end, inserted) in edits {
            check_deadline(deadline)?;
            let inserted = std::str::from_utf8(&inserted).map_err(|_| {
                StructuralError::InvalidQuery("replacement is not UTF-8".to_owned())
            })?;
            rewritten.push_str(&source[cursor..start]);
            rewritten.push_str(inserted);
            cursor = end;
        }
        rewritten.push_str(&source[cursor..]);
        check_deadline(deadline)?;
        if rewritten == source {
            continue;
        }
        let path_text = entry
            .path
            .to_str()
            .ok_or(StructuralError::IncompleteRewrite("path is not UTF-8"))?;
        let operation_charge = operation_output_charge(path_text, source, &rewritten)?;
        output
            .charge(operation_charge)
            .map_err(|_| StructuralError::IncompleteRewrite("output byte bound was exhausted"))?;
        retained_rewrite_bytes = retained_rewrite_bytes
            .checked_add(source.len())
            .and_then(|bytes| bytes.checked_add(rewritten.len()))
            .filter(|bytes| {
                *bytes <= options.max_rewrite_bytes && *bytes <= edit_limits.max_content_bytes
            })
            .ok_or(StructuralError::IncompleteRewrite(
                "retained rewrite byte bound was exhausted",
            ))?;
        let path = RootRelativePath::parse(path_text.to_owned(), edit_limits.max_path_bytes)
            .map_err(|error| StructuralError::EditIr(error.to_string()))?;
        let base = BaseFile::new(source.as_bytes(), entry.executable)
            .map_err(|error| StructuralError::EditIr(error.to_string()))?;
        let diff = render_whole_file_replacement(
            path.as_str(),
            source.as_bytes(),
            rewritten.as_bytes(),
            if entry.executable { 0o755 } else { 0o644 },
            options
                .max_change_diff_bytes
                .saturating_sub(change_diff.len()),
        )
        .map_err(map_diff_error)?;
        output
            .charge(json_string_charge(diff.len())?)
            .map_err(|_| StructuralError::IncompleteRewrite("output byte bound was exhausted"))?;
        change_diff.extend_from_slice(&diff);
        operations.push(EditOperation::ReplaceRange {
            path,
            base_digest: base.digest().clone(),
            range: ByteRange::new(0, source.len())
                .map_err(|error| StructuralError::EditIr(error.to_string()))?,
            expected: base.content().clone(),
            replacement: TextContent::from_bytes(rewritten.as_bytes())
                .map_err(|error| StructuralError::EditIr(error.to_string()))?,
            executable: ExecutableMode::Preserve,
        });
    }

    workspace.validate_revision_until(index.revision(), deadline)?;
    let rewrite = if replacement.is_some() {
        let revision = RevisionToken::parse(index.revision().to_string())
            .map_err(|error| StructuralError::EditIr(error.to_string()))?;
        let change_diff = String::from_utf8(change_diff)
            .map_err(|_| StructuralError::EditIr("change diff is not UTF-8".to_owned()))?;
        let change_diff_digest =
            format!("blake3:{}", blake3::hash(change_diff.as_bytes()).to_hex());
        let changed = !operations.is_empty();
        let ir = EditIr::new(revision, operations, edit_limits)
            .and_then(|ir| ir.with_expected_change_diff_digest(change_diff_digest.clone()))
            .map_err(|error| StructuralError::EditIr(error.to_string()))?;
        let canonical_ir = ir.canonical_bytes();
        Some(StructuralRewrite {
            ir_digest: format!("blake3:{}", blake3::hash(&canonical_ir).to_hex()),
            change_diff_digest,
            ir,
            change_diff,
            changed,
        })
    } else {
        None
    };
    let mut response = StructuralResponse {
        revision: index.revision(),
        matches,
        scanned_files,
        scanned_bytes,
        malformed_files,
        skipped,
        omitted,
        omitted_complete: !truncated,
        truncated,
        result_bytes: 0,
        rewrite,
    };
    set_result_size(
        &mut response,
        options.max_output_bytes,
        replacement.is_some(),
        deadline,
    )?;
    workspace.validate_revision_until(index.revision(), deadline)?;
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn push_capture(
    captures: &mut Vec<StructuralCapture>,
    name: &str,
    node: &ast_grep_core::Node<'_, StrDoc<Rust>>,
    source: &str,
    retained_bytes: usize,
    pending_bytes: &mut usize,
    output: &OutputBudget,
    pending_output: &mut usize,
    options: &StructuralOptions,
    deadline: Instant,
) -> Result<bool, StructuralError> {
    let text = node.text();
    let Some(next) = retained_bytes
        .checked_add(*pending_bytes)
        .and_then(|bytes| bytes.checked_add(name.len()))
        .and_then(|bytes| bytes.checked_add(text.len()))
        .filter(|bytes| *bytes <= options.max_capture_bytes)
    else {
        return Ok(false);
    };
    let charge = capture_output_charge(name.len(), text.len())?;
    let Some(next_output) = pending_output.checked_add(charge) else {
        return Ok(false);
    };
    if !output.fits(next_output) {
        return Ok(false);
    }
    *pending_bytes = next - retained_bytes;
    *pending_output = next_output;
    captures.push(StructuralCapture {
        name: name.to_owned(),
        range: node_range(node, source, deadline)?,
        text: text.into_owned(),
    });
    Ok(true)
}

fn node_range(
    node: &ast_grep_core::Node<'_, StrDoc<Rust>>,
    source: &str,
    deadline: Instant,
) -> Result<StructuralRange, StructuralError> {
    let range = node.range();
    let inner = node.get_inner_node();
    let start = inner.start_position();
    let end = inner.end_position();
    Ok(StructuralRange {
        start_byte: range.start,
        end_byte: range.end,
        start_line: start.row + 1,
        start_column: char_column(source, range.start, start.column, deadline)? + 1,
        end_line: end.row + 1,
        end_column: char_column(source, range.end, end.column, deadline)? + 1,
    })
}

fn char_column(
    source: &str,
    byte_offset: usize,
    byte_column: usize,
    deadline: Instant,
) -> Result<usize, StructuralError> {
    let line_start = byte_offset
        .checked_sub(byte_column)
        .ok_or(StructuralError::InvalidQuery(
            "parser returned an invalid source position".to_owned(),
        ))?;
    let line = source
        .get(line_start..byte_offset)
        .ok_or_else(|| StructuralError::InvalidQuery("parser range is not UTF-8".to_owned()))?;
    let mut count = 0_usize;
    for character in line.chars() {
        if count.is_multiple_of(1024) {
            check_deadline(deadline)?;
        }
        count += 1;
        let _ = character;
    }
    Ok(count)
}

fn validate(query: &StructuralQuery, options: &StructuralOptions) -> Result<(), StructuralError> {
    if options.max_pattern_bytes == 0
        || options.max_replacement_bytes == 0
        || options.max_source_bytes == 0
        || options.max_scanned_files == 0
        || options.max_scanned_bytes == 0
        || options.max_matches == 0
        || options.max_capture_bytes == 0
        || options.max_rewrite_bytes == 0
        || options.max_change_diff_bytes == 0
        || options.max_output_bytes == 0
        || options.max_time.is_zero()
    {
        return Err(StructuralError::InvalidOptions(
            "all bounds must be nonzero",
        ));
    }
    if query.pattern.is_empty()
        || query.pattern.len() > options.max_pattern_bytes
        || query.pattern.contains('\0')
    {
        return Err(StructuralError::InvalidQuery(
            "pattern is empty, too long, or contains NUL".to_owned(),
        ));
    }
    if query.rewrite.as_ref().is_some_and(|rewrite| {
        rewrite.is_empty()
            || rewrite.len() > options.max_replacement_bytes
            || rewrite.contains('\0')
    }) {
        return Err(StructuralError::InvalidQuery(
            "replacement is empty, too long, or contains NUL".to_owned(),
        ));
    }
    if options.languages.iter().any(|language| language.is_empty()) {
        return Err(StructuralError::InvalidOptions(
            "language filters must not be empty",
        ));
    }
    if options.path_prefixes.iter().any(|path| {
        path.as_os_str().is_empty()
            || path.is_absolute()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    }) {
        return Err(StructuralError::InvalidOptions(
            "path filters must be canonical and root-relative",
        ));
    }
    Ok(())
}

fn selected(path: &std::path::Path, options: &StructuralOptions) -> bool {
    (options.path_prefixes.is_empty()
        || options
            .path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix)))
        && (options.languages.is_empty()
            || options.languages.iter().any(|language| language == "rust"))
}

struct OutputBudget {
    used: usize,
    max: usize,
}

impl OutputBudget {
    fn new(max: usize, rewrite: bool) -> Result<Self, StructuralError> {
        let used = if rewrite { 2_048 } else { 1_024 };
        if used > max {
            return Err(StructuralError::InvalidOptions(
                "output byte bound is smaller than response metadata",
            ));
        }
        Ok(Self { used, max })
    }

    fn fits(&self, bytes: usize) -> bool {
        self.used
            .checked_add(bytes)
            .is_some_and(|next| next <= self.max)
    }

    fn charge(&mut self, bytes: usize) -> Result<(), StructuralError> {
        if !self.fits(bytes) {
            return Err(StructuralError::InvalidOptions(
                "structural output accounting exceeded its byte bound",
            ));
        }
        self.used += bytes;
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.max - self.used
    }
}

fn replacement_upper_bound(
    replacement: &str,
    found: &ast_grep_core::NodeMatch<'_, StrDoc<Rust>>,
    source: &str,
) -> Result<usize, StructuralError> {
    let occurrences = replacement.bytes().filter(|byte| *byte == b'$').count();
    let mut captured_bytes = 0_usize;
    let mut captured_newlines = 0_usize;
    for variable in found.get_env().get_matched_variables() {
        let Some(bytes) = found.get_env().get_var_bytes(&variable) else {
            continue;
        };
        captured_bytes =
            captured_bytes
                .checked_add(bytes.len())
                .ok_or(StructuralError::IncompleteRewrite(
                    "replacement expansion size overflow",
                ))?;
        captured_newlines = captured_newlines
            .checked_add(bytes.iter().filter(|byte| **byte == b'\n').count())
            .ok_or(StructuralError::IncompleteRewrite(
                "replacement expansion size overflow",
            ))?;
    }
    let inner = replacement
        .len()
        .checked_add(
            occurrences
                .checked_mul(
                    captured_bytes
                        .checked_add(replacement.len().checked_mul(captured_newlines).ok_or(
                            StructuralError::IncompleteRewrite(
                                "replacement expansion size overflow",
                            ),
                        )?)
                        .ok_or(StructuralError::IncompleteRewrite(
                            "replacement expansion size overflow",
                        ))?,
                )
                .ok_or(StructuralError::IncompleteRewrite(
                    "replacement expansion size overflow",
                ))?,
        )
        .ok_or(StructuralError::IncompleteRewrite(
            "replacement expansion size overflow",
        ))?;
    let outer_indent = source[..found.range().start]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b' ')
        .count();
    let inner_newlines = replacement
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .checked_add(occurrences.checked_mul(captured_newlines).ok_or(
            StructuralError::IncompleteRewrite("replacement expansion size overflow"),
        )?)
        .ok_or(StructuralError::IncompleteRewrite(
            "replacement expansion size overflow",
        ))?;
    inner
        .checked_add(outer_indent.checked_mul(inner_newlines).ok_or(
            StructuralError::IncompleteRewrite("replacement expansion size overflow"),
        )?)
        .ok_or(StructuralError::IncompleteRewrite(
            "replacement expansion size overflow",
        ))
}

fn match_output_charge(
    path: &std::path::Path,
    text_bytes: usize,
) -> Result<usize, StructuralError> {
    size_of::<StructuralMatch>()
        .checked_add(512)
        .and_then(|bytes| {
            bytes.checked_add(json_string_charge(path.as_os_str().as_encoded_bytes().len()).ok()?)
        })
        .and_then(|bytes| bytes.checked_add(json_string_charge(text_bytes).ok()?))
        .ok_or(StructuralError::InvalidOptions(
            "output byte accounting overflow",
        ))
}

fn capture_output_charge(name_bytes: usize, text_bytes: usize) -> Result<usize, StructuralError> {
    size_of::<StructuralCapture>()
        .checked_add(256)
        .and_then(|bytes| bytes.checked_add(json_string_charge(name_bytes).ok()?))
        .and_then(|bytes| bytes.checked_add(json_string_charge(text_bytes).ok()?))
        .ok_or(StructuralError::InvalidOptions(
            "output byte accounting overflow",
        ))
}

fn operation_output_charge(
    path: &str,
    before: &str,
    after: &str,
) -> Result<usize, StructuralError> {
    1_024_usize
        .checked_add(json_string_charge(path.len())?)
        .and_then(|bytes| bytes.checked_add(json_string_charge(before.len()).ok()?))
        .and_then(|bytes| bytes.checked_add(json_string_charge(after.len()).ok()?))
        .ok_or(StructuralError::InvalidOptions(
            "output byte accounting overflow",
        ))
}

fn json_string_charge(bytes: usize) -> Result<usize, StructuralError> {
    bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or(StructuralError::InvalidOptions(
            "output byte accounting overflow",
        ))
}

fn set_result_size(
    response: &mut StructuralResponse,
    max: usize,
    rewrite: bool,
    deadline: Instant,
) -> Result<(), StructuralError> {
    check_deadline(deadline)?;
    debug_assert_eq!(response.result_bytes, 0);
    let zero_size = serde_json::to_vec(response)
        .map_err(StructuralError::Serialization)?
        .len();
    let base = zero_size
        .checked_sub(1)
        .ok_or(StructuralError::InvalidOptions(
            "invalid response serialization",
        ))?;
    let mut size = base;
    loop {
        let next =
            base.checked_add(decimal_digits(size))
                .ok_or(StructuralError::InvalidOptions(
                    "output byte accounting overflow",
                ))?;
        if next == size {
            break;
        }
        size = next;
    }
    if size > max {
        return Err(if rewrite {
            StructuralError::IncompleteRewrite("output byte bound was exhausted")
        } else {
            StructuralError::InvalidOptions("output accounting exceeded its byte bound")
        });
    }
    response.result_bytes = size;
    check_deadline(deadline)
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn map_diff_error(error: ChangeDiffError) -> StructuralError {
    match error {
        ChangeDiffError::Limit => {
            StructuralError::IncompleteRewrite("change diff byte bound was exhausted")
        }
        ChangeDiffError::Io(error) => StructuralError::EditIr(error.to_string()),
    }
}

fn map_syntax_error(error: SyntaxError) -> StructuralError {
    match error {
        SyntaxError::ParseTimeout | SyntaxError::QueryTimeout => StructuralError::TimeLimit,
        error => StructuralError::Syntax(error),
    }
}

struct DeadlineWork {
    deadline: Instant,
    work: u8,
}

impl DeadlineWork {
    fn new(deadline: Instant) -> Self {
        Self { deadline, work: 0 }
    }

    fn tick(&mut self) -> Result<(), StructuralError> {
        self.work = self.work.wrapping_add(1);
        if self.work == 0 {
            check_deadline(self.deadline)?;
        }
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), StructuralError> {
    if Instant::now() >= deadline {
        Err(StructuralError::TimeLimit)
    } else {
        Ok(())
    }
}

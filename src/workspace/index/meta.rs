use std::{
    fmt,
    mem::size_of,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use crate::workspace::revision::{
    ContentDigest, EntryKind, EpochId, ManagedWorkspace, RevisionError, RevisionId, Snapshot,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexOptions {
    pub max_entries: usize,
    pub max_indexed_bytes: u64,
    pub max_file_bytes: u64,
    pub max_ignore_files: usize,
    pub max_ignore_bytes: usize,
    pub max_ignore_rules: usize,
    pub max_compiled_ignore_bytes: usize,
    pub max_pattern_bytes: usize,
    pub max_pattern_components: usize,
    pub max_matcher_work_bytes: usize,
    pub max_symbols_per_file: usize,
    pub max_symbol_bytes: usize,
    pub max_build_time: Duration,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            max_entries: 250_000,
            max_indexed_bytes: 256 * 1024 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
            max_ignore_files: 4_096,
            max_ignore_bytes: 4 * 1024 * 1024,
            max_ignore_rules: 65_536,
            max_compiled_ignore_bytes: 16 * 1024 * 1024,
            max_pattern_bytes: 4_096,
            max_pattern_components: 256,
            max_matcher_work_bytes: 64 * 1024,
            max_symbols_per_file: 256,
            max_symbol_bytes: 64 * 1024,
            max_build_time: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentState {
    Directory,
    Text,
    Binary,
    InvalidUtf8,
    TooLarge,
    IndexLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolKind {
    Function,
    Type,
    Module,
    Constant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub executable: bool,
    pub size: u64,
    pub language: Option<String>,
    pub content_state: ContentState,
    pub symbols: Vec<BasicSymbol>,
    text: Option<String>,
}

impl MetadataEntry {
    pub(crate) fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexCursor {
    epoch: EpochId,
    revision: RevisionId,
    digest: String,
    index_digest: [u8; 32],
    options_digest: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct MetadataIndex {
    epoch: EpochId,
    revision: RevisionId,
    digest: ContentDigest,
    index_digest: [u8; 32],
    options_digest: [u8; 32],
    entries: Vec<MetadataEntry>,
    truncated: bool,
}

impl MetadataIndex {
    pub fn build(
        workspace: &ManagedWorkspace,
        expected: RevisionId,
        options: &IndexOptions,
    ) -> Result<Self, IndexError> {
        validate_options(options)?;
        let started = Instant::now();
        let deadline = started
            .checked_add(options.max_build_time)
            .unwrap_or(started);
        let max_file_bytes = usize::try_from(options.max_file_bytes)
            .map_err(|_| IndexError::InvalidOptions("file byte bound is out of range"))?;
        let max_content_bytes = options
            .max_indexed_bytes
            .checked_add(options.max_ignore_bytes as u64)
            .ok_or(IndexError::InvalidOptions(
                "retained content bound overflow",
            ))?;
        let snapshot = workspace.metadata_snapshot_before(
            expected,
            max_file_bytes,
            options.max_ignore_bytes,
            max_content_bytes,
            deadline,
        )?;
        let index = Self::from_snapshot_until(&snapshot, options, deadline)?;
        workspace.validate_revision_until(expected, deadline)?;
        Ok(index)
    }

    pub fn from_snapshot(snapshot: &Snapshot, options: &IndexOptions) -> Result<Self, IndexError> {
        validate_options(options)?;
        let started = Instant::now();
        let deadline = started
            .checked_add(options.max_build_time)
            .unwrap_or(started);
        Self::from_snapshot_until(snapshot, options, deadline)
    }

    fn from_snapshot_until(
        snapshot: &Snapshot,
        options: &IndexOptions,
        deadline: Instant,
    ) -> Result<Self, IndexError> {
        let rules = IgnoreRules::compile(snapshot, options, deadline)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(options.max_entries.min(snapshot.entries().len()))
            .map_err(|_| IndexError::InvalidOptions("entry allocation failed"))?;
        let mut indexed_bytes = 0_u64;
        let mut truncated = false;

        for source in snapshot.entries() {
            if Instant::now() >= deadline {
                truncated = true;
                break;
            }
            validate_path(&source.path)?;
            let ignored = if private_path(&source.path) {
                true
            } else {
                match rules.ignored(&source.path, source.kind, deadline) {
                    Ok(ignored) => ignored,
                    Err(IndexError::DeadlineExceeded) => {
                        truncated = true;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            };
            if ignored {
                continue;
            }
            if entries.len() == options.max_entries {
                truncated = true;
                break;
            }
            let path_text = source
                .path
                .to_str()
                .ok_or_else(|| IndexError::NonUtf8Path(source.path.clone()))?;
            let size = source.size;
            let language = language(path_text).map(str::to_owned);
            let (content_state, text, symbols) = if source.kind == EntryKind::Directory {
                (ContentState::Directory, None, Vec::new())
            } else if size > options.max_file_bytes {
                (ContentState::TooLarge, None, Vec::new())
            } else if source.has_nul {
                (ContentState::Binary, None, Vec::new())
            } else if !source.valid_utf8 {
                (ContentState::InvalidUtf8, None, Vec::new())
            } else if !source.content_complete {
                truncated = true;
                (ContentState::IndexLimit, None, Vec::new())
            } else if let Ok(value) = std::str::from_utf8(&source.bytes) {
                if indexed_bytes.saturating_add(size) > options.max_indexed_bytes {
                    truncated = true;
                    (ContentState::IndexLimit, None, Vec::new())
                } else {
                    indexed_bytes += size;
                    let mut text = String::new();
                    text.try_reserve_exact(value.len())
                        .map_err(|_| IndexError::InvalidOptions("text allocation failed"))?;
                    text.push_str(value);
                    (
                        ContentState::Text,
                        Some(text),
                        lexical_symbols(value, options, deadline)?,
                    )
                }
            } else {
                return Err(IndexError::InvalidOptions(
                    "revision UTF-8 classification disagrees with retained content",
                ));
            };
            entries.push(MetadataEntry {
                path: source.path.clone(),
                kind: source.kind,
                executable: source.executable,
                size,
                language,
                content_state,
                symbols,
                text,
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let options_digest = digest_options(options, deadline)?;
        let index_digest = digest_index(snapshot, &entries, options_digest, truncated, deadline)?;
        Ok(Self {
            epoch: snapshot.revision().epoch(),
            revision: snapshot.revision().id(),
            digest: snapshot.revision().digest().clone(),
            index_digest,
            options_digest,
            entries,
            truncated,
        })
    }

    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    pub fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    pub fn index_digest(&self) -> &[u8; 32] {
        &self.index_digest
    }

    pub fn entries(&self) -> &[MetadataEntry] {
        &self.entries
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn cursor(&self) -> IndexCursor {
        IndexCursor {
            epoch: self.epoch,
            revision: self.revision,
            digest: self.digest.to_string(),
            index_digest: self.index_digest,
            options_digest: self.options_digest,
        }
    }

    pub fn validate_cursor(
        &self,
        workspace: &ManagedWorkspace,
        cursor: &IndexCursor,
    ) -> Result<(), IndexError> {
        workspace.validate_revision(cursor.revision)?;
        if cursor.epoch != self.epoch
            || cursor.revision != self.revision
            || cursor.digest != self.digest.as_str()
            || cursor.index_digest != self.index_digest
            || cursor.options_digest != self.options_digest
        {
            return Err(IndexError::CursorMismatch);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum IndexError {
    Revision(RevisionError),
    InvalidOptions(&'static str),
    UnsafePath(PathBuf),
    NonUtf8Path(PathBuf),
    InvalidIgnore {
        path: PathBuf,
        line: usize,
        reason: &'static str,
    },
    IgnoreLimit(&'static str),
    DeadlineExceeded,
    CursorMismatch,
}

impl From<RevisionError> for IndexError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(crate::workspace::revision::LimitKind::Time) => {
                Self::DeadlineExceeded
            }
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::InvalidOptions(reason) => write!(formatter, "invalid index options: {reason}"),
            Self::UnsafePath(path) => write!(formatter, "unsafe index path: {}", path.display()),
            Self::NonUtf8Path(path) => {
                write!(formatter, "non-UTF-8 index path: {}", path.display())
            }
            Self::InvalidIgnore { path, line, reason } => {
                write!(
                    formatter,
                    "{}:{line}: unsupported gitignore syntax: {reason}",
                    path.display()
                )
            }
            Self::IgnoreLimit(kind) => write!(formatter, "gitignore {kind} limit exceeded"),
            Self::DeadlineExceeded => formatter.write_str("metadata index deadline exceeded"),
            Self::CursorMismatch => formatter.write_str("index cursor does not match this index"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            _ => None,
        }
    }
}

fn validate_options(options: &IndexOptions) -> Result<(), IndexError> {
    if options.max_entries == 0
        || options.max_indexed_bytes == 0
        || options.max_file_bytes == 0
        || options.max_ignore_files == 0
        || options.max_ignore_bytes == 0
        || options.max_ignore_rules == 0
        || options.max_compiled_ignore_bytes == 0
        || options.max_pattern_bytes == 0
        || options.max_pattern_components == 0
        || options.max_matcher_work_bytes == 0
        || options.max_symbols_per_file == 0
        || options.max_symbol_bytes == 0
        || options.max_build_time.is_zero()
    {
        Err(IndexError::InvalidOptions("all bounds must be nonzero"))
    } else {
        Ok(())
    }
}

fn validate_path(path: &Path) -> Result<(), IndexError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        Err(IndexError::UnsafePath(path.to_owned()))
    } else {
        Ok(())
    }
}

fn private_path(path: &Path) -> bool {
    path.components().any(|part| {
        let Component::Normal(name) = part else {
            return true;
        };
        name == ".git" || name == ".kit"
    }) || path.components().next().is_some_and(|part| {
        matches!(part, Component::Normal(name) if name.to_string_lossy().starts_with(".kit-revision-"))
    })
}

fn language(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.').map(|(_, value)| value)?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "css" => "css",
        "go" => "go",
        "html" | "htm" => "html",
        "java" => "java",
        "js" | "mjs" | "cjs" => "javascript",
        "json" => "json",
        "kt" | "kts" => "kotlin",
        "md" | "markdown" => "markdown",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "sh" | "bash" | "zsh" => "shell",
        "swift" => "swift",
        "toml" => "toml",
        "ts" | "tsx" => "typescript",
        "xml" => "xml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

fn lexical_symbols(
    text: &str,
    options: &IndexOptions,
    deadline: Instant,
) -> Result<Vec<BasicSymbol>, IndexError> {
    let mut symbols = Vec::new();
    let mut bytes = 0_usize;
    for (line, source) in text.lines().enumerate() {
        check_deadline(deadline)?;
        let source = source.trim_start();
        let source = source
            .strip_prefix("pub ")
            .or_else(|| source.strip_prefix("export "))
            .unwrap_or(source);
        let found = [
            ("async fn ", SymbolKind::Function),
            ("fn ", SymbolKind::Function),
            ("def ", SymbolKind::Function),
            ("function ", SymbolKind::Function),
            ("struct ", SymbolKind::Type),
            ("enum ", SymbolKind::Type),
            ("class ", SymbolKind::Type),
            ("trait ", SymbolKind::Type),
            ("interface ", SymbolKind::Type),
            ("mod ", SymbolKind::Module),
            ("module ", SymbolKind::Module),
            ("const ", SymbolKind::Constant),
            ("static ", SymbolKind::Constant),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| source.strip_prefix(prefix).map(|rest| (rest, kind)));
        let Some((rest, kind)) = found else {
            continue;
        };
        let mut name = String::new();
        name.try_reserve_exact(rest.len().min(options.max_symbol_bytes))
            .map_err(|_| IndexError::InvalidOptions("symbol allocation failed"))?;
        for value in rest.chars() {
            if !value.is_alphanumeric() && value != '_' && value != '$' {
                break;
            }
            if bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len_utf8()))
                .is_none_or(|total| total > options.max_symbol_bytes)
            {
                return Ok(symbols);
            }
            name.push(value);
        }
        if name.is_empty()
            || (!name.starts_with('_')
                && !name.starts_with('$')
                && !name.starts_with(char::is_alphabetic))
        {
            continue;
        }
        if symbols.len() == options.max_symbols_per_file
            || bytes.saturating_add(name.len()) > options.max_symbol_bytes
        {
            break;
        }
        bytes += name.len();
        symbols
            .try_reserve(1)
            .map_err(|_| IndexError::InvalidOptions("symbol allocation failed"))?;
        symbols.push(BasicSymbol {
            name,
            kind,
            line: line + 1,
        });
    }
    Ok(symbols)
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    pattern: Vec<String>,
    negated: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    trailing_globstar: bool,
}

#[derive(Clone, Debug)]
struct IgnoreFileRules {
    base: Vec<String>,
    rules: Vec<IgnoreRule>,
}

#[derive(Clone, Debug)]
struct IgnoreRules {
    files: Vec<IgnoreFileRules>,
    max_matcher_work_bytes: usize,
}

impl IgnoreRules {
    fn compile(
        snapshot: &Snapshot,
        options: &IndexOptions,
        deadline: Instant,
    ) -> Result<Self, IndexError> {
        let mut files = Vec::new();
        for entry in snapshot.entries() {
            check_deadline(deadline)?;
            if entry.kind == EntryKind::File
                && entry
                    .path
                    .file_name()
                    .is_some_and(|name| name == ".gitignore")
            {
                if !entry.content_complete {
                    return Err(IndexError::IgnoreLimit("byte"));
                }
                if files.len() == options.max_ignore_files {
                    return Err(IndexError::IgnoreLimit("file count"));
                }
                files
                    .try_reserve(1)
                    .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
                files.push(entry);
            }
        }
        files.sort_by(|left, right| {
            left.path
                .components()
                .count()
                .cmp(&right.path.components().count())
                .then_with(|| left.path.cmp(&right.path))
        });
        let total = files.iter().try_fold(0_usize, |total, file| {
            total
                .checked_add(file.bytes.len())
                .ok_or(IndexError::IgnoreLimit("byte"))
        })?;
        if total > options.max_ignore_bytes {
            return Err(IndexError::IgnoreLimit("byte"));
        }

        let mut compiled_bytes = 0_usize;
        let mut rule_count = 0_usize;
        let mut compiled_files = Vec::new();
        for file in files {
            check_deadline(deadline)?;
            let source =
                std::str::from_utf8(&file.bytes).map_err(|_| IndexError::InvalidIgnore {
                    path: file.path.clone(),
                    line: 0,
                    reason: "file is not UTF-8",
                })?;
            let base_count = file
                .path
                .parent()
                .map_or(0, |path| path.components().count());
            let mut base_parts = Vec::new();
            base_parts
                .try_reserve_exact(base_count)
                .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
            if let Some(parent) = file.path.parent() {
                for part in parent.components() {
                    base_parts.push(
                        part.as_os_str()
                            .to_str()
                            .ok_or_else(|| IndexError::NonUtf8Path(file.path.clone()))?,
                    );
                }
            }
            let base_bytes = base_parts.iter().try_fold(0_usize, |total, part| {
                total
                    .checked_add(part.len())
                    .ok_or(IndexError::IgnoreLimit("compiled byte"))
            })?;
            let base_structure = base_parts
                .len()
                .checked_mul(size_of::<String>())
                .and_then(|value| value.checked_add(size_of::<IgnoreFileRules>()))
                .and_then(|value| value.checked_add(base_bytes))
                .ok_or(IndexError::IgnoreLimit("compiled byte"))?;
            charge_compiled(&mut compiled_bytes, base_structure, options)?;
            let base = copy_parts(&base_parts)?;
            let mut rules = Vec::new();
            for (line_index, raw) in source.split('\n').enumerate() {
                check_deadline(deadline)?;
                let raw = raw.strip_suffix('\r').unwrap_or(raw);
                if raw.len() > options.max_pattern_bytes {
                    return Err(ignore_error(&file.path, line_index, "pattern is too long"));
                }
                let Some((parsed, negated)) = parse_ignore_line(raw, &file.path, line_index)?
                else {
                    continue;
                };
                let mut line = parsed.as_str();
                if line.is_empty() {
                    if negated {
                        return Err(ignore_error(&file.path, line_index, "empty negation"));
                    }
                    continue;
                }
                if line.len() > options.max_pattern_bytes {
                    return Err(ignore_error(&file.path, line_index, "pattern is too long"));
                }
                if line.contains('[') || line.contains(']') {
                    return Err(ignore_error(&file.path, line_index, "character class"));
                }
                let directory_only = line.ends_with('/');
                if directory_only {
                    line = &line[..line.len() - 1];
                }
                let anchored = line.starts_with('/');
                if anchored {
                    line = &line[1..];
                }
                if line.is_empty() || line.contains("//") {
                    return Err(ignore_error(&file.path, line_index, "empty path component"));
                }
                let pattern_count = line.split('/').count();
                if pattern_count > options.max_pattern_components {
                    return Err(IndexError::IgnoreLimit("pattern component count"));
                }
                if line.split('/').any(|part| {
                    (part.contains("**") && part != "**") || part == "." || part == ".."
                }) {
                    return Err(ignore_error(
                        &file.path,
                        line_index,
                        "ambiguous globstar or component",
                    ));
                }
                if rule_count == options.max_ignore_rules {
                    return Err(IndexError::IgnoreLimit("rule count"));
                }
                let pattern_bytes = line
                    .len()
                    .checked_sub(pattern_count.saturating_sub(1))
                    .ok_or(IndexError::IgnoreLimit("compiled byte"))?;
                let rule_bytes = pattern_count
                    .checked_mul(size_of::<String>())
                    .and_then(|value| value.checked_add(size_of::<IgnoreRule>()))
                    .and_then(|value| value.checked_add(pattern_bytes))
                    .ok_or(IndexError::IgnoreLimit("compiled byte"))?;
                charge_compiled(&mut compiled_bytes, rule_bytes, options)?;
                rules
                    .try_reserve(1)
                    .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
                let pattern = copy_pattern(line, pattern_count)?;
                rule_count += 1;
                rules.push(IgnoreRule {
                    trailing_globstar: pattern.last().is_some_and(|part| part == "**"),
                    has_slash: pattern.len() > 1,
                    pattern,
                    negated,
                    directory_only,
                    anchored,
                });
            }
            compiled_files
                .try_reserve(1)
                .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
            compiled_files.push(IgnoreFileRules { base, rules });
        }
        Ok(Self {
            files: compiled_files,
            max_matcher_work_bytes: options.max_matcher_work_bytes,
        })
    }

    fn ignored(&self, path: &Path, kind: EntryKind, deadline: Instant) -> Result<bool, IndexError> {
        let part_count = path.components().count();
        let work_bytes = part_count
            .checked_mul(size_of::<&str>())
            .ok_or(IndexError::IgnoreLimit("matcher workspace"))?;
        if work_bytes > self.max_matcher_work_bytes {
            return Err(IndexError::IgnoreLimit("matcher workspace"));
        }
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(part_count)
            .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
        for part in path.components() {
            parts.push(
                part.as_os_str()
                    .to_str()
                    .ok_or_else(|| IndexError::NonUtf8Path(path.to_owned()))?,
            );
        }
        for end in 1..=parts.len() {
            check_deadline(deadline)?;
            let is_directory = end < parts.len() || kind == EntryKind::Directory;
            let mut ignored = false;
            for file in &self.files {
                for rule in &file.rules {
                    if rule.matches(&file.base, &parts[..end], is_directory, deadline)? {
                        ignored = !rule.negated;
                    }
                }
            }
            if ignored {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl IgnoreRule {
    fn matches(
        &self,
        base: &[String],
        path: &[&str],
        is_directory: bool,
        deadline: Instant,
    ) -> Result<bool, IndexError> {
        if path.len() <= base.len() || !path.iter().zip(base).all(|(left, right)| *left == right) {
            return Ok(false);
        }
        let relative = &path[base.len()..];
        if self.directory_only && !is_directory {
            return Ok(false);
        }
        if self.anchored || self.has_slash {
            if self.trailing_globstar && relative.len() + 1 == self.pattern.len() {
                return Ok(false);
            }
            glob_path(&self.pattern, relative, deadline)
        } else {
            relative.last().map_or(Ok(false), |part| {
                segment_match(&self.pattern[0], part, deadline)
            })
        }
    }
}

fn parse_ignore_line(
    raw: &str,
    path: &Path,
    line: usize,
) -> Result<Option<(String, bool)>, IndexError> {
    if raw.is_empty() || raw.starts_with('#') {
        return Ok(None);
    }
    let escaped_leading = raw.starts_with("\\!") || raw.starts_with("\\#");
    let (raw, negated) = if escaped_leading {
        (&raw[1..], false)
    } else if let Some(raw) = raw.strip_prefix('!') {
        (raw, true)
    } else {
        (raw, false)
    };
    let mut chars = Vec::new();
    chars
        .try_reserve_exact(raw.chars().count())
        .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
    let mut source = raw.chars();
    while let Some(value) = source.next() {
        if value == '\\' {
            let Some(escaped) = source.next() else {
                return Err(ignore_error(path, line, "backslash escape"));
            };
            if escaped != ' ' {
                return Err(ignore_error(path, line, "backslash escape"));
            }
            chars.push((escaped, true));
        } else {
            chars.push((value, false));
        }
    }
    while chars
        .last()
        .is_some_and(|(value, escaped)| *value == ' ' && !escaped)
    {
        chars.pop();
    }
    let bytes = chars.iter().try_fold(0_usize, |total, (value, _)| {
        total
            .checked_add(value.len_utf8())
            .ok_or(IndexError::IgnoreLimit("compiled byte"))
    })?;
    let mut parsed = String::new();
    parsed
        .try_reserve_exact(bytes)
        .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
    parsed.extend(chars.into_iter().map(|(value, _)| value));
    Ok(Some((parsed, negated)))
}

fn copy_parts(parts: &[&str]) -> Result<Vec<String>, IndexError> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(parts.len())
        .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
    for part in parts {
        let mut value = String::new();
        value
            .try_reserve_exact(part.len())
            .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
        value.push_str(part);
        copied.push(value);
    }
    Ok(copied)
}

fn copy_pattern(pattern: &str, component_count: usize) -> Result<Vec<String>, IndexError> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(component_count)
        .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
    for part in pattern.split('/') {
        let mut value = String::new();
        value
            .try_reserve_exact(part.len())
            .map_err(|_| IndexError::IgnoreLimit("allocation"))?;
        value.push_str(part);
        copied.push(value);
    }
    Ok(copied)
}

fn charge_compiled(
    total: &mut usize,
    bytes: usize,
    options: &IndexOptions,
) -> Result<(), IndexError> {
    *total = total
        .checked_add(bytes)
        .filter(|total| *total <= options.max_compiled_ignore_bytes)
        .ok_or(IndexError::IgnoreLimit("compiled byte"))?;
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<(), IndexError> {
    if Instant::now() >= deadline {
        Err(IndexError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn ignore_error(path: &Path, line: usize, reason: &'static str) -> IndexError {
    IndexError::InvalidIgnore {
        path: path.to_owned(),
        line: line + 1,
        reason,
    }
}

fn glob_path(pattern: &[String], path: &[&str], deadline: Instant) -> Result<bool, IndexError> {
    let (mut pattern_at, mut path_at, mut globstar, mut retry) = (0, 0, None, 0);
    while path_at < path.len() {
        check_deadline(deadline)?;
        if pattern_at < pattern.len() && pattern[pattern_at] == "**" {
            globstar = Some(pattern_at);
            pattern_at += 1;
            retry = path_at;
        } else if pattern_at < pattern.len()
            && segment_match(&pattern[pattern_at], path[path_at], deadline)?
        {
            pattern_at += 1;
            path_at += 1;
        } else if let Some(globstar_at) = globstar {
            pattern_at = globstar_at + 1;
            retry += 1;
            path_at = retry;
        } else {
            return Ok(false);
        }
    }
    while pattern_at < pattern.len() && pattern[pattern_at] == "**" {
        pattern_at += 1;
    }
    Ok(pattern_at == pattern.len())
}

fn segment_match(pattern: &str, value: &str, deadline: Instant) -> Result<bool, IndexError> {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_at, mut value_at, mut star, mut retry) = (0, 0, None, 0);
    while value_at < value.len() {
        check_deadline(deadline)?;
        if pattern_at < pattern.len()
            && (pattern[pattern_at] == b'?' || pattern[pattern_at] == value[value_at])
        {
            pattern_at += 1;
            value_at += 1;
        } else if pattern_at < pattern.len() && pattern[pattern_at] == b'*' {
            star = Some(pattern_at);
            pattern_at += 1;
            retry = value_at;
        } else if let Some(star_at) = star {
            pattern_at = star_at + 1;
            retry += 1;
            value_at = retry;
        } else {
            return Ok(false);
        }
    }
    while pattern_at < pattern.len() && pattern[pattern_at] == b'*' {
        pattern_at += 1;
    }
    Ok(pattern_at == pattern.len())
}

fn digest_options(options: &IndexOptions, deadline: Instant) -> Result<[u8; 32], IndexError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-metadata-index-options-v1\0");
    for value in [
        options.max_entries as u128,
        options.max_indexed_bytes as u128,
        options.max_file_bytes as u128,
        options.max_ignore_files as u128,
        options.max_ignore_bytes as u128,
        options.max_ignore_rules as u128,
        options.max_compiled_ignore_bytes as u128,
        options.max_pattern_bytes as u128,
        options.max_pattern_components as u128,
        options.max_matcher_work_bytes as u128,
        options.max_symbols_per_file as u128,
        options.max_symbol_bytes as u128,
        options.max_build_time.as_nanos(),
    ] {
        check_deadline(deadline)?;
        hash.update(&value.to_le_bytes());
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

fn digest_index(
    snapshot: &Snapshot,
    entries: &[MetadataEntry],
    options: [u8; 32],
    truncated: bool,
    deadline: Instant,
) -> Result<[u8; 32], IndexError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-metadata-index-v1\0");
    hash.update(snapshot.revision().digest().as_str().as_bytes());
    hash.update(&options);
    hash.update(&[u8::from(truncated)]);
    for entry in entries {
        check_deadline(deadline)?;
        hash.update(&(entry.path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
        hash.update(entry.path.as_os_str().as_encoded_bytes());
        hash.update(&(entry.size).to_le_bytes());
        hash.update(&[
            entry.kind as u8,
            entry.content_state as u8,
            u8::from(entry.executable),
        ]);
        if let Some(text) = &entry.text {
            hash.update(blake3::hash(text.as_bytes()).as_bytes());
        }
    }
    check_deadline(deadline)?;
    Ok(*hash.finalize().as_bytes())
}

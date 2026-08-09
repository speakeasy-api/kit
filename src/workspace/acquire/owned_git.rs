//! Owned Git-metadata readers for the untrusted copy-on-write acquisition
//! path. Everything here operates on the private snapshot copy using plain
//! filesystem reads: no `git` process is ever spawned. The snapshot copy has
//! already rejected symlinks, hardlinks, non-regular entries, and filesystem
//! boundaries, so every entry below the snapshot root is a regular file or a
//! directory.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};

use super::{AcquisitionError, io_error, remove_snapshot_entry, validated_git_path};
use crate::workspace::index::meta::{IgnoreRules, IgnoreSource, IndexError, IndexOptions};
use crate::workspace::revision::EntryKind;

/// HEAD and loose ref files are tiny; anything larger is not a usable ref.
const MAX_REF_FILE_BYTES: u64 = 4096;
/// Symbolic-ref indirection bound (`ref: ...` chains).
const MAX_REF_DEPTH: usize = 5;

fn unsupported(path: &Path, reason: &'static str) -> AcquisitionError {
    AcquisitionError::UnsupportedIndexState {
        path: path.to_path_buf(),
        reason,
    }
}

/// Resolves the snapshot's HEAD to a commit id without spawning git.
///
/// Returns `Ok(None)` whenever git's own `rev-parse --verify HEAD^{commit}`
/// would have failed for reasons the caller treats as "no usable history"
/// (unborn branch, garbage HEAD contents, unresolvable or unsafe ref names):
/// the caller then materializes the Git-free untracked snapshot, exactly as
/// the git-error path did before. Object existence is not verified — the
/// snapshot's ref store is the source of truth for the base commit identity.
pub(super) fn resolve_head(repository: &Path) -> Result<Option<String>, AcquisitionError> {
    let git_dir = repository.join(".git");
    match read_ref_file(&git_dir.join("HEAD"))? {
        RefFile::Content(content) => resolve_ref_value(&git_dir, &content, 0),
        RefFile::Missing | RefFile::Unusable => Ok(None),
    }
}

enum RefFile {
    Missing,
    Unusable,
    Content(String),
}

fn read_ref_file(path: &Path) -> Result<RefFile, AcquisitionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RefFile::Missing),
        Err(source) => return Err(io_error("inspect snapshot Git reference", source)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_REF_FILE_BYTES
    {
        return Ok(RefFile::Unusable);
    }
    let bytes = fs::read(path).map_err(|source| io_error("read snapshot Git reference", source))?;
    if bytes.len() as u64 > MAX_REF_FILE_BYTES {
        return Ok(RefFile::Unusable);
    }
    Ok(String::from_utf8(bytes).map_or(RefFile::Unusable, RefFile::Content))
}

fn resolve_ref_value(
    git_dir: &Path,
    content: &str,
    depth: usize,
) -> Result<Option<String>, AcquisitionError> {
    if depth > MAX_REF_DEPTH {
        return Ok(None);
    }
    let content = content.trim_end_matches(['\r', '\n']);
    if let Some(name) = content.strip_prefix("ref: ") {
        let name = name.trim();
        if !safe_ref_name(name) {
            return Ok(None);
        }
        return match read_ref_file(&git_dir.join(name))? {
            RefFile::Content(loose) => resolve_ref_value(git_dir, &loose, depth + 1),
            RefFile::Missing => packed_ref(git_dir, name),
            RefFile::Unusable => Ok(None),
        };
    }
    Ok(commit_oid(content))
}

/// Accepts 40-hex (SHA-1) and 64-hex (SHA-256) lowercase object ids.
fn commit_oid(value: &str) -> Option<String> {
    (matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| value.to_owned())
}

/// A conservative safety filter for ref names resolved through the snapshot:
/// names must live under `refs/` and contain only ordinary path components.
/// Unusual-but-valid git ref names that fail this filter fall back to the
/// untracked branch, which is the safe direction.
fn safe_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && name.len() <= 4096
        && !name.ends_with('/')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'+' | b'@')
        })
}

/// Looks a ref up in `.git/packed-refs`: `# ...` header lines and `^...`
/// peeled lines are skipped; entry lines are `<hex-oid> <refname>`.
fn packed_ref(git_dir: &Path, name: &str) -> Result<Option<String>, AcquisitionError> {
    let path = git_dir.join("packed-refs");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("inspect snapshot packed refs", source)),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > super::MAX_COMMAND_OUTPUT as u64
    {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|source| io_error("read snapshot packed refs", source))?;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' || line[0] == b'^' {
            continue;
        }
        let Some(separator) = line.iter().position(|byte| *byte == b' ') else {
            continue;
        };
        if &line[separator + 1..] == name.as_bytes() {
            let Ok(oid) = std::str::from_utf8(&line[..separator]) else {
                return Ok(None);
            };
            return Ok(commit_oid(oid));
        }
    }
    Ok(None)
}

/// One structurally parsed index entry; validation happens in a second pass
/// so that a split index (whose replaced entries carry empty names) is
/// reported as "split index" rather than as a malformed file.
struct RawIndexEntry {
    path: Vec<u8>,
    mode: u32,
    stage: u8,
    intent_to_add: bool,
    skip_worktree: bool,
    assume_unchanged: bool,
}

struct ParsedIndex {
    entries: Vec<RawIndexEntry>,
    split_index: bool,
}

/// Reads and validates the snapshot's `.git/index` without spawning git,
/// preserving the typed rejections the git-based validators produced:
/// split index, skip-worktree, assume-unchanged, unresolved merges,
/// intent-to-add, gitlinks, unsupported modes, and tracked paths whose
/// worktree entry is a directory. Returns the set of tracked paths.
///
/// `oid_bytes` is the object-id width implied by the resolved base commit
/// (20 for SHA-1, 32 for SHA-256 repositories).
pub(super) fn read_validated_index(
    repository: &Path,
    oid_bytes: usize,
) -> Result<BTreeSet<PathBuf>, AcquisitionError> {
    let index_path = repository.join(".git").join("index");
    let metadata = match fs::symlink_metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(source) => return Err(io_error("inspect snapshot Git index", source)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(unsupported(&index_path, "irregular index file"));
    }
    if metadata.len() > super::MAX_COMMAND_OUTPUT as u64 {
        return Err(AcquisitionError::OutputTooLarge("read snapshot Git index"));
    }
    let bytes = fs::read(&index_path).map_err(|source| io_error("read snapshot Git index", source))?;
    let parsed = parse_index(&bytes, oid_bytes, &index_path)?;
    if parsed.split_index {
        return Err(unsupported(&index_path, "split index"));
    }
    let mut tracked = BTreeSet::new();
    for entry in &parsed.entries {
        let relative = validated_git_path(&entry.path, "validate index")?;
        if entry.mode == 0o160000 {
            return Err(AcquisitionError::UnsupportedGitlink(relative));
        }
        if !matches!(entry.mode, 0o100644 | 0o100755 | 0o120000) {
            return Err(unsupported(&relative, "unsupported entry mode"));
        }
        if entry.stage != 0 {
            return Err(unsupported(&relative, "unresolved merge entry"));
        }
        if entry.intent_to_add {
            return Err(unsupported(&relative, "intent-to-add entry"));
        }
        if entry.skip_worktree {
            return Err(unsupported(&relative, "skip-worktree entry"));
        }
        if entry.assume_unchanged {
            return Err(unsupported(&relative, "assume-unchanged entry"));
        }
        tracked.insert(relative);
    }
    Ok(tracked)
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Structural parse of index format v2/v3/v4 (header, entries, extensions).
/// The trailing checksum width matches the repository's object format.
fn parse_index(
    bytes: &[u8],
    oid_bytes: usize,
    index_path: &Path,
) -> Result<ParsedIndex, AcquisitionError> {
    let malformed = || unsupported(index_path, "malformed index");
    let end = bytes
        .len()
        .checked_sub(oid_bytes)
        .filter(|end| *end >= 12)
        .ok_or_else(malformed)?;
    if &bytes[..4] != b"DIRC" {
        return Err(malformed());
    }
    let version = be32(&bytes[4..8]);
    if !matches!(version, 2 | 3 | 4) {
        return Err(unsupported(index_path, "unsupported index version"));
    }
    let count = be32(&bytes[8..12]) as usize;
    if count > super::MAX_SNAPSHOT_ENTRIES {
        return Err(AcquisitionError::SnapshotLimitExceeded);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve(count.min(bytes.len() / 32))
        .map_err(|_| AcquisitionError::SnapshotLimitExceeded)?;
    let mut cursor = 12_usize;
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..count {
        let mut fixed = 40 + oid_bytes + 2;
        if cursor.checked_add(fixed).is_none_or(|next| next > end) {
            return Err(malformed());
        }
        let mode = be32(&bytes[cursor + 24..cursor + 28]);
        let oid = &bytes[cursor + 40..cursor + 40 + oid_bytes];
        let flags = be16(&bytes[cursor + 40 + oid_bytes..cursor + fixed]);
        let assume_unchanged = flags & 0x8000 != 0;
        let extended = flags & 0x4000 != 0;
        let stage = ((flags >> 12) & 0x3) as u8;
        let mut skip_worktree = false;
        // git records intent-to-add entries with a zeroed object id; the
        // v3+ extended flag below marks them explicitly.
        let mut intent_to_add = oid.iter().all(|byte| *byte == 0);
        if extended {
            if version == 2 || cursor + fixed + 2 > end {
                return Err(malformed());
            }
            let extra = be16(&bytes[cursor + fixed..cursor + fixed + 2]);
            skip_worktree = extra & 0x4000 != 0;
            intent_to_add |= extra & 0x2000 != 0;
            fixed += 2;
        }
        let name_start = cursor + fixed;
        let terminator = bytes[name_start..end]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| name_start + offset)
            .ok_or_else(malformed)?;
        let path = if version == 4 {
            // v4 prefix-compresses names: a varint strip count against the
            // previous entry's name, then the NUL-terminated suffix; entries
            // are not padded.
            let (strip, after_varint) = read_varint(bytes, name_start, end).ok_or_else(malformed)?;
            let suffix_end = bytes[after_varint..end]
                .iter()
                .position(|byte| *byte == 0)
                .map(|offset| after_varint + offset)
                .ok_or_else(malformed)?;
            let keep = previous.len().checked_sub(strip).ok_or_else(malformed)?;
            let mut assembled = Vec::new();
            assembled
                .try_reserve(keep + (suffix_end - after_varint))
                .map_err(|_| AcquisitionError::SnapshotLimitExceeded)?;
            assembled.extend_from_slice(&previous[..keep]);
            assembled.extend_from_slice(&bytes[after_varint..suffix_end]);
            cursor = suffix_end + 1;
            assembled
        } else {
            let path = bytes[name_start..terminator].to_vec();
            // v2/v3 entries are NUL-padded so that the total entry length,
            // measured from the entry start, is a multiple of eight bytes
            // with at least one NUL after the name.
            let unpadded = fixed + (terminator - name_start);
            let padded = (unpadded + 8) & !7;
            cursor = cursor.checked_add(padded).ok_or_else(malformed)?;
            if cursor > end {
                return Err(malformed());
            }
            path
        };
        previous.clear();
        previous.extend_from_slice(&path);
        entries.push(RawIndexEntry {
            path,
            mode,
            stage,
            intent_to_add,
            skip_worktree,
            assume_unchanged,
        });
    }
    // Extensions: [4-byte signature][4-byte big-endian length][payload]. The
    // "link" extension marks a split index, which the acquisition rejects.
    let mut split_index = false;
    let mut at = cursor;
    while at < end {
        if at + 8 > end {
            return Err(malformed());
        }
        let signature = &bytes[at..at + 4];
        let size = be32(&bytes[at + 4..at + 8]) as usize;
        let payload_end = at
            .checked_add(8)
            .and_then(|value| value.checked_add(size))
            .filter(|payload_end| *payload_end <= end)
            .ok_or_else(malformed)?;
        if signature == b"link" {
            split_index = true;
        }
        at = payload_end;
    }
    Ok(ParsedIndex {
        entries,
        split_index,
    })
}

/// git's offset-style varint (varint.c `decode_varint`): 7 value bits per
/// byte, MSB continuation, with an implicit +1 per continuation byte.
fn read_varint(bytes: &[u8], mut at: usize, end: usize) -> Option<(usize, usize)> {
    if at >= end {
        return None;
    }
    let mut byte = bytes[at];
    at += 1;
    let mut value = usize::from(byte & 127);
    while byte & 128 != 0 {
        if at >= end {
            return None;
        }
        value = value.checked_add(1)?;
        byte = bytes[at];
        at += 1;
        value = value.checked_mul(128)?.checked_add(usize::from(byte & 127))?;
    }
    Some((value, at))
}

/// Equivalent of the git-based `validate_snapshot_entries`: a tracked path
/// whose snapshot entry is a directory is unsupported. Symlinks are valid
/// tracked entries (mode 120000) and were materialized as inert links by the
/// snapshot copy; other non-regular kinds still cannot occur.
pub(super) fn validate_tracked_snapshot_entries(
    repository: &Path,
    tracked: &BTreeSet<PathBuf>,
) -> Result<(), AcquisitionError> {
    let mut verified_directories = BTreeSet::new();
    for relative in tracked {
        // A tracked path must never resolve through a symlinked ancestor:
        // the snapshot materializes symlinks as inert entries, and a
        // follow here would reach outside the copied tree.
        let mut ancestors_present = true;
        if let Some(parent) = relative.parent() {
            let mut ancestor = PathBuf::new();
            for component in parent.components() {
                ancestor.push(component);
                if verified_directories.contains(&ancestor) {
                    continue;
                }
                let path = repository.join(&ancestor);
                match fs::symlink_metadata(&path) {
                    Ok(metadata)
                        if metadata.is_dir() && !metadata.file_type().is_symlink() =>
                    {
                        verified_directories.insert(ancestor.clone());
                    }
                    Ok(_) => {
                        return Err(AcquisitionError::SymlinkPath {
                            kind: "snapshot source",
                            path,
                        });
                    }
                    // A deleted subtree is legitimate dirty state.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        ancestors_present = false;
                        break;
                    }
                    Err(source) => return Err(io_error("inspect snapshot path", source)),
                }
            }
        }
        if !ancestors_present {
            continue;
        }
        let path = repository.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(AcquisitionError::UnsupportedSourceEntry(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspect snapshot path", source)),
        }
    }
    Ok(())
}

/// Removes ignored, untracked entries from the snapshot using the owned
/// ignore engine, mirroring
/// `git ls-files --others --ignored --exclude-standard --directory`:
///
/// - `.gitignore` files are collected parent-first across the snapshot, with
///   `.git/info/exclude` participating at the lowest precedence;
/// - `core.excludesFile` (user-global excludes) is deliberately NOT honored:
///   the sanitized snapshot must not depend on operator-level configuration,
///   and the failure direction is safe (extra files stay in the snapshot,
///   nothing is wrongly excluded);
/// - ignore lines whose syntax the owned matcher does not support are
///   skipped (files stay included) instead of failing the acquisition;
/// - fully ignored directories with no tracked content are pruned whole;
///   ignored directories sheltering tracked files are descended, where every
///   untracked file is removed (git permits no re-inclusion below an
///   excluded directory);
/// - nested repositories (directories containing `.git`) are boundaries:
///   removed whole when ignored and untracked, otherwise left untouched.
pub(super) fn remove_ignored_snapshot_entries(
    repository: &Path,
    tracked: &BTreeSet<PathBuf>,
) -> Result<(), AcquisitionError> {
    let options = IndexOptions::default();
    let deadline = Instant::now()
        .checked_add(super::GIT_TIMEOUT)
        .ok_or(AcquisitionError::CommandTimedOut("evaluate ignore rules"))?;
    let mut sources = Vec::new();
    collect_ignore_source(
        &mut sources,
        Vec::new(),
        PathBuf::from(".git/info/exclude"),
        &repository.join(".git").join("info").join("exclude"),
        &options,
    )?;
    collect_gitignore_sources(repository, Path::new(""), &mut sources, &options)?;
    let rules = IgnoreRules::compile_sources(sources, &options, deadline, true)
        .map_err(map_ignore_error)?;
    remove_ignored_walk(repository, Path::new(""), &rules, tracked, false, deadline)
}

/// Computes, from the SOURCE tree, the set of entries that
/// `remove_ignored_snapshot_entries` would delete from the snapshot, so the
/// copy can skip them entirely. Without this, ignored build directories abort
/// acquisition before ignore rules ever apply: cargo hardlinks objects inside
/// `target/`, and the copy walk rejects hardlinked (and symlinked) entries it
/// was never going to keep.
///
/// The prune set is advisory: it is computed from the live source, so a
/// concurrent mutation can make it stale. The post-copy passes
/// (`validate_tracked_snapshot_entries`, `remove_ignored_snapshot_entries`)
/// remain the authority on the snapshot's final content, and the walk below
/// mirrors their semantics exactly — same ignore sources, same tracked-file
/// protection, same nested-repository boundaries. When the source has no
/// resolvable HEAD or a readable index cannot be established, nothing is
/// pruned and the copy behaves as before.
pub(super) fn compute_source_prune_set(
    source: &Path,
) -> Result<BTreeSet<PathBuf>, AcquisitionError> {
    let Ok(Some(base_commit)) = resolve_head(source) else {
        return Ok(BTreeSet::new());
    };
    let Ok(tracked) = read_validated_index(source, base_commit.len() / 2) else {
        return Ok(BTreeSet::new());
    };
    let options = IndexOptions::default();
    let deadline = Instant::now()
        .checked_add(super::GIT_TIMEOUT)
        .ok_or(AcquisitionError::CommandTimedOut("evaluate ignore rules"))?;
    let mut sources = Vec::new();
    collect_ignore_source(
        &mut sources,
        Vec::new(),
        PathBuf::from(".git/info/exclude"),
        &source.join(".git").join("info").join("exclude"),
        &options,
    )?;
    collect_gitignore_sources(source, Path::new(""), &mut sources, &options)?;
    let rules = IgnoreRules::compile_sources(sources, &options, deadline, true)
        .map_err(map_ignore_error)?;
    let mut prune = BTreeSet::new();
    prune_walk(
        source,
        Path::new(""),
        &rules,
        &tracked,
        false,
        deadline,
        &mut prune,
    )?;
    Ok(prune)
}

/// Source-side twin of `remove_ignored_walk`: collects prunable paths instead
/// of deleting them. The one extra case is non-regular entries (symlinks,
/// sockets): they cannot occur in a snapshot but do occur in sources, and an
/// ignored untracked one must be pruned or the copy walk rejects it.
fn prune_walk(
    root: &Path,
    relative: &Path,
    rules: &IgnoreRules,
    tracked: &BTreeSet<PathBuf>,
    ancestor_ignored: bool,
    deadline: Instant,
    prune: &mut BTreeSet<PathBuf>,
) -> Result<(), AcquisitionError> {
    let absolute = root.join(relative);
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&absolute).map_err(|source| io_error("read source directory", source))?
    {
        let entry = entry.map_err(|source| io_error("read source directory entry", source))?;
        entries.push(entry.file_name());
    }
    entries.sort();
    for name in entries {
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let child = relative.join(&name);
        let path = root.join(&child);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error("inspect source directory entry", source)),
        };
        if metadata.is_dir() {
            let ignored =
                ancestor_ignored || matches_ignored(rules, &child, EntryKind::Directory, deadline)?;
            if ignored && !tracked_under(tracked, &child) {
                prune.insert(child);
            } else if path.join(".git").symlink_metadata().is_err() {
                prune_walk(root, &child, rules, tracked, ignored, deadline, prune)?;
            }
        } else {
            if tracked.contains(&child) {
                continue;
            }
            let ignored =
                ancestor_ignored || matches_ignored(rules, &child, EntryKind::File, deadline)?;
            if ignored {
                prune.insert(child);
            }
        }
    }
    Ok(())
}

fn map_ignore_error(error: IndexError) -> AcquisitionError {
    match error {
        IndexError::DeadlineExceeded => AcquisitionError::CommandTimedOut("evaluate ignore rules"),
        IndexError::IgnoreLimit(_) => AcquisitionError::SnapshotLimitExceeded,
        _ => AcquisitionError::Unavailable {
            capability: "owned ignore evaluation",
        },
    }
}

/// Reads one candidate ignore file into `sources`. Missing files are fine;
/// non-UTF-8 files are skipped (their rules cannot match, so their files stay
/// included — the safe direction).
fn collect_ignore_source(
    sources: &mut Vec<IgnoreSource>,
    base: Vec<String>,
    display_path: PathBuf,
    path: &Path,
    options: &IndexOptions,
) -> Result<(), AcquisitionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error("inspect snapshot ignore file", source)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.len() > options.max_ignore_bytes as u64 {
        return Err(AcquisitionError::SnapshotLimitExceeded);
    }
    if sources.len() == options.max_ignore_files {
        return Err(AcquisitionError::SnapshotLimitExceeded);
    }
    let bytes = fs::read(path).map_err(|source| io_error("read snapshot ignore file", source))?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(());
    };
    sources.push(IgnoreSource {
        base,
        path: display_path,
        text,
    });
    Ok(())
}

/// Collects `.gitignore` files parent-first (each directory's file before its
/// children's), skipping the top-level `.git` directory and never descending
/// into nested repositories.
fn collect_gitignore_sources(
    root: &Path,
    relative: &Path,
    sources: &mut Vec<IgnoreSource>,
    options: &IndexOptions,
) -> Result<(), AcquisitionError> {
    let absolute = root.join(relative);
    let Some(base) = utf8_components(relative) else {
        // A non-UTF-8 directory name cannot participate in the owned
        // matcher; its subtree keeps all files (safe direction).
        return Ok(());
    };
    collect_ignore_source(
        sources,
        base,
        relative.join(".gitignore"),
        &absolute.join(".gitignore"),
        options,
    )?;
    let mut children = Vec::new();
    for entry in
        fs::read_dir(&absolute).map_err(|source| io_error("read snapshot directory", source))?
    {
        let entry = entry.map_err(|source| io_error("read snapshot directory entry", source))?;
        let name = entry.file_name();
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("inspect snapshot directory entry", source))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            children.push(name);
        }
    }
    children.sort();
    for name in children {
        let child = relative.join(&name);
        if root.join(&child).join(".git").symlink_metadata().is_ok() {
            continue;
        }
        collect_gitignore_sources(root, &child, sources, options)?;
    }
    Ok(())
}

fn utf8_components(path: &Path) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    for component in path.components() {
        parts.push(component.as_os_str().to_str()?.to_owned());
    }
    Some(parts)
}

fn matches_ignored(
    rules: &IgnoreRules,
    path: &Path,
    kind: EntryKind,
    deadline: Instant,
) -> Result<bool, AcquisitionError> {
    match rules.ignored(path, kind, deadline) {
        Ok(ignored) => Ok(ignored),
        // The matcher cannot express non-UTF-8 paths; keep such entries.
        Err(IndexError::NonUtf8Path(_)) => Ok(false),
        Err(error) => Err(map_ignore_error(error)),
    }
}

fn tracked_under(tracked: &BTreeSet<PathBuf>, directory: &Path) -> bool {
    tracked
        .range(directory.to_path_buf()..)
        .next()
        .is_some_and(|path| path.starts_with(directory))
}

fn remove_ignored_walk(
    root: &Path,
    relative: &Path,
    rules: &IgnoreRules,
    tracked: &BTreeSet<PathBuf>,
    ancestor_ignored: bool,
    deadline: Instant,
) -> Result<(), AcquisitionError> {
    let absolute = root.join(relative);
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&absolute).map_err(|source| io_error("read snapshot directory", source))?
    {
        let entry = entry.map_err(|source| io_error("read snapshot directory entry", source))?;
        entries.push(entry.file_name());
    }
    entries.sort();
    for name in entries {
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let child = relative.join(&name);
        let path = root.join(&child);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error("inspect snapshot directory entry", source)),
        };
        if metadata.is_dir() {
            let ignored =
                ancestor_ignored || matches_ignored(rules, &child, EntryKind::Directory, deadline)?;
            if ignored && !tracked_under(tracked, &child) {
                remove_snapshot_entry(&path)?;
            } else if path.join(".git").symlink_metadata().is_err() {
                remove_ignored_walk(root, &child, rules, tracked, ignored, deadline)?;
            }
            // Nested repositories that are not fully ignored are opaque
            // boundaries, exactly as git treats them.
        } else {
            // Files and symlinks share ignore semantics; symlinks are
            // materialized as inert entries by the snapshot copy.
            if tracked.contains(&child) {
                continue;
            }
            let ignored =
                ancestor_ignored || matches_ignored(rules, &child, EntryKind::File, deadline)?;
            if ignored {
                remove_snapshot_entry(&path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_repository(prefix: &str) -> PathBuf {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("{prefix}-{}", super::super::hex(&random)));
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        root
    }

    #[test]
    fn head_resolves_through_loose_refs() {
        let root = temporary_repository("kit-owned-loose");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            root.join(".git/refs/heads/main"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            resolve_head(&root).unwrap().as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn head_resolves_through_packed_refs_with_header_and_peeled_lines() {
        let root = temporary_repository("kit-owned-packed");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            root.join(".git/packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \n\
             1111111111111111111111111111111111111111 refs/tags/v1\n\
             ^2222222222222222222222222222222222222222\n\
             3333333333333333333333333333333333333333 refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(
            resolve_head(&root).unwrap().as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loose_ref_shadows_packed_ref() {
        let root = temporary_repository("kit-owned-shadow");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            root.join(".git/packed-refs"),
            "1111111111111111111111111111111111111111 refs/heads/main\n",
        )
        .unwrap();
        fs::write(
            root.join(".git/refs/heads/main"),
            "2222222222222222222222222222222222222222\n",
        )
        .unwrap();
        assert_eq!(
            resolve_head(&root).unwrap().as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detached_head_resolves_directly_for_both_object_formats() {
        let root = temporary_repository("kit-owned-detached");
        fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            resolve_head(&root).unwrap().as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        let sha256 = "a".repeat(64);
        fs::write(root.join(".git/HEAD"), format!("{sha256}\n")).unwrap();
        assert_eq!(resolve_head(&root).unwrap().as_deref(), Some(sha256.as_str()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unborn_and_garbage_heads_resolve_to_none() {
        let root = temporary_repository("kit-owned-unborn");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(resolve_head(&root).unwrap(), None);
        fs::write(root.join(".git/HEAD"), "not a reference at all\n").unwrap();
        assert_eq!(resolve_head(&root).unwrap(), None);
        fs::write(root.join(".git/HEAD"), "ref: ../../escape\n").unwrap();
        assert_eq!(resolve_head(&root).unwrap(), None);
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/../../../escape\n").unwrap();
        assert_eq!(resolve_head(&root).unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symbolic_ref_chains_are_depth_bounded() {
        let root = temporary_repository("kit-owned-chain");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/a\n").unwrap();
        fs::write(root.join(".git/refs/heads/a"), "ref: refs/heads/b\n").unwrap();
        fs::write(
            root.join(".git/refs/heads/b"),
            "4444444444444444444444444444444444444444\n",
        )
        .unwrap();
        assert_eq!(
            resolve_head(&root).unwrap().as_deref(),
            Some("4444444444444444444444444444444444444444")
        );
        fs::write(root.join(".git/refs/heads/a"), "ref: refs/heads/a\n").unwrap();
        assert_eq!(resolve_head(&root).unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn varint_decoding_matches_git_offset_encoding() {
        assert_eq!(read_varint(&[0x00], 0, 1), Some((0, 1)));
        assert_eq!(read_varint(&[0x7f], 0, 1), Some((127, 1)));
        // 0x80 0x00 encodes (0+1)*128 + 0 = 128 in git's offset varint.
        assert_eq!(read_varint(&[0x80, 0x00], 0, 2), Some((128, 2)));
        assert_eq!(read_varint(&[0x80], 0, 1), None);
    }
}

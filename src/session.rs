//! Durable, append-only session transcripts and their filesystem lock.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agentkit_core::{Item, ItemKind, Part, Timestamp};
use agentkit_loop::{TranscriptEvent, TranscriptObserver};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 3;
const REDIRECT_SCHEMA_VERSION: u32 = 4;
const PREVIOUS_SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
const MAX_RFC3339_MILLIS: u64 = 253_402_300_799_999;
pub(crate) const SESSION_ORIGIN_METADATA_KEY: &str = "dev.kit.session.origin";
pub(crate) const SUBAGENT_SESSION_ORIGIN: &str = "subagent";
pub(crate) const TOP_LEVEL_SESSION_ORIGIN: &str = "top_level";
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    session_id: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replacement: Option<Vec<Item>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redirect: Option<PathBuf>,
}

/// A loaded transcript together with the observer that owns its mutation lock.
pub struct OpenSession {
    pub transcript: Vec<Item>,
    pub observer: SessionObserver,
}

/// Read-only metadata for one durable session in a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub is_subagent: bool,
    /// Last activity as milliseconds since the Unix epoch.
    pub updated_at: u64,
}

impl CatalogEntry {
    /// Formats the last activity as an RFC 3339 UTC timestamp.
    #[must_use]
    pub fn updated_at_rfc3339(&self) -> String {
        timestamp_rfc3339(self.updated_at)
    }
}

#[derive(Clone)]
pub struct SessionObserver(Arc<Mutex<Writer>>);

struct Writer {
    session_id: String,
    generation: u64,
    path: PathBuf,
    workspace_root: PathBuf,
    file: File,
    lock: SessionLock,
}

struct SessionLock {
    path: PathBuf,
    token: String,
    // Retaining the OS lock closes the token check/write race. The option lets
    // Drop close the handle before removing the path on Windows.
    file: Option<File>,
}

/// Removes an incompletely bootstrapped new transcript unless opening commits.
struct CreatedTranscript {
    path: PathBuf,
    keep: bool,
}

impl CreatedTranscript {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for CreatedTranscript {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Generates a path-safe id that remains unique across Kit processes.
#[must_use]
pub fn new_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    format!(
        "s-{millis}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// Loads a transcript without taking mutating authority.
///
/// A file that lost a tool result is repaired in memory only; writing the
/// repair back is reserved for [`open`], which holds the transcript's lock.
pub fn load(root: &Path, session_id: &str) -> Result<Vec<Item>, String> {
    load_in(root, &default_directory()?, session_id)
}

pub(crate) fn load_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
) -> Result<Vec<Item>, String> {
    validate_id(session_id)?;
    let workspace_root = canonical_workspace(root);
    let mut items = select_authority(directory, &workspace_root, session_id)?
        .ok_or_else(|| format!("session {session_id:?} does not exist"))?
        .items;
    crate::transcript::repair_unanswered_tool_calls(&mut items);
    Ok(items)
}

/// Copies a completed transcript into a new durable session.
///
/// The source remains owned by its ACP child; callers serialize this operation
/// with prompts so the read is a stable, completed-turn snapshot.
pub fn clone_completed(root: &Path, source: &str, destination: &str) -> Result<(), String> {
    clone_completed_in(root, &default_directory()?, source, destination)
}

pub(crate) fn clone_completed_in(
    root: &Path,
    directory: &Path,
    source: &str,
    destination: &str,
) -> Result<(), String> {
    let transcript = load_in(root, directory, source)?;
    let opened = open_with_initial_timestamps_in(
        root,
        directory,
        destination,
        false,
        false,
        transcript,
        false,
    )?;
    drop(opened);
    Ok(())
}

/// Removes an abandoned lock file, but never one held by a live process.
///
/// This is the last-resort cleanup path for a hosting client whose server had
/// to be killed before normal `SessionLock` destruction completed.
pub fn remove_stale_lock(root: &Path, session_id: &str) -> Result<(), String> {
    remove_stale_lock_in(root, &default_directory()?, session_id)
}

pub(crate) fn remove_stale_lock_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
) -> Result<(), String> {
    validate_id(session_id)?;
    let workspace_root = canonical_workspace(root);
    let scoped = lock_path(
        &workspace_storage_directory(directory, &workspace_root),
        session_id,
    );
    let path = if scoped
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", scoped.display()))?
    {
        scoped
    } else if let Some(transcript) =
        legacy_transcript_for_workspace(directory, &workspace_root, session_id)?
    {
        transcript.with_extension("lock")
    } else {
        return Ok(());
    };
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not inspect session lock: {error}")),
    };
    file.try_lock()
        .map_err(|_| "session lock is still held by a live Kit instance".to_string())?;
    drop(file);
    fs::remove_file(&path).map_err(|error| format!("could not remove stale session lock: {error}"))
}

/// Opens a new or resumed transcript and takes its mutation lock.
///
/// `resume` requires the transcript to exist. `force` fences an abandoned lock;
/// an older process checks the lock token before every append and can no longer
/// write after it has been replaced.
pub fn open(
    root: &Path,
    session_id: &str,
    resume: bool,
    force: bool,
    initial: Vec<Item>,
) -> Result<OpenSession, String> {
    open_in(
        root,
        &default_directory()?,
        session_id,
        resume,
        force,
        initial,
    )
}

pub(crate) fn open_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
    resume: bool,
    force: bool,
    initial: Vec<Item>,
) -> Result<OpenSession, String> {
    open_with_initial_timestamps_in(root, directory, session_id, resume, force, initial, true)
}

fn open_with_initial_timestamps_in(
    root: &Path,
    directory: &Path,
    session_id: &str,
    resume: bool,
    force: bool,
    initial: Vec<Item>,
    stamp_initial: bool,
) -> Result<OpenSession, String> {
    validate_id(session_id)?;
    if !resume && initial.is_empty() {
        return Err("a new session requires an initial transcript".into());
    }
    let workspace_root = canonical_workspace(root);
    let scoped_directory = workspace_storage_directory(directory, &workspace_root);
    fs::create_dir_all(&scoped_directory)
        .map_err(|error| format!("could not create session directory: {error}"))?;
    let path = transcript_path(&scoped_directory, session_id);
    let lock = SessionLock::acquire(lock_path(&scoped_directory, session_id), force)?;
    let _migration_locks = lock_migration_sources(directory, &workspace_root, session_id)?;
    recover_torn_migration_writes(&path, directory, &workspace_root, session_id)?;
    let authority = select_authority(directory, &workspace_root, session_id)?;
    if resume {
        let authority =
            authority.ok_or_else(|| format!("session {session_id:?} does not exist"))?;
        let title_seed = authority
            .historical_items
            .iter()
            .find_map(|items| first_user_item_index(items).map(|index| items[..=index].to_vec()));
        establish_scoped_authority(
            &path,
            session_id,
            &workspace_root,
            &authority.items,
            title_seed.as_deref(),
        )?;
        for legacy in authority.legacy_histories {
            redirect_legacy_transcript(&legacy, &path, session_id, &workspace_root)?;
        }
    } else if authority.is_some() {
        return Err(format!(
            "session {session_id:?} already exists; use --resume"
        ));
    }
    let (mut transcript, mut generation) = if resume {
        read_records(&path, session_id)?
    } else {
        (Vec::new(), 0)
    };
    let stored_workspace = resume
        .then(|| transcript_workspace(&path, session_id))
        .transpose()?
        .flatten();
    if let Some(stored) = &stored_workspace
        && stored != &workspace_root
    {
        return Err(format!(
            "session {session_id:?} belongs to workspace {}, not {}",
            stored.display(),
            workspace_root.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    if resume {
        options.create(false);
    } else {
        options.create_new(true);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut created = (!resume).then(|| CreatedTranscript::new(path.clone()));
    let mut writer = Writer {
        session_id: session_id.into(),
        generation,
        path,
        workspace_root,
        file,
        lock,
    };
    if resume && stored_workspace.is_none() {
        writer.replace(&transcript)?;
    }
    if !resume {
        for mut item in initial {
            if stamp_initial {
                stamp_item(&mut item, Timestamp::now());
                writer.append(&item)?;
            } else {
                writer.append_snapshot_item(&item)?;
            }
            transcript.push(item);
        }
        generation = writer.generation;
        debug_assert_eq!(generation, transcript.len() as u64);
    }
    // Nothing guards a transcript between sessions: it is a plain file a user
    // can edit, truncate, or lose a write from, and a tool call left unanswered
    // by any of that is history no provider will accept again. The repair is
    // written back here, under the lock this session just took, so the stored
    // transcript is sound rather than repaired anew on every read. Appending
    // keeps the file append-only; the in-memory copy carries each result next
    // to its own call.
    for item in crate::transcript::repair_unanswered_tool_calls(&mut transcript) {
        writer.append(&item)?;
    }
    if let Some(created) = created.take() {
        created.keep();
    }
    Ok(OpenSession {
        transcript,
        observer: SessionObserver(Arc::new(Mutex::new(writer))),
    })
}

impl SessionObserver {
    /// Durably records a complete transcript replacement produced by a mutator.
    /// Existing append records remain intact, while readers treat this record as
    /// a new canonical snapshot.
    pub fn replace(&self, transcript: &[Item]) -> Result<(), String> {
        if transcript.is_empty() {
            return Err("cannot persist an empty transcript replacement".into());
        }
        self.0
            .lock()
            .map_err(|_| "session transcript writer poisoned".to_string())?
            .replace(transcript)
    }
}

impl TranscriptObserver for SessionObserver {
    fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
        let mut writer = self.0.lock().expect("session transcript writer poisoned");
        if let Err(error) = writer.append(event.item) {
            // The loop invokes observers before committing the item in memory.
            // Refusing that mutation is safer than continuing with history that
            // was not durably recorded and cannot be resumed faithfully.
            panic!("session persistence failed: {error}");
        }
    }
}

impl Writer {
    fn append(&mut self, item: &Item) -> Result<(), String> {
        if item.created_at.is_none() {
            return Err("transcript item missing created_at before persistence".into());
        }
        self.append_snapshot_item(item)
    }

    fn append_snapshot_item(&mut self, item: &Item) -> Result<(), String> {
        self.ensure_lock()?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| "session generation overflowed".to_string())?;
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            generation,
            workspace_root: Some(self.workspace_root.clone()),
            item: Some(item.clone()),
            replacement: None,
            redirect: None,
        };
        self.write_record(record, generation)
    }

    fn replace(&mut self, transcript: &[Item]) -> Result<(), String> {
        self.ensure_lock()?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| "session generation overflowed".to_string())?;
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            generation,
            workspace_root: Some(self.workspace_root.clone()),
            item: None,
            replacement: Some(transcript.to_vec()),
            redirect: None,
        };
        self.write_record(record, generation)
    }

    fn write_record(&mut self, record: Record, generation: u64) -> Result<(), String> {
        // Encode before touching the append-only file, so serialization
        // failures can never leave a partial JSON record behind.
        let mut encoded = serde_json::to_vec(&record)
            .map_err(|error| format!("could not encode transcript record: {error}"))?;
        encoded.push(b'\n');
        self.file
            .write_all(&encoded)
            .and_then(|_| self.file.sync_data())
            .map_err(|error| format!("could not persist transcript record: {error}"))?;
        self.generation = generation;
        Ok(())
    }

    fn ensure_lock(&mut self) -> Result<(), String> {
        match self.lock.check() {
            Ok(()) => {
                if self.path.try_exists().map_err(|error| {
                    format!("could not inspect {}: {error}", self.path.display())
                })? {
                    Ok(())
                } else {
                    self.reconstruct()
                }
            }
            Err(LockError::Missing) => self.recover(),
            Err(error) => Err(error.to_string()),
        }
    }

    fn recover(&mut self) -> Result<(), String> {
        let directory = self
            .path
            .parent()
            .ok_or_else(|| "session transcript has no parent directory".to_string())?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("could not recreate session directory: {error}"))?;
        let lock = SessionLock::acquire(lock_path(directory, &self.session_id), false)?;
        self.reconstruct()?;
        self.lock = lock;
        Ok(())
    }

    fn reconstruct(&mut self) -> Result<(), String> {
        let mut source = self
            .file
            .try_clone()
            .and_then(|mut file| {
                file.rewind()?;
                Ok(file)
            })
            .map_err(|error| format!("could not read open session transcript: {error}"))?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|error| format!("could not reconstruct {}: {error}", self.path.display()))?;
        if let Err(error) = io::copy(&mut source, &mut file).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&self.path);
            return Err(format!(
                "could not reconstruct {}: {error}",
                self.path.display()
            ));
        }
        self.file = file;
        Ok(())
    }
}

enum LockError {
    Missing,
    Other(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("session lock was lost"),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

impl SessionLock {
    fn acquire(path: PathBuf, force: bool) -> Result<Self, String> {
        Self::acquire_with(path, force, |file, token| {
            file.set_len(0)
                .and_then(|_| file.write_all(token.as_bytes()))
                .and_then(|_| file.sync_all())
        })
    }

    fn acquire_with(
        path: PathBuf,
        force: bool,
        initialize: impl FnOnce(&mut File, &str) -> io::Result<()>,
    ) -> Result<Self, String> {
        Self::acquire_with_hook(path, force, || {}, initialize)
    }

    fn acquire_with_hook(
        path: PathBuf,
        force: bool,
        before_lock: impl FnOnce(),
        initialize: impl FnOnce(&mut File, &str) -> io::Result<()>,
    ) -> Result<Self, String> {
        let token = format!("{}:{}:{}", std::process::id(), new_id(), SCHEMA_VERSION);
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if force {
            options.create(true);
        } else {
            options.create_new(true);
        }
        let mut file = options.open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!("session is locked by another Kit instance ({}); use --force to override a stale lock", path.display())
            } else {
                format!("could not acquire session lock {}: {error}", path.display())
            }
        })?;
        before_lock();
        if file.try_lock().is_err() {
            // A forced opener can take the OS lock after this process creates
            // the pathname but before this call. It now owns that pathname, so
            // the loser must close only and must never unlink it.
            drop(file);
            return Err(format!(
                "session is actively locked by another Kit instance ({})",
                path.display()
            ));
        }
        if let Err(error) = initialize(&mut file, &token) {
            // The OS lock proves this process owns mutation authority even for
            // a forced stale-lock takeover. Close before removing so a failed
            // token write cannot leave a corrupted path on Windows.
            remove_failed_lock(&path, file);
            return Err(format!("could not write session lock: {error}"));
        }
        Ok(Self {
            path,
            token,
            file: Some(file),
        })
    }

    fn check(&self) -> Result<(), LockError> {
        let current = self.read_token()?;
        if current == self.token {
            Ok(())
        } else {
            Err(LockError::Other(
                "session lock was overridden by another Kit instance".into(),
            ))
        }
    }

    // Reads the lock file's token to confirm this process still owns it. On
    // Unix the advisory lock lets a fresh handle read the path, which also
    // reports the file being unlinked as `Missing` so the writer can recover.
    #[cfg(not(windows))]
    fn read_token(&self) -> Result<String, LockError> {
        fs::read_to_string(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                LockError::Missing
            } else {
                LockError::Other(format!("session lock was lost: {error}"))
            }
        })
    }

    // On Windows `File::try_lock` takes a mandatory exclusive lock, so
    // re-opening the path to read the token fails with a sharing violation
    // (os error 33). Read it through the lock-owning handle instead, which the
    // lock owner is always permitted to do. The lock file is opened without
    // FILE_SHARE_DELETE, so it cannot be unlinked while this handle is held; a
    // `Missing` state therefore cannot arise here and losing the handle is
    // itself the lost-lock condition.
    #[cfg(windows)]
    fn read_token(&self) -> Result<String, LockError> {
        use std::io::Read;
        let Some(file) = self.file.as_ref() else {
            return Err(LockError::Missing);
        };
        let mut handle: &File = file;
        let mut current = String::new();
        handle
            .seek(SeekFrom::Start(0))
            .and_then(|_| handle.read_to_string(&mut current))
            .map_err(|error| LockError::Other(format!("session lock was lost: {error}")))?;
        Ok(current)
    }
}

fn remove_failed_lock(path: &Path, file: File) {
    drop(file);
    let _ = fs::remove_file(path);
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if self.check().is_ok() {
            drop(self.file.take());
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn stamp_item(item: &mut Item, now: Timestamp) {
    if item.created_at.is_none() {
        item.created_at = Some(now);
    }
}

struct TranscriptHistory {
    items: Vec<Item>,
    generation: u64,
    states: Vec<Vec<Item>>,
}

enum StoredTranscript {
    History(TranscriptHistory),
    Redirect(PathBuf),
}

fn read_records(path: &Path, session_id: &str) -> Result<(Vec<Item>, u64), String> {
    read_records_following(path, session_id, 0)
}

fn read_records_following(
    path: &Path,
    session_id: &str,
    redirects: usize,
) -> Result<(Vec<Item>, u64), String> {
    match read_records_direct(path, session_id)? {
        StoredTranscript::History(history) => Ok((history.items, history.generation)),
        StoredTranscript::Redirect(target) => {
            if redirects >= 4 || target.file_name() != path.file_name() || !target.is_absolute() {
                return Err(format!("invalid session redirect in {}", path.display()));
            }
            read_records_following(&target, session_id, redirects + 1)
        }
    }
}

fn read_records_direct(path: &Path, session_id: &str) -> Result<StoredTranscript, String> {
    let file =
        File::open(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut items = Vec::new();
    let mut expected = 1_u64;
    let mut states = Vec::new();
    let mut redirect = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.map_err(|error| format!("could not read transcript line {}: {error}", index + 1))?;
        let record: Record = serde_json::from_str(&line)
            .map_err(|error| format!("invalid transcript line {}: {error}", index + 1))?;
        if !matches!(
            record.schema_version,
            LEGACY_SCHEMA_VERSION
                | PREVIOUS_SCHEMA_VERSION
                | SCHEMA_VERSION
                | REDIRECT_SCHEMA_VERSION
        ) {
            return Err(format!(
                "unsupported session schema version {} on line {} (Kit supports {})",
                record.schema_version,
                index + 1,
                REDIRECT_SCHEMA_VERSION
            ));
        }
        if record.session_id != session_id || record.generation != expected {
            return Err(format!(
                "invalid session identity or generation on transcript line {}",
                index + 1
            ));
        }
        if redirect.is_some() {
            return Err(format!(
                "session redirect must be the final transcript line ({})",
                path.display()
            ));
        }
        match (record.item, record.replacement, record.redirect) {
            (Some(item), None, None) if record.schema_version <= SCHEMA_VERSION => items.push(item),
            (None, Some(replacement), None)
                if matches!(
                    record.schema_version,
                    PREVIOUS_SCHEMA_VERSION | SCHEMA_VERSION
                ) && !replacement.is_empty() =>
            {
                if !items.is_empty() {
                    states.push(items.clone());
                }
                items = replacement;
            }
            (None, None, Some(target))
                if record.schema_version == REDIRECT_SCHEMA_VERSION
                    && record.workspace_root.is_some() =>
            {
                redirect = Some(target);
            }
            _ => {
                return Err(format!(
                    "transcript line {} must contain exactly one item, replacement, or redirect",
                    index + 1
                ));
            }
        }
        expected += 1;
    }
    if let Some(target) = redirect {
        return Ok(StoredTranscript::Redirect(target));
    }
    if items.is_empty() {
        return Err(format!("session transcript {} is empty", path.display()));
    }
    states.push(items.clone());
    Ok(StoredTranscript::History(TranscriptHistory {
        items,
        generation: expected - 1,
        states,
    }))
}

fn canonical_workspace(root: &Path) -> PathBuf {
    if let Ok(canonical) = root.canonicalize() {
        return canonical;
    }
    let mut ancestor = root.to_path_buf();
    let mut suffix = Vec::new();
    while let Some(name) = ancestor.file_name().map(ToOwned::to_owned) {
        suffix.push(name);
        if !ancestor.pop() {
            return root.to_path_buf();
        }
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    root.to_path_buf()
}

fn normalized_absolute(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("could not normalize {}: {error}", path.display()))
}

fn default_directory() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".kit/sessions"))
        .ok_or_else(|| "HOME is unset; cannot locate durable sessions".into())
}

fn workspace_directory(root: &Path) -> PathBuf {
    root.join(".kit/sessions")
}

fn transcript_workspace(path: &Path, session_id: &str) -> Result<Option<PathBuf>, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    transcript_workspace_bytes(path, session_id, &bytes)
}

fn transcript_workspace_bytes(
    path: &Path,
    session_id: &str,
    bytes: &[u8],
) -> Result<Option<PathBuf>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("invalid transcript {}: {error}", path.display()))?;
    let mut workspace = None;
    for (index, line) in text.lines().enumerate() {
        let record: Record = serde_json::from_str(line)
            .map_err(|error| format!("invalid transcript line {}: {error}", index + 1))?;
        if record.session_id != session_id {
            return Err(format!(
                "invalid session identity on transcript line {}",
                index + 1
            ));
        }
        if let Some(record_workspace) = record.workspace_root {
            if workspace
                .as_ref()
                .is_some_and(|workspace| workspace != &record_workspace)
            {
                return Err(format!(
                    "conflicting workspace roots in transcript {}",
                    path.display()
                ));
            }
            workspace = Some(record_workspace);
        }
    }
    Ok(workspace)
}

fn ensure_workspace(path: &Path, session_id: &str, root: &Path) -> Result<(), String> {
    if let Some(stored) = transcript_workspace(path, session_id)?
        && stored != root
    {
        return Err(format!(
            "session {session_id:?} belongs to workspace {}, not {}",
            stored.display(),
            root.display()
        ));
    }
    Ok(())
}

/// Lists durable sessions bound to one workspace, newest first, without taking mutation locks.
pub fn catalog(root: &Path) -> Result<Vec<CatalogEntry>, String> {
    catalog_for_workspace(root, &default_directory()?)
}

fn catalog_for_workspace(
    root: &Path,
    global_directory: &Path,
) -> Result<Vec<CatalogEntry>, String> {
    let root = root.canonicalize().map_err(|error| {
        format!(
            "could not resolve workspace root {}: {error}",
            root.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "workspace root {} is not a directory",
            root.display()
        ));
    }
    let ids = list_ids_for_workspace(&root, global_directory)?;
    let mut entries = Vec::with_capacity(ids.len());
    for id in ids {
        // Discovery is best-effort per transcript: a damaged file or one caught
        // mid-append must not hide every other session in the workspace.
        let Ok(Some(authority)) = select_authority_with(global_directory, &root, &id, false) else {
            continue;
        };
        if catalog_is_subagent(&authority.historical_items, &authority.items) {
            continue;
        }
        let is_subagent = false;
        let (title, preview) = catalog_text(&authority.historical_items, &authority.items);
        let item_updated = authority
            .items
            .iter()
            .filter_map(|item| {
                item.created_at
                    .map(|timestamp| timestamp.0)
                    .filter(|timestamp| *timestamp <= MAX_RFC3339_MILLIS)
            })
            .max()
            .unwrap_or(0);
        let file_updated = fs::metadata(&authority.path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        entries.push(CatalogEntry {
            id,
            title,
            preview,
            is_subagent,
            updated_at: item_updated.max(file_updated),
        });
    }
    entries.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(entries)
}

fn catalog_is_subagent(historical_items: &[Vec<Item>], current_items: &[Item]) -> bool {
    historical_items
        .iter()
        .map(Vec::as_slice)
        .chain(std::iter::once(current_items))
        .flatten()
        .any(|item| {
            item.kind == ItemKind::System
                && item
                    .metadata
                    .get(SESSION_ORIGIN_METADATA_KEY)
                    .and_then(serde_json::Value::as_str)
                    == Some(SUBAGENT_SESSION_ORIGIN)
        })
}

fn catalog_text(
    historical_items: &[Vec<Item>],
    current_items: &[Item],
) -> (Option<String>, Option<String>) {
    let title = historical_items
        .iter()
        .map(Vec::as_slice)
        .chain(std::iter::once(current_items))
        .find_map(first_user_text)
        .and_then(|text| {
            text.lines()
                .map(normalize_catalog_text)
                .find(|line| !line.is_empty())
        })
        .map(|line| truncate_catalog_text(&line, 80));
    let preview = first_user_text(current_items)
        .map(normalize_catalog_text)
        .filter(|text| !text.is_empty())
        .map(|text| truncate_catalog_text(&text, 160));
    (title, preview)
}

fn first_user_item_index(items: &[Item]) -> Option<usize> {
    items.iter().position(|item| {
        item.kind == ItemKind::User
            && item.parts.iter().any(|part| {
                matches!(
                    part,
                    Part::Text(text) if !normalize_catalog_text(&text.text).is_empty()
                )
            })
    })
}

fn first_user_item(items: &[Item]) -> Option<&Item> {
    first_user_item_index(items).map(|index| &items[index])
}

fn first_user_text(items: &[Item]) -> Option<&str> {
    first_user_item(items)?
        .parts
        .iter()
        .find_map(|part| match part {
            Part::Text(text) if !normalize_catalog_text(&text.text).is_empty() => {
                Some(text.text.as_str())
            }
            _ => None,
        })
}

fn normalize_catalog_text(text: &str) -> String {
    text.chars()
        .filter(|character| {
            (!character.is_control() || character.is_whitespace())
                && !is_default_ignorable(*character)
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_default_ignorable(character: char) -> bool {
    matches!(
        character as u32,
        0x00ad
            | 0x034f
            | 0x061c
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180f
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0x3164
            | 0xfe00..=0xfe0f
            | 0xfeff
            | 0xffa0
            | 0xfff0..=0xfff8
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0000..=0xe0fff
    )
}

fn truncate_catalog_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut value = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    value.push('…');
    value
}

fn timestamp_rfc3339(milliseconds: u64) -> String {
    let milliseconds = milliseconds.min(MAX_RFC3339_MILLIS);
    let seconds = milliseconds / 1_000;
    let millis = milliseconds % 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_date(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

// Gregorian civil date from days since 1970-01-01.
fn civil_date(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn list_ids_for_workspace(root: &Path, global_directory: &Path) -> Result<Vec<String>, String> {
    let root = canonical_workspace(root);
    let scoped_directory = workspace_storage_directory(global_directory, &root);
    let legacy_directory = workspace_directory(&root);
    let mut ids = Vec::new();
    ids.extend(list_ids_in(&scoped_directory)?);
    for id in list_ids_in(global_directory)? {
        let path = transcript_path(global_directory, &id);
        if matches!(
            transcript_workspace(&path, &id),
            Ok(Some(stored)) if stored == root
        ) {
            ids.push(id);
        }
    }
    // Its location scopes this pre-global-layout directory to the workspace.
    ids.extend(list_ids_in(&legacy_directory)?);
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub(crate) fn belongs_to_workspace(root: &Path, session_id: &str) -> Result<bool, String> {
    belongs_to_workspace_in(root, &default_directory()?, session_id)
}

fn belongs_to_workspace_in(
    root: &Path,
    global_directory: &Path,
    session_id: &str,
) -> Result<bool, String> {
    validate_id(session_id)?;
    let root = canonical_workspace(root);
    Ok(select_authority(global_directory, &root, session_id)?.is_some())
}

pub(crate) fn list_ids_in(directory: &Path) -> Result<Vec<String>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "could not list session directory {}: {error}",
                directory.display()
            ));
        }
    };
    let mut ids = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if validate_id(id).is_ok() {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

struct Authority {
    items: Vec<Item>,
    historical_items: Vec<Vec<Item>>,
    path: PathBuf,
    legacy_histories: Vec<PathBuf>,
}

struct HistoryCandidate {
    path: PathBuf,
    history: TranscriptHistory,
}

fn history_descends_from(history: &TranscriptHistory, ancestor: &[Item]) -> bool {
    history
        .states
        .iter()
        .any(|state| state.starts_with(ancestor))
}

fn select_authority(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<Option<Authority>, String> {
    select_authority_with(directory, root, session_id, true)
}

fn select_authority_with(
    directory: &Path,
    root: &Path,
    session_id: &str,
    include_unbound_global: bool,
) -> Result<Option<Authority>, String> {
    let scoped = transcript_path(&workspace_storage_directory(directory, root), session_id);
    let global = transcript_path(directory, session_id);
    let local = legacy_transcript(root, session_id);
    let mut histories = Vec::new();
    let mut legacy_histories = Vec::new();
    let mut unbound_global = None;

    for (path, is_global, is_legacy) in [
        (&scoped, false, false),
        (&global, true, true),
        (&local, false, true),
    ] {
        if !path
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
        {
            continue;
        }
        let workspace = transcript_workspace(path, session_id)?;
        if is_global
            && (workspace.as_deref().is_some_and(|stored| stored != root)
                || workspace.is_none() && !include_unbound_global)
        {
            continue;
        }
        let is_unbound_global = is_global && workspace.is_none();
        if !is_global && workspace.as_deref().is_some_and(|stored| stored != root) {
            return Err(format!(
                "session {session_id:?} belongs to workspace {}, not {}",
                workspace.unwrap().display(),
                root.display()
            ));
        }
        match read_records_direct(path, session_id)? {
            StoredTranscript::History(history) => {
                let candidate = HistoryCandidate {
                    path: path.clone(),
                    history,
                };
                if is_unbound_global {
                    unbound_global = Some(candidate);
                } else {
                    histories.push(candidate);
                    if is_legacy {
                        legacy_histories.push(path.clone());
                    }
                }
            }
            StoredTranscript::Redirect(target) => {
                let target = normalized_absolute(&target)?;
                let scoped_target = normalized_absolute(&scoped)?;
                if target != scoped_target {
                    return Err(format!("invalid session redirect in {}", path.display()));
                }
                ensure_workspace(&target, session_id, root)?;
                let StoredTranscript::History(history) = read_records_direct(&target, session_id)?
                else {
                    return Err(format!(
                        "scoped transcript {} is a redirect",
                        target.display()
                    ));
                };
                histories.push(HistoryCandidate {
                    path: target,
                    history,
                });
            }
        }
    }

    if histories.is_empty()
        && let Some(candidate) = unbound_global
    {
        legacy_histories.push(candidate.path.clone());
        histories.push(candidate);
    }

    let mut authority: Option<HistoryCandidate> = None;
    for candidate in histories {
        let Some(current) = authority.as_mut() else {
            authority = Some(candidate);
            continue;
        };
        let candidate_descends = history_descends_from(&candidate.history, &current.history.items);
        let current_descends = history_descends_from(&current.history, &candidate.history.items);
        match (candidate_descends, current_descends) {
            (true, false) => *current = candidate,
            (false, true) => {}
            (true, true) if candidate.path == scoped => *current = candidate,
            (true, true) => {}
            (false, false) => {
                return Err(format!(
                    "divergent session histories for {session_id:?}: {} and {}",
                    current.path.display(),
                    candidate.path.display()
                ));
            }
        }
    }
    Ok(authority.map(|candidate| Authority {
        items: candidate.history.items,
        historical_items: candidate.history.states,
        path: candidate.path,
        legacy_histories,
    }))
}

fn torn_migration_tail_start(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return None;
    }
    let start = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    serde_json::from_slice::<Record>(&bytes[start..])
        .is_err()
        .then_some(start)
}

fn migration_source_workspace(path: &Path, session_id: &str) -> Result<Option<PathBuf>, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let complete = torn_migration_tail_start(&bytes).unwrap_or(bytes.len());
    transcript_workspace_bytes(path, session_id, &bytes[..complete])
}

fn recover_torn_migration_writes(
    scoped: &Path,
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<(), String> {
    let mut paths = applicable_migration_sources(directory, root, session_id)?;
    paths.push(scoped.to_path_buf());
    paths.sort();
    paths.dedup();
    for path in paths {
        let exists = path
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if !exists {
            continue;
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if bytes.is_empty() && path == scoped {
            fs::remove_file(&path)
                .map_err(|error| format!("could not remove {}: {error}", path.display()))?;
            sync_parent_directory(&path)?;
            continue;
        }
        let Some(complete) = torn_migration_tail_start(&bytes) else {
            continue;
        };
        if complete == 0 && path == scoped {
            fs::remove_file(&path)
                .map_err(|error| format!("could not remove {}: {error}", path.display()))?;
            sync_parent_directory(&path)?;
        } else {
            let file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| format!("could not open {}: {error}", path.display()))?;
            file.set_len(complete as u64)
                .and_then(|_| file.sync_all())
                .map_err(|error| {
                    format!(
                        "could not recover torn migration {}: {error}",
                        path.display()
                    )
                })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            format!(
                "could not sync session directory {}: {error}",
                parent.display()
            )
        })
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn applicable_migration_sources(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<Vec<PathBuf>, String> {
    let global = transcript_path(directory, session_id);
    let mut sources = vec![legacy_transcript(root, session_id)];
    let global_exists = global
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", global.display()))?;
    if !global_exists
        || migration_source_workspace(&global, session_id)?
            .as_deref()
            .is_none_or(|stored| stored == root)
    {
        sources.push(global);
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn lock_migration_sources(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<Vec<SessionLock>, String> {
    let mut locks = Vec::new();
    let mut locked_paths = Vec::new();
    loop {
        let sources = applicable_migration_sources(directory, root, session_id)?;
        let pending = sources
            .into_iter()
            .map(|path| path.with_extension("lock"))
            .filter(|path| !locked_paths.contains(path))
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(locks);
        }
        for path in pending {
            fs::create_dir_all(path.parent().expect("session lock has a parent"))
                .map_err(|error| format!("could not create legacy session directory: {error}"))?;
            let lock = SessionLock::acquire(path.clone(), true)
                .map_err(|error| format!("legacy {error}"))?;
            locked_paths.push(path);
            locks.push(lock);
        }
        // Re-read source ownership while the applicable locks are held. If a
        // previously absent source appeared, the next iteration locks it too.
    }
}

fn write_migration_record(path: &Path, record: &Record, create: bool) -> Result<(), String> {
    write_migration_record_with(path, record, create, |file, encoded| {
        file.write_all(encoded)
    })
}

fn write_migration_record_with(
    path: &Path,
    record: &Record,
    create: bool,
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> Result<(), String> {
    let mut encoded = serde_json::to_vec(record)
        .map_err(|error| format!("could not encode transcript record: {error}"))?;
    encoded.push(b'\n');
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let original_len = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
        .len();
    if !create {
        file.seek(SeekFrom::End(0))
            .map_err(|error| format!("could not seek {}: {error}", path.display()))?;
    }
    if let Err(error) = write(&mut file, &encoded).and_then(|_| file.sync_all()) {
        let rollback = if create {
            drop(file);
            fs::remove_file(path)
        } else {
            file.set_len(original_len).and_then(|_| file.sync_all())
        };
        return match rollback {
            Ok(()) => Err(format!("could not persist transcript migration: {error}")),
            Err(rollback) => Err(format!(
                "could not persist transcript migration: {error}; rollback failed: {rollback}"
            )),
        };
    }
    if create {
        sync_parent_directory(path)?;
    }
    Ok(())
}

fn establish_scoped_authority(
    path: &Path,
    session_id: &str,
    root: &Path,
    items: &[Item],
    title_seed: Option<&[Item]>,
) -> Result<(), String> {
    let exists = path
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    let (mut generation, existing_items) = if exists {
        match read_records_direct(path, session_id)? {
            StoredTranscript::History(history)
                if history.items == items
                    && transcript_workspace(path, session_id)?.as_deref() == Some(root) =>
            {
                return Ok(());
            }
            StoredTranscript::History(history) => (
                history
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| "session generation overflowed".to_string())?,
                Some(history.items),
            ),
            StoredTranscript::Redirect(_) => {
                return Err(format!(
                    "scoped transcript {} is a redirect",
                    path.display()
                ));
            }
        }
    } else {
        (1, None)
    };
    let title_seed =
        title_seed.filter(|seed| *seed != items && existing_items.as_deref() != Some(*seed));
    let mut create = !exists;
    if let Some(title_seed) = title_seed {
        write_migration_record(
            path,
            &Record {
                schema_version: SCHEMA_VERSION,
                session_id: session_id.into(),
                generation,
                workspace_root: Some(root.to_path_buf()),
                item: None,
                replacement: Some(title_seed.to_vec()),
                redirect: None,
            },
            create,
        )?;
        generation = generation
            .checked_add(1)
            .ok_or_else(|| "session generation overflowed".to_string())?;
        create = false;
    }
    write_migration_record(
        path,
        &Record {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.into(),
            generation,
            workspace_root: Some(root.to_path_buf()),
            item: None,
            replacement: Some(items.to_vec()),
            redirect: None,
        },
        create,
    )
}

fn redirect_legacy_transcript(
    path: &Path,
    target: &Path,
    session_id: &str,
    root: &Path,
) -> Result<(), String> {
    let target = normalized_absolute(target)?;
    let generation = match read_records_direct(path, session_id)? {
        StoredTranscript::History(history) => history
            .generation
            .checked_add(1)
            .ok_or_else(|| "session generation overflowed".to_string())?,
        StoredTranscript::Redirect(current) if normalized_absolute(&current)? == target => {
            return Ok(());
        }
        StoredTranscript::Redirect(_) => {
            return Err(format!("invalid session redirect in {}", path.display()));
        }
    };
    write_migration_record(
        path,
        &Record {
            schema_version: REDIRECT_SCHEMA_VERSION,
            session_id: session_id.into(),
            generation,
            workspace_root: Some(root.to_path_buf()),
            item: None,
            replacement: None,
            redirect: Some(target),
        },
        false,
    )
}

fn legacy_transcript_for_workspace(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>, String> {
    let global = transcript_path(directory, session_id);
    if global.exists()
        && transcript_workspace(&global, session_id)?
            .as_deref()
            .is_none_or(|stored| stored == root)
    {
        return Ok(Some(global));
    }
    let local = legacy_transcript(root, session_id);
    if local.exists() {
        read_records(&local, session_id)?;
        Ok(Some(local))
    } else {
        Ok(None)
    }
}

fn workspace_storage_directory(directory: &Path, root: &Path) -> PathBuf {
    let identity = blake3::hash(root.as_os_str().as_encoded_bytes());
    directory.join(format!("w-{}", identity.to_hex()))
}

fn transcript_path(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(format!("{session_id}.jsonl"))
}

fn legacy_transcript(root: &Path, session_id: &str) -> PathBuf {
    transcript_path(&workspace_directory(root), session_id)
}

fn lock_path(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(format!("{session_id}.lock"))
}

pub(crate) fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err("session id must be 1-128 ASCII letters, digits, '-' or '_'".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{ItemKind, Part};

    fn session_directory(root: &Path) -> PathBuf {
        root.join("sessions")
    }

    fn project_root(root: &Path) -> PathBuf {
        root.join("project")
    }

    fn open(
        root: &Path,
        session_id: &str,
        resume: bool,
        force: bool,
        initial: Vec<Item>,
    ) -> Result<OpenSession, String> {
        open_in(
            &project_root(root),
            &session_directory(root),
            session_id,
            resume,
            force,
            initial,
        )
    }

    fn load(root: &Path, session_id: &str) -> Result<Vec<Item>, String> {
        load_in(&project_root(root), &session_directory(root), session_id)
    }

    fn clone_completed(root: &Path, source: &str, destination: &str) -> Result<(), String> {
        clone_completed_in(
            &project_root(root),
            &session_directory(root),
            source,
            destination,
        )
    }

    fn remove_stale_lock(root: &Path, session_id: &str) -> Result<(), String> {
        remove_stale_lock_in(&project_root(root), &session_directory(root), session_id)
    }

    fn scoped_directory(root: &Path) -> PathBuf {
        workspace_storage_directory(
            &session_directory(root),
            &canonical_workspace(&project_root(root)),
        )
    }

    fn legacy_directory(root: &Path) -> PathBuf {
        workspace_directory(&canonical_workspace(&project_root(root)))
    }

    fn transcript_path(root: &Path, session_id: &str) -> PathBuf {
        super::transcript_path(&scoped_directory(root), session_id)
    }

    fn session_lock_path(root: &Path, session_id: &str) -> PathBuf {
        super::lock_path(&scoped_directory(root), session_id)
    }

    fn item_text(item: &Item) -> &str {
        let Some(Part::Text(text)) = item.parts.first() else {
            panic!("expected text item");
        };
        &text.text
    }

    fn write_history(
        path: &Path,
        schema_version: u32,
        session_id: &str,
        texts: &[&str],
        workspace_root: Option<PathBuf>,
    ) -> Vec<Item> {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let items = texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                Item::text(ItemKind::System, *text).with_created_at(Timestamp((index + 1) as u64))
            })
            .collect::<Vec<_>>();
        let encoded = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                serde_json::to_string(&Record {
                    schema_version,
                    session_id: session_id.into(),
                    generation: (index + 1) as u64,
                    workspace_root: workspace_root.clone(),
                    item: Some(item.clone()),
                    replacement: None,
                    redirect: None,
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, format!("{encoded}\n")).unwrap();
        items
    }

    #[test]
    fn generated_ids_are_valid_unique_and_durable() {
        let first = new_id();
        let second = new_id();
        assert!(first.starts_with("s-"));
        assert!(second.starts_with("s-"));
        assert_ne!(first, second);
        assert!(first.len() <= 128);
        validate_id(&first).unwrap();
        validate_id(&second).unwrap();
    }

    #[test]
    fn incomplete_new_transcript_is_removed_but_committed_transcript_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let failed = root.path().join("failed.jsonl");
        fs::write(&failed, "partial").unwrap();
        drop(CreatedTranscript::new(failed.clone()));
        assert!(!failed.exists());

        let committed = root.path().join("committed.jsonl");
        fs::write(&committed, "complete").unwrap();
        CreatedTranscript::new(committed.clone()).keep();
        assert!(committed.exists());
    }

    #[test]
    fn failed_lock_token_initialization_removes_new_lock_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("failed.lock");
        let error = SessionLock::acquire_with(path.clone(), false, |_, _| {
            Err(io::Error::other("injected token failure"))
        })
        .err()
        .expect("injected token initialization unexpectedly succeeded");
        assert!(error.contains("injected token failure"));
        assert!(!path.exists(), "failed initialization left a lock path");

        drop(SessionLock::acquire(path.clone(), false).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn failed_forced_lock_token_initialization_removes_corrupted_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("failed-force.lock");
        fs::write(&path, "stale owner").unwrap();

        let error = SessionLock::acquire_with(path.clone(), true, |file, _| {
            file.set_len(0)?;
            Err(io::Error::other("injected forced token failure"))
        })
        .err()
        .expect("injected forced initialization unexpectedly succeeded");

        assert!(error.contains("injected forced token failure"));
        assert!(
            !path.exists(),
            "failed forced initialization left a lock path"
        );
        drop(SessionLock::acquire(path.clone(), false).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn non_force_lock_loser_does_not_unlink_forced_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("takeover.lock");
        let takeover = std::cell::RefCell::new(None);

        let error = SessionLock::acquire_with_hook(
            path.clone(),
            false,
            || {
                *takeover.borrow_mut() = Some(SessionLock::acquire(path.clone(), true).unwrap());
            },
            |file, token| file.write_all(token.as_bytes()),
        )
        .err()
        .expect("non-force opener unexpectedly retained the OS lock");

        assert!(error.contains("actively locked"));
        assert!(path.exists(), "lock loser unlinked the forced owner's path");
        assert!(takeover.borrow().as_ref().unwrap().check().is_ok());
        drop(takeover.into_inner());
        assert!(!path.exists());
    }

    #[test]
    fn appends_versioned_generations_and_resumes() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        opened.observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("abc"),
            item: &Item::text(ItemKind::User, "hello").with_created_at(Timestamp(123)),
        });
        drop(opened);
        let resumed = open(
            root.path(),
            "abc",
            true,
            false,
            vec![Item::text(ItemKind::System, "ignored")],
        )
        .unwrap();
        assert_eq!(resumed.transcript.len(), 2);
        assert!(resumed.transcript[0].created_at.is_some());
        assert_eq!(resumed.transcript[1].created_at, Some(Timestamp(123)));
        let text = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert!(text.contains(&format!("\"schema_version\":{SCHEMA_VERSION}")));
        assert!(text.contains("\"generation\":2"));
        assert!(text.contains("\"workspace_root\""));
    }

    #[test]
    #[should_panic(expected = "transcript item missing created_at before persistence")]
    fn observer_rejects_unstamped_items() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();

        opened.observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("abc"),
            item: &Item::text(ItemKind::User, "unstamped"),
        });
    }

    #[test]
    fn persisted_tool_boundaries_preserve_loop_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(&opened.observer, &call_item("call-1"));
        write(
            &opened.observer,
            &Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(agentkit_core::ToolResultPart::success(
                    "call-1",
                    agentkit_core::ToolOutput::text("done"),
                ))],
            ),
        );

        let items = stored(root.path());
        assert!(items.iter().all(|item| item.created_at.is_some()));
        let dispatched = items[1].created_at.unwrap().0;
        let completed = items[2].created_at.unwrap().0;
        assert!(completed >= dispatched);
    }

    #[test]
    fn reads_legacy_null_and_missing_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let directory = legacy_directory(root.path());
        fs::create_dir_all(&directory).unwrap();
        let mut missing = serde_json::to_value(Item::text(ItemKind::System, "missing")).unwrap();
        missing.as_object_mut().unwrap().remove("created_at");
        let lines = [
            serde_json::json!({
                "schema_version": LEGACY_SCHEMA_VERSION,
                "session_id": "abc",
                "generation": 1,
                "item": missing,
            }),
            serde_json::json!({
                "schema_version": LEGACY_SCHEMA_VERSION,
                "session_id": "abc",
                "generation": 2,
                "item": Item::text(ItemKind::System, "null"),
            }),
        ];
        let stored = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(directory.join("abc.jsonl"), stored).unwrap();

        let loaded = load(root.path(), "abc").unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|item| item.created_at.is_none()));
    }

    #[test]
    fn cloning_preserves_historical_unknown_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let directory = legacy_directory(root.path());
        fs::create_dir_all(&directory).unwrap();
        let lines = [
            serde_json::json!({
                "schema_version": LEGACY_SCHEMA_VERSION,
                "session_id": "source",
                "generation": 1,
                "item": Item::text(ItemKind::System, "unknown"),
            }),
            serde_json::json!({
                "schema_version": LEGACY_SCHEMA_VERSION,
                "session_id": "source",
                "generation": 2,
                "item": Item::text(ItemKind::User, "known").with_created_at(Timestamp(77)),
            }),
        ];
        let stored = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(directory.join("source.jsonl"), stored).unwrap();

        clone_completed(root.path(), "source", "branch").unwrap();

        let cloned = load(root.path(), "branch").unwrap();
        assert_eq!(cloned[0].created_at, None);
        assert_eq!(cloned[1].created_at, Some(Timestamp(77)));
    }

    #[test]
    fn replacement_is_canonical_across_resume_and_later_appends() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "discard me"));
        let original_timestamp = opened.transcript[0].created_at;
        let replacement = vec![
            opened.transcript[0].clone(),
            Item::text(ItemKind::Context, "summary"),
        ];
        opened.observer.replace(&replacement).unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "after"));
        drop(opened);

        let resumed = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(resumed.transcript.len(), 3);
        assert_eq!(resumed.transcript[1].kind, ItemKind::Context);
        assert_eq!(resumed.transcript[2].kind, ItemKind::User);
        assert_eq!(resumed.transcript[0].created_at, original_timestamp);
        assert_eq!(resumed.transcript[1].created_at, None);
        assert!(resumed.transcript[2].created_at.is_some());
        let stored = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert!(stored.contains("\"replacement\""));
        assert!(stored.contains("\"generation\":4"));
    }

    fn call_item(id: &str) -> Item {
        Item::new(
            ItemKind::Assistant,
            vec![Part::ToolCall(agentkit_core::ToolCallPart::new(
                id,
                "compose",
                serde_json::json!({}),
            ))],
        )
    }

    fn write(observer: &SessionObserver, item: &Item) {
        let mut item = item.clone();
        stamp_item(&mut item, Timestamp::now());
        observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("abc"),
            item: &item,
        });
    }

    fn stored(root: &Path) -> Vec<Item> {
        read_records(&transcript_path(root, "abc"), "abc")
            .unwrap()
            .0
    }

    /// The writer records what the loop commits and nothing else: closing an
    /// interrupted call is the loop's job, and synthesizing anything here would
    /// duplicate the result it already appended.
    #[test]
    fn the_writer_records_exactly_what_it_is_given() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(&opened.observer, &call_item("call-1"));
        write(
            &opened.observer,
            &Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(agentkit_core::ToolResultPart::success(
                    "call-1",
                    agentkit_core::ToolOutput::text("done"),
                ))],
            ),
        );
        write(&opened.observer, &Item::text(ItemKind::User, "next"));

        assert_eq!(stored(root.path()).len(), 4, "nothing extra was recorded");
    }

    #[test]
    fn resuming_answers_and_persists_a_tool_call_the_file_never_answered() {
        use agentkit_core::ToolCallPart;

        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        // The shape a damaged file carries: the call is recorded, its result is
        // missing, and the next prompt lands after it.
        for item in [
            Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(ToolCallPart::new(
                    "call-1",
                    "compose",
                    serde_json::json!({}),
                ))],
            ),
            Item::text(ItemKind::User, "changes published"),
        ] {
            write(&opened.observer, &item);
        }
        drop(opened);

        let resumed = open(root.path(), "abc", true, false, Vec::new()).unwrap();

        assert!(!crate::transcript::has_unanswered_tool_calls(
            &resumed.transcript
        ));
        assert_eq!(
            resumed
                .transcript
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            [
                ItemKind::System,
                ItemKind::Assistant,
                ItemKind::Tool,
                ItemKind::User
            ],
            "the synthesized result belongs next to its call"
        );
        let repaired_timestamp = resumed.transcript[2].created_at;
        assert!(repaired_timestamp.is_some());
        resumed.observer.replace(&resumed.transcript).unwrap();
        drop(resumed);

        // The repair and its construction timestamp survive a later replacement,
        // and a subsequent resume finds nothing else to fix.
        // and never records the same result twice.
        let again = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(again.transcript.len(), 4);
        let results = again
            .transcript
            .iter()
            .flat_map(|item| &item.parts)
            .filter(|part| matches!(part, Part::ToolResult(_)))
            .count();
        assert_eq!(results, 1);
        assert_eq!(again.transcript[2].created_at, repaired_timestamp);
        let text = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.contains("\"generation\":5"));
    }

    #[test]
    fn lock_requires_explicit_override_and_only_reclaims_stale_locks() {
        let root = tempfile::tempdir().unwrap();
        let first = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "one")],
        )
        .unwrap();
        assert!(
            open(
                root.path(),
                "abc",
                true,
                false,
                vec![Item::text(ItemKind::System, "x")]
            )
            .is_err()
        );
        assert!(
            remove_stale_lock(root.path(), "abc").is_err(),
            "cleanup must not remove a live owner's lock"
        );
        assert!(
            open(
                root.path(),
                "abc",
                true,
                true,
                vec![Item::text(ItemKind::System, "x")]
            )
            .is_err(),
            "force must not steal authority from a live owner"
        );
        drop(first);
        fs::write(session_lock_path(root.path(), "abc"), "abandoned").unwrap();
        remove_stale_lock(root.path(), "abc").unwrap();
        assert!(!session_lock_path(root.path(), "abc").exists());
        assert!(
            open(
                root.path(),
                "abc",
                true,
                false,
                vec![Item::text(ItemKind::System, "x")]
            )
            .is_ok()
        );
    }

    #[test]
    fn legacy_load_falls_back_without_copy_and_resume_migrates() {
        let root = tempfile::tempdir().unwrap();
        let legacy = project_root(root.path()).join(".kit/sessions");
        fs::create_dir_all(&legacy).unwrap();
        let item = Item::text(ItemKind::System, "legacy").with_created_at(Timestamp(7));
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: "abc".into(),
            generation: 1,
            workspace_root: None,
            item: Some(item.clone()),
            replacement: None,
            redirect: None,
        };
        fs::write(
            legacy.join("abc.jsonl"),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();

        assert_eq!(load(root.path(), "abc").unwrap(), vec![item.clone()]);
        assert!(!transcript_path(root.path(), "abc").exists());

        let legacy_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(legacy.join("abc.lock"))
            .unwrap();
        legacy_lock.try_lock().unwrap();
        assert!(
            open(root.path(), "abc", true, false, Vec::new())
                .err()
                .unwrap()
                .contains("legacy session is actively locked")
        );
        legacy_lock.unlock().unwrap();
        drop(legacy_lock);

        let resumed = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(resumed.transcript, vec![item]);
        write(&resumed.observer, &Item::text(ItemKind::User, "global"));
        drop(resumed);

        assert!(transcript_path(root.path(), "abc").is_file());
        assert_eq!(load(root.path(), "abc").unwrap().len(), 2);
    }

    #[test]
    fn schema_one_and_two_global_only_transcripts_migrate() {
        let root = tempfile::tempdir().unwrap();
        for (schema, session_id) in [
            (LEGACY_SCHEMA_VERSION, "schema-one"),
            (PREVIOUS_SCHEMA_VERSION, "schema-two"),
        ] {
            let global = super::transcript_path(&session_directory(root.path()), session_id);
            let expected = write_history(&global, schema, session_id, &["global"], None);

            assert_eq!(load(root.path(), session_id).unwrap(), expected);
            let opened = open(root.path(), session_id, true, false, Vec::new()).unwrap();
            assert_eq!(opened.transcript, expected);
            drop(opened);

            let scoped = transcript_path(root.path(), session_id);
            assert!(scoped.is_file());
            assert!(matches!(
                read_records_direct(&global, session_id).unwrap(),
                StoredTranscript::Redirect(target) if target == normalized_absolute(&scoped).unwrap()
            ));
        }
    }

    #[test]
    fn migrated_redirect_roundtrips_through_current_readers() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        write_history(&global, PREVIOUS_SCHEMA_VERSION, "abc", &["before"], None);

        let opened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "after"));
        drop(opened);

        assert_eq!(read_records(&global, "abc").unwrap().0.len(), 2);
        assert_eq!(load(root.path(), "abc").unwrap().len(), 2);
    }

    #[test]
    fn unrelated_live_global_lock_does_not_block_another_workspace() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let global = super::transcript_path(storage.path(), "shared");
        write_history(
            &global,
            SCHEMA_VERSION,
            "shared",
            &["first"],
            Some(canonical_workspace(&first)),
        );
        let global_lock_path = global.with_extension("lock");
        let global_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&global_lock_path)
            .unwrap();
        global_lock.try_lock().unwrap();

        let opened = open_in(
            &second,
            storage.path(),
            "shared",
            false,
            false,
            vec![Item::text(ItemKind::System, "second")],
        )
        .unwrap();
        assert_eq!(item_text(&opened.transcript[0]), "second");
        drop(opened);

        global_lock.unlock().unwrap();
        drop(global_lock);
        fs::remove_file(global_lock_path).unwrap();
    }

    #[test]
    fn failed_tombstone_append_rolls_back_partial_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("abc.jsonl");
        write_history(&path, PREVIOUS_SCHEMA_VERSION, "abc", &["original"], None);
        let before = fs::read(&path).unwrap();
        let _lock = SessionLock::acquire(path.with_extension("lock"), true).unwrap();
        let record = Record {
            schema_version: REDIRECT_SCHEMA_VERSION,
            session_id: "abc".into(),
            generation: 2,
            workspace_root: Some(root.path().to_path_buf()),
            item: None,
            replacement: None,
            redirect: Some(root.path().join("scoped/abc.jsonl")),
        };

        let error = write_migration_record_with(&path, &record, false, |file, encoded| {
            file.write_all(&encoded[..encoded.len() / 2])?;
            Err(io::Error::other("injected tombstone failure"))
        })
        .unwrap_err();

        assert!(error.contains("injected tombstone failure"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(matches!(
            read_records_direct(&path, "abc").unwrap(),
            StoredTranscript::History(TranscriptHistory { generation: 1, .. })
        ));
    }

    #[test]
    fn relative_session_directory_migration_persists_absolute_redirect() {
        let current = env::current_dir().unwrap();
        let owner = tempfile::tempdir_in(&current).unwrap();
        let relative_owner = owner.path().strip_prefix(&current).unwrap();
        let directory = relative_owner.join("sessions");
        let root = owner.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let global = super::transcript_path(&directory, "abc");
        let expected = write_history(&global, PREVIOUS_SCHEMA_VERSION, "abc", &["relative"], None);

        drop(open_in(&root, &directory, "abc", true, false, Vec::new()).unwrap());

        let target = match read_records_direct(&global, "abc").unwrap() {
            StoredTranscript::Redirect(target) => target,
            StoredTranscript::History(_) => panic!("legacy transcript was not redirected"),
        };
        assert!(target.is_absolute());
        assert_eq!(target, target.canonicalize().unwrap());
        assert_eq!(load_in(&root, &directory, "abc").unwrap(), expected);
    }

    #[test]
    fn newer_global_history_extends_stale_scoped_and_workspace_local_histories() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        let local = super::transcript_path(&legacy_directory(root.path()), "abc");
        write_history(
            &transcript_path(root.path(), "abc"),
            SCHEMA_VERSION,
            "abc",
            &["first"],
            Some(canonical_workspace(&project_root(root.path()))),
        );
        write_history(&local, LEGACY_SCHEMA_VERSION, "abc", &["first"], None);
        let expected = write_history(
            &global,
            PREVIOUS_SCHEMA_VERSION,
            "abc",
            &["first", "newer"],
            Some(canonical_workspace(&project_root(root.path()))),
        );

        let opened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(opened.transcript, expected);
        drop(opened);
        assert!(matches!(
            read_records_direct(&global, "abc").unwrap(),
            StoredTranscript::Redirect(_)
        ));
        assert!(matches!(
            read_records_direct(&local, "abc").unwrap(),
            StoredTranscript::Redirect(_)
        ));
    }

    #[test]
    fn scoped_replacement_descends_from_materialized_stale_legacy_history() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        let stale = write_history(
            &global,
            PREVIOUS_SCHEMA_VERSION,
            "abc",
            &["before-one", "before-two"],
            Some(canonical_workspace(&project_root(root.path()))),
        );
        let scoped = transcript_path(root.path(), "abc");
        write_history(
            &scoped,
            SCHEMA_VERSION,
            "abc",
            &["before-one", "before-two"],
            Some(canonical_workspace(&project_root(root.path()))),
        );
        let compacted = vec![
            Item::text(ItemKind::Context, "compacted newer state").with_created_at(Timestamp(9)),
        ];
        write_migration_record(
            &scoped,
            &Record {
                schema_version: SCHEMA_VERSION,
                session_id: "abc".into(),
                generation: stale.len() as u64 + 1,
                workspace_root: Some(canonical_workspace(&project_root(root.path()))),
                item: None,
                replacement: Some(compacted.clone()),
                redirect: None,
            },
            false,
        )
        .unwrap();

        assert_eq!(load(root.path(), "abc").unwrap(), compacted);
        let opened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(opened.transcript, compacted);
        drop(opened);
        assert!(matches!(
            read_records_direct(&global, "abc").unwrap(),
            StoredTranscript::Redirect(_)
        ));
    }

    #[test]
    fn torn_new_scoped_authority_is_removed_and_recreated_from_legacy() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        let expected = write_history(&global, PREVIOUS_SCHEMA_VERSION, "abc", &["legacy"], None);
        let scoped = transcript_path(root.path(), "abc");
        fs::create_dir_all(scoped.parent().unwrap()).unwrap();
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: "abc".into(),
            generation: 1,
            workspace_root: Some(canonical_workspace(&project_root(root.path()))),
            item: None,
            replacement: Some(expected.clone()),
            redirect: None,
        };
        let encoded = serde_json::to_vec(&record).unwrap();
        fs::write(&scoped, &encoded[..encoded.len() / 2]).unwrap();

        assert!(load(root.path(), "abc").is_err());
        let opened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(opened.transcript, expected);
        drop(opened);
        assert_eq!(load(root.path(), "abc").unwrap(), expected);
    }

    #[test]
    fn torn_redirect_is_truncated_and_retried() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        let expected = write_history(
            &global,
            PREVIOUS_SCHEMA_VERSION,
            "abc",
            &["shared"],
            Some(canonical_workspace(&project_root(root.path()))),
        );
        let scoped = transcript_path(root.path(), "abc");
        write_history(
            &scoped,
            SCHEMA_VERSION,
            "abc",
            &["shared"],
            Some(canonical_workspace(&project_root(root.path()))),
        );
        let record = Record {
            schema_version: REDIRECT_SCHEMA_VERSION,
            session_id: "abc".into(),
            generation: 2,
            workspace_root: Some(canonical_workspace(&project_root(root.path()))),
            item: None,
            replacement: None,
            redirect: Some(normalized_absolute(&scoped).unwrap()),
        };
        let encoded = serde_json::to_vec(&record).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&global)
            .unwrap()
            .write_all(&encoded[..encoded.len() / 2])
            .unwrap();

        assert!(load(root.path(), "abc").is_err());
        let opened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(opened.transcript, expected);
        drop(opened);
        assert!(matches!(
            read_records_direct(&global, "abc").unwrap(),
            StoredTranscript::Redirect(_)
        ));
        assert_eq!(load(root.path(), "abc").unwrap(), expected);
    }

    #[test]
    fn downgraded_reader_rejects_tombstone_before_write_and_reupgrade_resumes() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        write_history(&global, PREVIOUS_SCHEMA_VERSION, "abc", &["original"], None);
        drop(open(root.path(), "abc", true, false, Vec::new()).unwrap());
        let before = fs::read(&global).unwrap();

        let downgraded_append = || -> Result<(), String> {
            let mut generation = 0;
            for line in BufReader::new(File::open(&global).unwrap()).lines() {
                let record: Record = serde_json::from_str(&line.unwrap()).unwrap();
                if record.schema_version > SCHEMA_VERSION {
                    return Err("unsupported schema".into());
                }
                generation = record.generation;
            }
            write_migration_record(
                &global,
                &Record {
                    schema_version: SCHEMA_VERSION,
                    session_id: "abc".into(),
                    generation: generation + 1,
                    workspace_root: None,
                    item: Some(Item::text(ItemKind::User, "downgraded write")),
                    replacement: None,
                    redirect: None,
                },
                false,
            )
        };
        assert_eq!(downgraded_append().unwrap_err(), "unsupported schema");
        assert_eq!(fs::read(&global).unwrap(), before);

        let reopened = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(item_text(&reopened.transcript[0]), "original");
    }

    #[test]
    fn migration_honors_global_and_workspace_local_mixed_version_locks() {
        let root = tempfile::tempdir().unwrap();
        let global = super::transcript_path(&session_directory(root.path()), "abc");
        let local = super::transcript_path(&legacy_directory(root.path()), "abc");
        write_history(&global, PREVIOUS_SCHEMA_VERSION, "abc", &["same"], None);
        write_history(&local, LEGACY_SCHEMA_VERSION, "abc", &["same"], None);

        for lock_path in [global.with_extension("lock"), local.with_extension("lock")] {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .unwrap();
            lock.try_lock().unwrap();
            assert!(
                open(root.path(), "abc", true, false, Vec::new())
                    .err()
                    .unwrap()
                    .contains("legacy session is actively locked")
            );
            lock.unlock().unwrap();
            drop(lock);
            fs::remove_file(lock_path).unwrap();
        }

        assert!(open(root.path(), "abc", true, false, Vec::new()).is_ok());
    }

    #[test]
    fn divergent_scoped_and_workspace_local_histories_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let global = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "global")],
        )
        .unwrap();
        drop(global);
        let legacy = project_root(root.path()).join(".kit/sessions");
        fs::create_dir_all(&legacy).unwrap();
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: "abc".into(),
            generation: 1,
            workspace_root: None,
            item: Some(Item::text(ItemKind::System, "legacy")),
            replacement: None,
            redirect: None,
        };
        fs::write(
            legacy.join("abc.jsonl"),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();

        let error = load(root.path(), "abc").unwrap_err();
        assert!(error.contains("divergent session histories"));
        assert!(
            open(root.path(), "abc", true, false, Vec::new())
                .err()
                .unwrap()
                .contains("divergent session histories")
        );
    }

    #[test]
    fn writer_reconstructs_deleted_storage_from_open_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "before"));

        fs::remove_dir_all(session_directory(root.path())).unwrap();
        write(&opened.observer, &Item::text(ItemKind::Assistant, "after"));
        write(&opened.observer, &Item::text(ItemKind::User, "continued"));

        assert!(session_lock_path(root.path(), "abc").is_file());
        assert_eq!(stored(root.path()).len(), 4);
        drop(opened);
        assert_eq!(
            open(root.path(), "abc", true, false, Vec::new())
                .unwrap()
                .transcript
                .len(),
            4
        );
    }

    #[test]
    fn writer_reconstructs_a_deleted_transcript_while_keeping_its_lock() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "before"));
        fs::remove_file(transcript_path(root.path(), "abc")).unwrap();

        write(&opened.observer, &Item::text(ItemKind::Assistant, "after"));

        assert_eq!(stored(root.path()).len(), 3);
        assert!(session_lock_path(root.path(), "abc").is_file());
    }

    #[test]
    fn list_ids_ignores_non_transcripts_and_sorts_valid_ids() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("zeta.jsonl"), "transcript").unwrap();
        fs::write(directory.path().join("alpha.jsonl"), "transcript").unwrap();
        fs::write(directory.path().join("active.lock"), "lock").unwrap();
        fs::write(directory.path().join("bad id.jsonl"), "invalid").unwrap();
        fs::create_dir(directory.path().join("nested.jsonl")).unwrap();

        assert_eq!(list_ids_in(directory.path()).unwrap(), ["alpha", "zeta"]);
        let not_directory = directory.path().join("plain-file");
        fs::write(&not_directory, "not a directory").unwrap();
        assert!(
            list_ids_in(&not_directory)
                .unwrap_err()
                .contains("could not list session directory")
        );
    }

    #[test]
    fn catalog_text_omits_terminal_controls_and_skips_control_only_parts() {
        let items = vec![
            Item::text(ItemKind::User, "\u{1b}\u{7}"),
            Item::text(ItemKind::User, "Safe\u{1b}[31m title\u{7}"),
        ];

        let (title, preview) = catalog_text(&[], &items);
        assert_eq!(title.as_deref(), Some("Safe[31m title"));
        assert_eq!(preview.as_deref(), Some("Safe[31m title"));
        assert!(
            title
                .iter()
                .chain(&preview)
                .all(|text| !text.chars().any(char::is_control))
        );
    }

    #[test]
    fn catalog_is_workspace_filtered_newest_first_and_extracts_display_text() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let now = Timestamp::now().0;

        let older = open_in(
            &first,
            storage.path(),
            "older",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(
            &older.observer,
            &Item::text(ItemKind::User, "  First title  \nwith a useful preview  ")
                .with_created_at(Timestamp(now + 1_000)),
        );
        let newer = open_in(
            &first,
            storage.path(),
            "newer",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(
            &newer.observer,
            &Item::text(ItemKind::User, "Newest session").with_created_at(Timestamp(now + 2_000)),
        );
        let other = open_in(
            &second,
            storage.path(),
            "other",
            false,
            false,
            vec![Item::text(ItemKind::User, "Other workspace")],
        )
        .unwrap();

        let entries = catalog_for_workspace(&first, storage.path()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert_eq!(entries[1].title.as_deref(), Some("First title"));
        assert_eq!(
            entries[1].preview.as_deref(),
            Some("First title with a useful preview")
        );
        assert!(entries[0].updated_at > entries[1].updated_at);
        drop((older, newer, other));
    }

    #[test]
    fn catalog_omits_only_structurally_marked_subagent_sessions() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();

        let open_with_origin = |id: &str, origin: Option<serde_json::Value>| {
            let mut item = Item::text(ItemKind::System, format!("{id} system prompt"));
            if let Some(origin) = origin {
                item.metadata
                    .insert(SESSION_ORIGIN_METADATA_KEY.into(), origin);
            }
            open_in(root.path(), storage.path(), id, false, false, vec![item]).unwrap()
        };
        let legacy = open_with_origin("legacy", None);
        let top_level = open_with_origin(
            "top-level",
            Some(serde_json::Value::String(TOP_LEVEL_SESSION_ORIGIN.into())),
        );
        let malformed = open_with_origin("malformed", Some(serde_json::Value::Bool(true)));
        let subagent = open_with_origin(
            "subagent",
            Some(serde_json::Value::String(SUBAGENT_SESSION_ORIGIN.into())),
        );
        subagent
            .observer
            .replace(&[Item::text(ItemKind::System, "compacted system prompt")])
            .unwrap();

        let entries = catalog_for_workspace(root.path(), storage.path()).unwrap();
        let ids = entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"legacy"));
        assert!(ids.contains(&"top-level"));
        assert!(ids.contains(&"malformed"));
        assert!(!ids.contains(&"subagent"));
        drop((legacy, top_level, malformed, subagent));
    }

    #[test]
    fn catalog_timestamps_are_rfc3339() {
        assert_eq!(timestamp_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            timestamp_rfc3339(1_709_164_800_123),
            "2024-02-29T00:00:00.123Z"
        );
        assert_eq!(timestamp_rfc3339(u64::MAX), "9999-12-31T23:59:59.999Z");
    }

    #[test]
    fn catalog_strips_bidi_and_default_ignorable_characters() {
        let items = vec![Item::text(
            ItemKind::User,
            "safe\u{202e}txt\u{2066} zero\u{200b}width\u{ad} \
             bom\u{feff} tag\u{e0021} variation\u{fe0f}",
        )];

        let (title, preview) = catalog_text(&[], &items);
        assert_eq!(
            title.as_deref(),
            Some("safetxt zerowidth bom tag variation")
        );
        assert_eq!(title, preview);
    }

    #[test]
    fn catalog_title_survives_transcript_replacement() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let root = roots.path().join("project");
        fs::create_dir(&root).unwrap();
        let opened = open_in(
            &root,
            storage.path(),
            "compacted",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        write(
            &opened.observer,
            &Item::text(ItemKind::User, "Original title"),
        );
        opened
            .observer
            .replace(&[
                Item::text(ItemKind::System, "summary"),
                Item::text(ItemKind::User, "Newer retained prompt"),
            ])
            .unwrap();

        let entries = catalog_for_workspace(&root, storage.path()).unwrap();
        assert_eq!(entries[0].title.as_deref(), Some("Original title"));
        assert_eq!(entries[0].preview.as_deref(), Some("Newer retained prompt"));
    }

    #[test]
    fn compacted_legacy_migration_preserves_earliest_user_title() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let id = "legacy-compacted";
        let original_system = Record {
            schema_version: SCHEMA_VERSION,
            session_id: id.into(),
            generation: 1,
            workspace_root: None,
            item: Some(Item::text(ItemKind::System, "system")),
            replacement: None,
            redirect: None,
        };
        let original_user = Record {
            schema_version: SCHEMA_VERSION,
            session_id: id.into(),
            generation: 2,
            workspace_root: None,
            item: Some(Item::text(ItemKind::User, "Original title")),
            replacement: None,
            redirect: None,
        };
        let current = vec![
            Item::text(ItemKind::System, "summary"),
            Item::text(ItemKind::User, "Newer retained prompt"),
        ];
        let replacement = Record {
            schema_version: SCHEMA_VERSION,
            session_id: id.into(),
            generation: 3,
            workspace_root: None,
            item: None,
            replacement: Some(current.clone()),
            redirect: None,
        };
        let legacy = super::transcript_path(storage.path(), id);
        fs::write(
            &legacy,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&original_system).unwrap(),
                serde_json::to_string(&original_user).unwrap(),
                serde_json::to_string(&replacement).unwrap()
            ),
        )
        .unwrap();

        let resumed = open_in(root.path(), storage.path(), id, true, false, Vec::new()).unwrap();
        assert_eq!(resumed.transcript, current);
        drop(resumed);
        assert!(matches!(
            read_records_direct(&legacy, id).unwrap(),
            StoredTranscript::Redirect(_)
        ));
        let entries = catalog_for_workspace(root.path(), storage.path()).unwrap();
        assert_eq!(entries[0].title.as_deref(), Some("Original title"));
        assert_eq!(entries[0].preview.as_deref(), Some("Newer retained prompt"));
    }

    #[test]
    fn catalog_skips_damaged_and_partially_appended_transcripts() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let root = roots.path().join("project");
        fs::create_dir(&root).unwrap();
        let valid = open_in(
            &root,
            storage.path(),
            "valid",
            false,
            false,
            vec![Item::text(ItemKind::User, "Valid session")],
        )
        .unwrap();
        let directory = workspace_storage_directory(storage.path(), &canonical_workspace(&root));
        fs::write(directory.join("malformed.jsonl"), b"not json\n").unwrap();
        fs::write(directory.join("partial.jsonl"), b"{\"schema_version\":3").unwrap();

        let entries = catalog_for_workspace(&root, storage.path()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["valid"]
        );
        drop(valid);
    }

    #[test]
    fn catalog_rejects_missing_and_non_directory_roots() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let missing = roots.path().join("missing");
        assert!(catalog_for_workspace(&missing, storage.path()).is_err());

        let file = roots.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(catalog_for_workspace(&file, storage.path()).is_err());
    }

    #[test]
    fn unbound_global_is_hidden_until_explicit_resume_binds_it() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: "legacy".into(),
            generation: 1,
            workspace_root: None,
            item: Some(Item::text(ItemKind::User, "private prompt")),
            replacement: None,
            redirect: None,
        };
        fs::write(
            super::transcript_path(storage.path(), "legacy"),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();

        assert!(
            catalog_for_workspace(&first, storage.path())
                .unwrap()
                .is_empty()
        );
        assert!(
            catalog_for_workspace(&second, storage.path())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            item_text(&load_in(&first, storage.path(), "legacy").unwrap()[0]),
            "private prompt"
        );
        let resumed = open_in(&first, storage.path(), "legacy", true, false, Vec::new()).unwrap();
        drop(resumed);
        assert_eq!(
            catalog_for_workspace(&first, storage.path()).unwrap()[0].id,
            "legacy"
        );
        assert!(
            catalog_for_workspace(&second, storage.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn catalog_rejects_conflicting_workspace_metadata_without_hiding_valid_sessions() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let first = canonical_workspace(&first);
        let second = canonical_workspace(&second);
        let records = [
            Record {
                schema_version: SCHEMA_VERSION,
                session_id: "conflict".into(),
                generation: 1,
                workspace_root: Some(first.clone()),
                item: Some(Item::text(ItemKind::User, "first private prompt")),
                replacement: None,
                redirect: None,
            },
            Record {
                schema_version: SCHEMA_VERSION,
                session_id: "conflict".into(),
                generation: 2,
                workspace_root: Some(second.clone()),
                item: Some(Item::text(ItemKind::User, "second private prompt")),
                replacement: None,
                redirect: None,
            },
        ];
        let transcript = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(
            super::transcript_path(storage.path(), "conflict"),
            transcript,
        )
        .unwrap();
        let valid = open_in(
            &first,
            storage.path(),
            "valid",
            false,
            false,
            vec![Item::text(ItemKind::User, "valid")],
        )
        .unwrap();

        assert_eq!(
            catalog_for_workspace(&first, storage.path()).unwrap()[0].id,
            "valid"
        );
        assert!(
            catalog_for_workspace(&second, storage.path())
                .unwrap()
                .is_empty()
        );
        drop(valid);
    }

    #[test]
    fn load_and_resume_ignore_unbound_global_on_a_scoped_id_collision() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let id = "collision";
        let global = Record {
            schema_version: SCHEMA_VERSION,
            session_id: id.into(),
            generation: 1,
            workspace_root: None,
            item: Some(Item::text(ItemKind::User, "private global prompt")),
            replacement: None,
            redirect: None,
        };
        fs::write(
            super::transcript_path(storage.path(), id),
            format!("{}\n", serde_json::to_string(&global).unwrap()),
        )
        .unwrap();
        let scoped_directory =
            workspace_storage_directory(storage.path(), &canonical_workspace(root.path()));
        fs::create_dir_all(&scoped_directory).unwrap();
        let scoped = Record {
            workspace_root: Some(canonical_workspace(root.path())),
            item: Some(Item::text(ItemKind::User, "visible scoped prompt")),
            ..global
        };
        fs::write(
            super::transcript_path(&scoped_directory, id),
            format!("{}\n", serde_json::to_string(&scoped).unwrap()),
        )
        .unwrap();

        let entries = catalog_for_workspace(root.path(), storage.path()).unwrap();
        assert_eq!(entries[0].title.as_deref(), Some("visible scoped prompt"));
        assert_eq!(
            item_text(&load_in(root.path(), storage.path(), id).unwrap()[0]),
            "visible scoped prompt"
        );
        let resumed = open_in(root.path(), storage.path(), id, true, false, Vec::new()).unwrap();
        assert_eq!(item_text(&resumed.transcript[0]), "visible scoped prompt");
        drop(resumed);

        let StoredTranscript::History(history) =
            read_records_direct(&super::transcript_path(storage.path(), id), id).unwrap()
        else {
            panic!("unbound global collision was redirected");
        };
        assert_eq!(item_text(&history.items[0]), "private global prompt");
    }

    #[test]
    fn identical_ids_round_trip_independently_across_workspaces() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        let first_open = open_in(
            &first,
            storage.path(),
            "shared",
            false,
            false,
            vec![Item::text(ItemKind::System, "first")],
        )
        .unwrap();
        let second_open = open_in(
            &second,
            storage.path(),
            "shared",
            false,
            false,
            vec![Item::text(ItemKind::System, "second")],
        )
        .unwrap();
        write(
            &first_open.observer,
            &Item::text(ItemKind::User, "first-only"),
        );
        write(
            &second_open.observer,
            &Item::text(ItemKind::User, "second-only"),
        );

        let first_path = super::transcript_path(
            &workspace_storage_directory(storage.path(), &canonical_workspace(&first)),
            "shared",
        );
        let second_path = super::transcript_path(
            &workspace_storage_directory(storage.path(), &canonical_workspace(&second)),
            "shared",
        );
        assert_ne!(first_path, second_path);
        assert!(first_path.with_extension("lock").is_file());
        assert!(second_path.with_extension("lock").is_file());
        assert_eq!(
            item_text(&load_in(&first, storage.path(), "shared").unwrap()[0]),
            "first"
        );
        assert_eq!(
            item_text(&load_in(&second, storage.path(), "shared").unwrap()[0]),
            "second"
        );
        drop(first_open);
        drop(second_open);

        let first_resumed =
            open_in(&first, storage.path(), "shared", true, false, Vec::new()).unwrap();
        let second_resumed =
            open_in(&second, storage.path(), "shared", true, false, Vec::new()).unwrap();
        assert_eq!(item_text(&first_resumed.transcript[1]), "first-only");
        assert_eq!(item_text(&second_resumed.transcript[1]), "second-only");
        assert_eq!(
            list_ids_for_workspace(&first, storage.path()).unwrap(),
            ["shared"]
        );
        assert_eq!(
            list_ids_for_workspace(&second, storage.path()).unwrap(),
            ["shared"]
        );
    }

    #[test]
    fn unscoped_transcript_migrates_without_blocking_same_id_in_another_workspace() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let item = Item::text(ItemKind::System, "unscoped").with_created_at(Timestamp(7));
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: "legacy-id".into(),
            generation: 1,
            workspace_root: Some(canonical_workspace(&first)),
            item: Some(item.clone()),
            replacement: None,
            redirect: None,
        };
        let unscoped = super::transcript_path(storage.path(), "legacy-id");
        fs::write(
            &unscoped,
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();

        assert_eq!(
            load_in(&first, storage.path(), "legacy-id").unwrap(),
            [item]
        );
        assert!(load_in(&second, storage.path(), "legacy-id").is_err());
        assert_eq!(
            list_ids_for_workspace(&first, storage.path()).unwrap(),
            ["legacy-id"]
        );
        assert!(
            list_ids_for_workspace(&second, storage.path())
                .unwrap()
                .is_empty()
        );
        let migrated =
            open_in(&first, storage.path(), "legacy-id", true, false, Vec::new()).unwrap();
        drop(migrated);
        assert!(unscoped.is_file(), "migration must retain the old artifact");
        let scoped = super::transcript_path(
            &workspace_storage_directory(storage.path(), &canonical_workspace(&first)),
            "legacy-id",
        );
        assert!(scoped.is_file());

        let second_open = open_in(
            &second,
            storage.path(),
            "legacy-id",
            false,
            false,
            vec![Item::text(ItemKind::System, "second")],
        )
        .unwrap();
        assert_eq!(item_text(&second_open.transcript[0]), "second");
        assert_eq!(
            list_ids_for_workspace(&first, storage.path()).unwrap(),
            ["legacy-id"]
        );
        assert_eq!(
            list_ids_for_workspace(&second, storage.path()).unwrap(),
            ["legacy-id"]
        );
    }

    #[test]
    fn legacy_transcript_is_listed_only_in_its_project_and_binds_on_resume() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let first = roots.path().join("first");
        let second = roots.path().join("second");
        let legacy = workspace_directory(&first);
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&second).unwrap();
        let record = serde_json::json!({
            "schema_version": PREVIOUS_SCHEMA_VERSION,
            "session_id": "legacy",
            "generation": 1,
            "item": Item::text(ItemKind::System, "legacy"),
        });
        fs::write(legacy.join("legacy.jsonl"), format!("{record}\n")).unwrap();

        assert_eq!(
            list_ids_for_workspace(&first, storage.path()).unwrap(),
            ["legacy"]
        );
        assert!(
            list_ids_for_workspace(&second, storage.path())
                .unwrap()
                .is_empty()
        );
        assert!(belongs_to_workspace_in(&first, storage.path(), "legacy").unwrap());

        let opened = open_in(&first, storage.path(), "legacy", true, false, Vec::new()).unwrap();
        drop(opened);
        let migrated = super::transcript_path(
            &workspace_storage_directory(storage.path(), &canonical_workspace(&first)),
            "legacy",
        );
        assert_eq!(
            transcript_workspace(&migrated, "legacy").unwrap(),
            Some(canonical_workspace(&first))
        );
        assert!(!belongs_to_workspace_in(&second, storage.path(), "legacy").unwrap());
    }

    #[test]
    fn writer_fails_closed_when_another_owner_wins_recovery_lock() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        fs::remove_dir_all(session_directory(root.path())).unwrap();
        fs::create_dir_all(scoped_directory(root.path())).unwrap();
        let other = SessionLock::acquire(session_lock_path(root.path(), "abc"), false).unwrap();
        let item = Item::text(ItemKind::User, "must not persist").with_created_at(Timestamp(9));

        let error = opened.observer.0.lock().unwrap().append(&item).unwrap_err();

        assert!(error.contains("overridden by another Kit instance"));
        assert!(!transcript_path(root.path(), "abc").exists());
        drop(other);
    }
}

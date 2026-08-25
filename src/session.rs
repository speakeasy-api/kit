//! Durable, append-only session transcripts and their filesystem lock.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agentkit_core::{Item, Timestamp};
use agentkit_loop::{TranscriptEvent, TranscriptObserver};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 3;
const PREVIOUS_SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
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
}

/// A loaded transcript together with the observer that owns its mutation lock.
pub struct OpenSession {
    pub transcript: Vec<Item>,
    pub observer: SessionObserver,
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
    let path = preferred_transcript(directory, &workspace_root, session_id)?;
    let (mut items, _) = read_records(&path, session_id)?;
    ensure_workspace(&path, session_id, &workspace_root)?;
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
    let legacy = legacy_transcript_for_workspace(directory, &workspace_root, session_id)?;
    let lock = SessionLock::acquire(lock_path(&scoped_directory, session_id), force)?;
    let scoped_exists = path
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if resume && !scoped_exists {
        let legacy = legacy
            .as_ref()
            .ok_or_else(|| format!("session {session_id:?} does not exist"))?;
        let _legacy_lock = lock_legacy_for_migration(legacy)?;
        read_records(legacy, session_id)?;
        copy_new(legacy, &path)?;
    } else if !resume && (scoped_exists || legacy.is_some()) {
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
        if file.try_lock().is_err() {
            if !force {
                remove_failed_lock(&path, file);
            }
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
        let current = fs::read_to_string(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                LockError::Missing
            } else {
                LockError::Other(format!("session lock was lost: {error}"))
            }
        })?;
        if current == self.token {
            Ok(())
        } else {
            Err(LockError::Other(
                "session lock was overridden by another Kit instance".into(),
            ))
        }
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

fn read_records(path: &Path, session_id: &str) -> Result<(Vec<Item>, u64), String> {
    let file =
        File::open(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut items = Vec::new();
    let mut expected = 1_u64;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.map_err(|error| format!("could not read transcript line {}: {error}", index + 1))?;
        let record: Record = serde_json::from_str(&line)
            .map_err(|error| format!("invalid transcript line {}: {error}", index + 1))?;
        if !matches!(
            record.schema_version,
            LEGACY_SCHEMA_VERSION | PREVIOUS_SCHEMA_VERSION | SCHEMA_VERSION
        ) {
            return Err(format!(
                "unsupported session schema version {} on line {} (Kit supports {})",
                record.schema_version,
                index + 1,
                SCHEMA_VERSION
            ));
        }
        if record.session_id != session_id || record.generation != expected {
            return Err(format!(
                "invalid session identity or generation on transcript line {}",
                index + 1
            ));
        }
        match (record.item, record.replacement) {
            (Some(item), None) => items.push(item),
            (None, Some(replacement))
                if record.schema_version >= PREVIOUS_SCHEMA_VERSION && !replacement.is_empty() =>
            {
                items = replacement;
            }
            _ => {
                return Err(format!(
                    "transcript line {} must contain exactly one item or replacement",
                    index + 1
                ));
            }
        }
        expected += 1;
    }
    if items.is_empty() {
        return Err(format!("session transcript {} is empty", path.display()));
    }
    Ok((items, expected - 1))
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
    let file =
        File::open(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut workspace = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.map_err(|error| format!("could not read transcript line {}: {error}", index + 1))?;
        let record: Record = serde_json::from_str(&line)
            .map_err(|error| format!("invalid transcript line {}: {error}", index + 1))?;
        if record.session_id != session_id {
            return Err(format!(
                "invalid session identity on transcript line {}",
                index + 1
            ));
        }
        if record.workspace_root.is_some() {
            workspace = record.workspace_root;
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

/// Lists durable transcript ids bound to one workspace without taking mutation locks.
pub(crate) fn list_ids(root: &Path) -> Result<Vec<String>, String> {
    list_ids_for_workspace(root, &default_directory()?)
}

fn list_ids_for_workspace(root: &Path, global_directory: &Path) -> Result<Vec<String>, String> {
    let root = canonical_workspace(root);
    let scoped_directory = workspace_storage_directory(global_directory, &root);
    let legacy_directory = workspace_directory(&root);
    let mut ids = Vec::new();
    for id in list_ids_in(&scoped_directory)? {
        let path = transcript_path(&scoped_directory, &id);
        read_records(&path, &id)?;
        ensure_workspace(&path, &id, &root)?;
        ids.push(id);
    }
    for id in list_ids_in(global_directory)? {
        let path = transcript_path(global_directory, &id);
        read_records(&path, &id)?;
        let legacy = legacy_transcript(&root, &id);
        let legacy_exists = legacy
            .try_exists()
            .map_err(|error| format!("could not inspect {}: {error}", legacy.display()))?;
        let stored_workspace = transcript_workspace(&path, &id)?;
        if stored_workspace.as_deref() == Some(root.as_path())
            || stored_workspace.is_none() && legacy_exists
        {
            ids.push(id);
        }
    }
    for id in list_ids_in(&legacy_directory)? {
        let path = transcript_path(&legacy_directory, &id);
        read_records(&path, &id)?;
        ids.push(id);
    }
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
    let scoped = transcript_path(
        &workspace_storage_directory(global_directory, &root),
        session_id,
    );
    if scoped
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", scoped.display()))?
    {
        read_records(&scoped, session_id)?;
        ensure_workspace(&scoped, session_id, &root)?;
        return Ok(true);
    }
    Ok(legacy_transcript_for_workspace(global_directory, &root, session_id)?.is_some())
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
        let entry = entry.map_err(|error| format!("could not read session entry: {error}"))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
            .is_file()
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

fn preferred_transcript(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<PathBuf, String> {
    let scoped = transcript_path(&workspace_storage_directory(directory, root), session_id);
    if scoped
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", scoped.display()))?
    {
        return Ok(scoped);
    }
    Ok(legacy_transcript_for_workspace(directory, root, session_id)?.unwrap_or(scoped))
}

fn legacy_transcript_for_workspace(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>, String> {
    let unscoped = transcript_path(directory, session_id);
    let legacy = legacy_transcript(root, session_id);
    let legacy_exists = legacy
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", legacy.display()))?;
    if unscoped
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", unscoped.display()))?
    {
        read_records(&unscoped, session_id)?;
        match transcript_workspace(&unscoped, session_id)? {
            Some(stored) if stored == root => return Ok(Some(unscoped)),
            None if legacy_exists => return Ok(Some(legacy)),
            _ => {}
        }
    }
    if legacy_exists {
        read_records(&legacy, session_id)?;
        Ok(Some(legacy))
    } else {
        Ok(None)
    }
}

fn lock_legacy_for_migration(transcript: &Path) -> Result<Option<File>, String> {
    let path = transcript.with_extension("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not inspect legacy session lock {}: {error}",
                path.display()
            ));
        }
    };
    file.try_lock().map_err(|_| {
        format!(
            "legacy session is actively locked by another Kit instance ({}); stop it before resuming with this Kit version",
            path.display()
        )
    })?;
    Ok(Some(file))
}

fn copy_new(source: &Path, destination: &Path) -> Result<(), String> {
    let mut source =
        File::open(source).map_err(|error| format!("could not read legacy session: {error}"))?;
    let mut copied = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            format!(
                "could not copy legacy session to {}: {error}",
                destination.display()
            )
        })?;
    if let Err(error) = io::copy(&mut source, &mut copied).and_then(|_| copied.sync_all()) {
        let _ = fs::remove_file(destination);
        return Err(format!(
            "could not copy legacy session to {}: {error}",
            destination.display()
        ));
    }
    Ok(())
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
    fn global_transcript_wins_over_legacy() {
        let root = tempfile::tempdir().unwrap();
        let global = open(
            root.path(),
            "abc",
            false,
            false,
            vec![Item::text(ItemKind::System, "global")],
        )
        .unwrap();
        let expected = global.transcript.clone();
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
        };
        fs::write(
            legacy.join("abc.jsonl"),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();

        assert_eq!(load(root.path(), "abc").unwrap(), expected);
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

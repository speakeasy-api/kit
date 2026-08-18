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

pub const SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    session_id: String,
    generation: u64,
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
    file: File,
    lock: SessionLock,
}

struct SessionLock {
    path: PathBuf,
    token: String,
    // Retaining the OS lock closes the token check/write race.
    _file: File,
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
    let path = preferred_transcript(directory, root, session_id)?;
    let (mut items, _) = read_records(&path, session_id)?;
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
pub fn remove_stale_lock(_root: &Path, session_id: &str) -> Result<(), String> {
    remove_stale_lock_in(&default_directory()?, session_id)
}

pub(crate) fn remove_stale_lock_in(directory: &Path, session_id: &str) -> Result<(), String> {
    validate_id(session_id)?;
    let path = lock_path(directory, session_id);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not inspect session lock: {error}")),
    };
    file.try_lock()
        .map_err(|_| "session lock is still held by a live Kit instance".to_string())?;
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
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create session directory: {error}"))?;
    let path = transcript_path(directory, session_id);
    let legacy = legacy_transcript(root, session_id);
    let lock = SessionLock::acquire(lock_path(directory, session_id), force)?;
    let global_exists = path
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    let legacy_exists = legacy
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", legacy.display()))?;
    if resume && !global_exists {
        if !legacy_exists {
            return Err(format!("session {session_id:?} does not exist"));
        }
        let _legacy_lock = lock_legacy_for_migration(&legacy)?;
        read_records(&legacy, session_id)?;
        copy_new(&legacy, &path)?;
    } else if !resume && (global_exists || legacy_exists) {
        return Err(format!(
            "session {session_id:?} already exists; use --resume"
        ));
    }
    let (mut transcript, mut generation) = if resume {
        read_records(&path, session_id)?
    } else {
        (Vec::new(), 0)
    };
    let file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut writer = Writer {
        session_id: session_id.into(),
        generation,
        path,
        file,
        lock,
    };
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
        file.try_lock().map_err(|_| {
            format!(
                "session is actively locked by another Kit instance ({})",
                path.display()
            )
        })?;
        file.set_len(0)
            .and_then(|_| file.write_all(token.as_bytes()))
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("could not write session lock: {error}"))?;
        Ok(Self {
            path,
            token,
            _file: file,
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

impl Drop for SessionLock {
    fn drop(&mut self) {
        if self.check().is_ok() {
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
            LEGACY_SCHEMA_VERSION | SCHEMA_VERSION
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
                if record.schema_version == SCHEMA_VERSION && !replacement.is_empty() =>
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

fn default_directory() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".kit/sessions"))
        .ok_or_else(|| "HOME is unset; cannot locate durable sessions".into())
}

fn preferred_transcript(
    directory: &Path,
    root: &Path,
    session_id: &str,
) -> Result<PathBuf, String> {
    let global = transcript_path(directory, session_id);
    if global
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", global.display()))?
    {
        Ok(global)
    } else {
        Ok(legacy_transcript(root, session_id))
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

fn transcript_path(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(format!("{session_id}.jsonl"))
}

fn legacy_transcript(root: &Path, session_id: &str) -> PathBuf {
    transcript_path(&root.join(".kit/sessions"), session_id)
}

fn lock_path(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(format!("{session_id}.lock"))
}

fn validate_id(value: &str) -> Result<(), String> {
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
        remove_stale_lock_in(&session_directory(root), session_id)
    }

    fn transcript_path(root: &Path, session_id: &str) -> PathBuf {
        super::transcript_path(&session_directory(root), session_id)
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
        assert!(text.contains("\"schema_version\":2"));
        assert!(text.contains("\"generation\":2"));
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
        let directory = session_directory(root.path());
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
        let directory = session_directory(root.path());
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
        fs::write(session_directory(root.path()).join("abc.lock"), "abandoned").unwrap();
        remove_stale_lock(root.path(), "abc").unwrap();
        assert!(!session_directory(root.path()).join("abc.lock").exists());
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

        assert!(session_directory(root.path()).join("abc.lock").is_file());
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
        assert!(session_directory(root.path()).join("abc.lock").is_file());
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
        fs::create_dir_all(session_directory(root.path())).unwrap();
        let other = SessionLock::acquire(
            super::lock_path(&session_directory(root.path()), "abc"),
            false,
        )
        .unwrap();
        let item = Item::text(ItemKind::User, "must not persist").with_created_at(Timestamp(9));

        let error = opened.observer.0.lock().unwrap().append(&item).unwrap_err();

        assert!(error.contains("overridden by another Kit instance"));
        assert!(!transcript_path(root.path(), "abc").exists());
        drop(other);
    }
}

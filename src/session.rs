//! Durable, append-only session transcripts and their filesystem lock.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agentkit_core::Item;
use agentkit_loop::{TranscriptEvent, TranscriptObserver};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    session_id: String,
    generation: u64,
    item: Item,
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
pub fn load(root: &Path, session_id: &str) -> Result<Vec<Item>, String> {
    validate_id(session_id)?;
    read_records(&transcript_path(root, session_id), session_id).map(|(items, _)| items)
}

/// Removes an abandoned lock file, but never one held by a live process.
///
/// This is the last-resort cleanup path for a hosting client whose server had
/// to be killed before normal `SessionLock` destruction completed.
pub fn remove_stale_lock(root: &Path, session_id: &str) -> Result<(), String> {
    validate_id(session_id)?;
    let path = root
        .join(".kit/sessions")
        .join(format!("{session_id}.lock"));
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
    initial: Item,
) -> Result<OpenSession, String> {
    validate_id(session_id)?;
    let directory = root.join(".kit/sessions");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create session directory: {error}"))?;
    let path = transcript_path(root, session_id);
    if resume && !path.is_file() {
        return Err(format!("session {session_id:?} does not exist"));
    }
    if !resume && path.exists() {
        return Err(format!(
            "session {session_id:?} already exists; use --resume"
        ));
    }
    let lock = SessionLock::acquire(directory.join(format!("{session_id}.lock")), force)?;
    let (mut transcript, mut generation) = if resume {
        read_records(&path, session_id)?
    } else {
        (Vec::new(), 0)
    };
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut writer = Writer {
        session_id: session_id.into(),
        generation,
        file,
        lock,
    };
    if !resume {
        writer.append(&initial)?;
        generation = writer.generation;
        transcript.push(initial);
        debug_assert_eq!(generation, 1);
    }
    Ok(OpenSession {
        transcript,
        observer: SessionObserver(Arc::new(Mutex::new(writer))),
    })
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
        self.lock.check()?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| "session generation overflowed".to_string())?;
        let record = Record {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            generation,
            item: item.clone(),
        };
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

    fn check(&self) -> Result<(), String> {
        let current = fs::read_to_string(&self.path)
            .map_err(|error| format!("session lock was lost: {error}"))?;
        if current == self.token {
            Ok(())
        } else {
            Err("session lock was overridden by another Kit instance".into())
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
        if record.schema_version != SCHEMA_VERSION {
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
        items.push(record.item);
        expected += 1;
    }
    if items.is_empty() {
        return Err(format!("session transcript {} is empty", path.display()));
    }
    Ok((items, expected - 1))
}

fn transcript_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(".kit/sessions")
        .join(format!("{session_id}.jsonl"))
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
    use agentkit_core::ItemKind;

    #[test]
    fn appends_versioned_generations_and_resumes() {
        let root = tempfile::tempdir().unwrap();
        let opened = open(
            root.path(),
            "abc",
            false,
            false,
            Item::text(ItemKind::System, "system"),
        )
        .unwrap();
        opened.observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("abc"),
            item: &Item::text(ItemKind::User, "hello"),
        });
        drop(opened);
        let resumed = open(
            root.path(),
            "abc",
            true,
            false,
            Item::text(ItemKind::System, "ignored"),
        )
        .unwrap();
        assert_eq!(resumed.transcript.len(), 2);
        let text = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert!(text.contains("\"schema_version\":1"));
        assert!(text.contains("\"generation\":2"));
    }

    #[test]
    fn lock_requires_explicit_override_and_only_reclaims_stale_locks() {
        let root = tempfile::tempdir().unwrap();
        let first = open(
            root.path(),
            "abc",
            false,
            false,
            Item::text(ItemKind::System, "one"),
        )
        .unwrap();
        assert!(
            open(
                root.path(),
                "abc",
                true,
                false,
                Item::text(ItemKind::System, "x")
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
                Item::text(ItemKind::System, "x")
            )
            .is_err(),
            "force must not steal authority from a live owner"
        );
        drop(first);
        fs::write(root.path().join(".kit/sessions/abc.lock"), "abandoned").unwrap();
        remove_stale_lock(root.path(), "abc").unwrap();
        assert!(!root.path().join(".kit/sessions/abc.lock").exists());
        assert!(
            open(
                root.path(),
                "abc",
                true,
                false,
                Item::text(ItemKind::System, "x")
            )
            .is_ok()
        );
    }
}

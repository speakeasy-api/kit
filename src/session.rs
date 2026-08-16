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
    validate_id(session_id)?;
    let (mut items, _) = read_records(&transcript_path(root, session_id), session_id)?;
    crate::transcript::repair_unanswered_tool_calls(&mut items);
    Ok(items)
}

/// Copies a completed transcript into a new durable session.
///
/// The source remains owned by its ACP child; callers serialize this operation
/// with prompts so the read is a stable, completed-turn snapshot.
pub fn clone_completed(root: &Path, source: &str, destination: &str) -> Result<(), String> {
    let transcript = load(root, source)?;
    let opened = open(root, destination, false, false, transcript)?;
    drop(opened);
    Ok(())
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
    initial: Vec<Item>,
) -> Result<OpenSession, String> {
    validate_id(session_id)?;
    if !resume && initial.is_empty() {
        return Err("a new session requires an initial transcript".into());
    }
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
        for item in initial {
            writer.append(&item)?;
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
        self.lock.check()?;
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
        self.lock.check()?;
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
    use agentkit_core::{ItemKind, Part};

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
            item: &Item::text(ItemKind::User, "hello"),
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
        let text = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert!(text.contains("\"schema_version\":2"));
        assert!(text.contains("\"generation\":2"));
    }

    #[test]
    fn reads_legacy_schema_one_records() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".kit/sessions");
        fs::create_dir_all(&directory).unwrap();
        let item = Item::text(ItemKind::System, "legacy");
        let line = serde_json::json!({
            "schema_version": LEGACY_SCHEMA_VERSION,
            "session_id": "abc",
            "generation": 1,
            "item": item,
        });
        fs::write(directory.join("abc.jsonl"), format!("{line}\n")).unwrap();

        let loaded = load(root.path(), "abc").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].kind, ItemKind::System);
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
        let replacement = vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::Context, "summary"),
        ];
        opened.observer.replace(&replacement).unwrap();
        write(&opened.observer, &Item::text(ItemKind::User, "after"));
        drop(opened);

        let resumed = open(root.path(), "abc", true, false, Vec::new()).unwrap();
        assert_eq!(resumed.transcript.len(), 3);
        assert_eq!(resumed.transcript[1].kind, ItemKind::Context);
        assert_eq!(resumed.transcript[2].kind, ItemKind::User);
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
        observer.on_transcript_event(TranscriptEvent {
            session_id: &agentkit_core::SessionId::new("abc"),
            item,
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
            opened.observer.on_transcript_event(TranscriptEvent {
                session_id: &agentkit_core::SessionId::new("abc"),
                item: &item,
            });
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
        drop(resumed);

        // The repair was written back, so a later resume finds nothing to fix
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
        let text = fs::read_to_string(transcript_path(root.path(), "abc")).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(text.contains("\"generation\":4"));
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
        fs::write(root.path().join(".kit/sessions/abc.lock"), "abandoned").unwrap();
        remove_stale_lock(root.path(), "abc").unwrap();
        assert!(!root.path().join(".kit/sessions/abc.lock").exists());
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
}

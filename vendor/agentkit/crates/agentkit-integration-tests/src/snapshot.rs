//! RON-backed snapshot harness for end-to-end tests.
//!
//! A test scenario is captured as a single [`SessionRecording`] in a
//! `tests/snapshots/<name>.ron` file. The file is the source of truth for
//! both directions of a test:
//!
//! - The mock model's scripted output (`turns[].events`) drives what the
//!   "model" emits each turn.
//! - Everything else — the input transcript handed to the model on each
//!   begin_turn, the tool catalog visible to it, the final transcript —
//!   is captured at runtime and compared against the same file at the end
//!   of the test.
//!
//! On mismatch, a pretty line-diff over RON-pretty-printed recordings is
//! printed via `pretty_assertions`. To rewrite the file with the observed
//! recording, set `UPDATE_SNAPSHOTS=1` in the environment.
//!
//! ```ignore
//! UPDATE_SNAPSHOTS=1 cargo test -p agentkit-integration-tests
//! ```

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentkit_core::{Item, MetadataMap, Part, ToolOutput, TurnCancellation};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
};
use agentkit_tools_core::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A complete recording of one end-to-end test session: how it started,
/// every turn the model handled, and the final transcript the loop ended up
/// with.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionRecording {
    /// Session identifier passed to [`SessionConfig::new`].
    pub session_id: String,
    /// Initial transcript items handed to `Agent::start`.
    pub initial_items: Vec<Item>,
    /// One entry per `begin_turn` call. Order matches turn order.
    pub turns: Vec<TurnRecord>,
    /// `LoopDriver::snapshot().transcript` at the end of the test.
    pub final_transcript: Vec<Item>,
}

/// One observed model turn — shaped like the wire payload of a chat
/// completions request: a transcript and the full tool catalog (specs,
/// not just names) the model can choose from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TurnRecord {
    /// Full transcript handed to the model at `begin_turn`, including any
    /// `ItemKind::System` items that act as the system prompt.
    pub input: Vec<Item>,
    /// Full tool specs the loop advertised to the model, sorted by name.
    /// Captures description and JSON schema, not just the name — the
    /// model's tool choice is conditioned on these.
    pub tools: Vec<ToolSpec>,
    /// Events the model emitted in response. Last event is always
    /// `Finished` (the runtime turn-finish heuristic).
    pub events: Vec<ModelTurnEvent>,
}

impl SessionRecording {
    /// Read a recording from `path` (RON). Panics on I/O or parse errors —
    /// these are bugs in the test setup, not test failures.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read snapshot {}: {err}", path.display()));
        ron::de::from_str(&text)
            .unwrap_or_else(|err| panic!("parse snapshot {}: {err}", path.display()))
    }

    /// Load the recording at `path`, or — if the file is missing — call
    /// `seed` to construct one in-memory. Lets tests bootstrap a fresh
    /// `.ron` file: the first run uses the seed (typically with empty
    /// `final_transcript` / observed turn metadata) and
    /// [`assert_recording`] then writes the populated recording back to
    /// disk.
    pub fn load_or_seed(path: impl AsRef<Path>, seed: impl FnOnce() -> Self) -> Self {
        let path = path.as_ref();
        if path.exists() {
            Self::load(path)
        } else {
            seed()
        }
    }

    /// Render this recording as pretty RON.
    pub fn to_pretty_ron(&self) -> String {
        let cfg = ron::ser::PrettyConfig::default()
            .depth_limit(usize::MAX)
            .struct_names(true)
            .new_line("\n".to_string())
            .indentor("    ".to_string());
        ron::ser::to_string_pretty(self, cfg).expect("serialize SessionRecording")
    }

    /// Write this recording to `path`. Creates parent directories as needed.
    pub fn save(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create snapshot dir");
        }
        let mut text = self.to_pretty_ron();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        std::fs::write(path, text).expect("write snapshot");
    }
}

#[derive(Default)]
struct AdapterState {
    expected: Vec<TurnRecord>,
    observed: Vec<TurnRecord>,
    cursor: usize,
}

/// Mock [`ModelAdapter`] driven by a [`SessionRecording`].
///
/// On each `begin_turn` call:
///
/// - The full input transcript and tool catalog the loop handed the model
///   are recorded.
/// - The next scripted [`ModelTurnEvent`]s from `recording.turns[i].events`
///   are returned verbatim.
///
/// The recording is verified at the end of the test via [`assert_recording`].
#[derive(Clone)]
pub struct SnapshotAdapter {
    state: Arc<Mutex<AdapterState>>,
}

impl SnapshotAdapter {
    /// Build an adapter scripted from `recording.turns[].events`.
    pub fn from_recording(recording: &SessionRecording) -> Self {
        Self {
            state: Arc::new(Mutex::new(AdapterState {
                expected: recording.turns.clone(),
                observed: Vec::new(),
                cursor: 0,
            })),
        }
    }

    /// Build a [`SessionRecording`] from observations: copies `session_id`
    /// and `initial_items` from `original` (the snapshot the test loaded),
    /// fills in observed turns, and stamps the driver's final transcript.
    pub fn into_recording(
        &self,
        original: &SessionRecording,
        final_transcript: Vec<Item>,
    ) -> SessionRecording {
        let state = self.state.lock().unwrap();
        SessionRecording {
            session_id: original.session_id.clone(),
            initial_items: original.initial_items.clone(),
            turns: state.observed.clone(),
            final_transcript,
        }
    }
}

#[async_trait]
impl ModelAdapter for SnapshotAdapter {
    type Session = SnapshotSession;

    async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(SnapshotSession {
            state: Arc::clone(&self.state),
        })
    }
}

/// Session counterpart to [`SnapshotAdapter`].
pub struct SnapshotSession {
    state: Arc<Mutex<AdapterState>>,
}

#[async_trait]
impl ModelSession for SnapshotSession {
    type Turn = SnapshotTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let mut state = self.state.lock().unwrap();
        let cursor = state.cursor;
        let mut tools: Vec<ToolSpec> = request.available_tools.clone();
        tools.sort_by(|a, b| a.name.0.cmp(&b.name.0));

        let events = state
            .expected
            .get(cursor)
            .map(|t| t.events.clone())
            .ok_or_else(|| {
                LoopError::InvalidState(format!(
                    "snapshot has {} scripted turns; begin_turn called for turn {cursor}",
                    state.expected.len(),
                ))
            })?;

        state.observed.push(TurnRecord {
            input: request.transcript.clone(),
            tools,
            events: events.clone(),
        });
        state.cursor += 1;

        Ok(SnapshotTurn {
            queue: events.into(),
        })
    }
}

/// Streaming turn produced by [`SnapshotSession`].
pub struct SnapshotTurn {
    queue: VecDeque<ModelTurnEvent>,
}

#[async_trait]
impl ModelTurn for SnapshotTurn {
    async fn next_event(
        &mut self,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.queue.pop_front())
    }
}

/// Resolve a snapshot path relative to the test crate's `tests/snapshots/`
/// directory. Pass the bare file name (no extension required, but `.ron`
/// is recommended).
pub fn snapshot_path(file_name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("snapshots");
    path.push(file_name);
    path
}

fn update_mode() -> bool {
    matches!(std::env::var("UPDATE_SNAPSHOTS"), Ok(v) if !v.is_empty() && v != "0")
}

/// Compare `observed` to the snapshot stored at `path`. In strict mode
/// (default), panics on mismatch with a colored line-diff. With
/// `UPDATE_SNAPSHOTS=1`, or when the file is missing (first-run
/// bootstrap), rewrites `path` with `observed` and returns successfully.
pub fn assert_recording(observed: &SessionRecording, path: impl AsRef<Path>) {
    let path = path.as_ref();
    let bootstrapping = !path.exists();
    let mut observed = observed.clone();
    normalise_recording(&mut observed);
    if update_mode() || bootstrapping {
        observed.save(path);
        eprintln!(
            "{}: wrote {}",
            if bootstrapping {
                "BOOTSTRAP"
            } else {
                "UPDATE_SNAPSHOTS"
            },
            path.display()
        );
        return;
    }
    let mut expected = SessionRecording::load(path);
    normalise_recording(&mut expected);
    let exp_text = expected.to_pretty_ron();
    let obs_text = observed.to_pretty_ron();
    if exp_text != obs_text {
        pretty_assertions::assert_eq!(
            exp_text,
            obs_text,
            "\nsnapshot mismatch: {}\n(re-run with UPDATE_SNAPSHOTS=1 to accept)",
            path.display(),
        );
    }
}

/// Strips non-deterministic fields the loop populates so snapshot comparisons
/// stay stable. Currently zeroes [`Item::created_at`] across every Item.
fn normalise_recording(recording: &mut SessionRecording) {
    recording.initial_items.iter_mut().for_each(normalise_item);
    for turn in &mut recording.turns {
        turn.input.iter_mut().for_each(normalise_item);
        for spec in &mut turn.tools {
            canonicalise_json(&mut spec.input_schema);
            if let Some(schema) = &mut spec.output_schema {
                canonicalise_json(schema);
            }
            canonicalise_metadata(&mut spec.metadata);
        }
        for event in &mut turn.events {
            match event {
                ModelTurnEvent::ToolCall(part) => {
                    canonicalise_json(&mut part.input);
                    canonicalise_metadata(&mut part.metadata);
                }
                ModelTurnEvent::Finished(result) => {
                    result.output_items.iter_mut().for_each(normalise_item);
                    canonicalise_metadata(&mut result.metadata);
                }
                ModelTurnEvent::Delta(_) | ModelTurnEvent::Usage(_) => {}
            }
        }
    }
    recording
        .final_transcript
        .iter_mut()
        .for_each(normalise_item);
}

fn normalise_item(item: &mut Item) {
    item.created_at = None;
    item.parts.iter_mut().for_each(canonicalise_part);
    canonicalise_metadata(&mut item.metadata);
}

fn canonicalise_part(part: &mut Part) {
    match part {
        Part::Text(part) => canonicalise_metadata(&mut part.metadata),
        Part::Media(part) => canonicalise_metadata(&mut part.metadata),
        Part::File(part) => canonicalise_metadata(&mut part.metadata),
        Part::Reasoning(part) => canonicalise_metadata(&mut part.metadata),
        Part::Structured(part) => {
            canonicalise_json(&mut part.value);
            if let Some(schema) = &mut part.schema {
                canonicalise_json(schema);
            }
            canonicalise_metadata(&mut part.metadata);
        }
        Part::ToolCall(part) => {
            canonicalise_json(&mut part.input);
            canonicalise_metadata(&mut part.metadata);
        }
        Part::ToolResult(part) => {
            canonicalise_output(&mut part.output);
            canonicalise_metadata(&mut part.metadata);
        }
        Part::Custom(part) => {
            if let Some(value) = &mut part.value {
                canonicalise_json(value);
            }
            canonicalise_metadata(&mut part.metadata);
        }
    }
}

fn canonicalise_output(output: &mut ToolOutput) {
    match output {
        ToolOutput::Text(_) => {}
        ToolOutput::Structured(value) => canonicalise_json(value),
        ToolOutput::Parts(parts) => parts.iter_mut().for_each(canonicalise_part),
        ToolOutput::Files(files) => files
            .iter_mut()
            .for_each(|file| canonicalise_metadata(&mut file.metadata)),
    }
}

fn canonicalise_metadata(metadata: &mut MetadataMap) {
    metadata.values_mut().for_each(canonicalise_json);
}

/// Recursively sorts JSON object keys so recordings compare identically
/// whether or not some crate in the build graph enables serde_json's
/// `preserve_order` feature (agent-client-protocol does, so workspace-wide
/// builds serialize maps in insertion order while narrower builds sort them).
fn canonicalise_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (_, nested) in &mut entries {
                canonicalise_json(nested);
            }
            map.extend(entries);
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalise_json),
        _ => {}
    }
}

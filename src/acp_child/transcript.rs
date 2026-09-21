//! Runtime-owned ephemeral disk inspection history, never reopened across runs.
//! The registry lock publishes session handles and generation gates; `start` is its
//! sole writer. Poison isolates inspection, not execution. One blocking writer
//! owns each child session, with bounded nonblocking admission. Readers open their
//! own descriptors, never lock the writer, and only see committed JSON lines.
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};
const QUEUE: usize = 256;
const FRAME_BYTES: usize = 1024 * 1024;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;
const PAGE_UPDATES: usize = 64;
const PAGE_BYTES: u64 = 256 * 1024;

#[derive(Default)]
pub(crate) struct Transcripts {
    generations: Mutex<HashMap<String, (u64, Transcript)>>,
}
impl Transcripts {
    pub(crate) fn start(&self, id: &str, generation: u64) {
        {
            let Ok(mut generations) = self.generations.lock() else {
                return;
            };
            if let Some((current, _)) = generations.get_mut(id) {
                *current = (*current).max(generation);
                return;
            }
        }
        // One disk log survives all prompt generations of this child session.
        let transcript = Transcript::start();
        let Ok(mut generations) = self.generations.lock() else {
            return;
        };
        if let Some((current, _)) = generations.get_mut(id) {
            *current = (*current).max(generation);
            return;
        }
        generations.insert(id.into(), (generation, transcript));
    }
    pub(crate) fn get(&self, id: &str, generation: u64) -> Result<Transcript, String> {
        let generations = self
            .generations
            .lock()
            .map_err(|_| "transcript registry poisoned")?;
        let (current, transcript) = generations.get(id).ok_or(
            "transcript unavailable: unknown direct child; descendant inspection is unsupported",
        )?;
        if *current != generation {
            return Err("stale transcript generation".into());
        }
        Ok(transcript.clone())
    }
}
#[derive(Clone)]
pub(crate) struct Transcript {
    sender: mpsc::SyncSender<Vec<u8>>,
    shared: Arc<Shared>,
}
#[derive(Default)]
struct Shared {
    // Owns deletion until both writer and outstanding readers finish.
    path: OnceLock<tempfile::TempPath>,
    committed: AtomicU64,
    pending: AtomicUsize,
    pending_bytes: AtomicUsize,
    failed: AtomicBool,
    // Serializes only queue admission versus sealing. No reader, IO, callback,
    // or await uses this lock; writers are record/finish. Poison fails inspection.
    finished: Mutex<bool>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        let Some(mut path) = self.path.take() else {
            return;
        };
        // Last-reader/generation teardown can run on an execution future. Never
        // perform filesystem cleanup there, even if thread creation fails. In
        // that exceptional case leave an ephemeral file for OS temp cleanup.
        path.disable_cleanup(true);
        let _ = std::thread::Builder::new()
            .name("kit-transcript-cleanup".into())
            .spawn(move || {
                let _ = std::fs::remove_file(&path);
            });
    }
}
#[derive(Debug)]
pub(crate) struct Page {
    pub updates: Vec<Value>,
    pub next_cursor: u64,
    pub caught_up: bool,
}
impl Transcript {
    fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let shared = Arc::new(Shared::default());
        let writer = Arc::clone(&shared);
        if std::thread::Builder::new()
            .name("kit-transcript".into())
            .spawn(move || {
                struct Guard(Arc<Shared>, bool);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        if !self.1 {
                            self.0.failed.store(true, Ordering::Release);
                        }
                    }
                }
                let mut guard = Guard(writer.clone(), false);
                let Ok(file) = tempfile::Builder::new()
                    .prefix("kit-transcript-")
                    .tempfile()
                else {
                    return;
                };
                let (mut file, path) = file.into_parts();
                if writer.path.set(path).is_err() {
                    return;
                }
                for bytes in receiver {
                    if bytes.is_empty() {
                        guard.1 = true;
                        return;
                    }
                    if writer.failed.load(Ordering::Acquire) {
                        return;
                    }
                    if file.write_all(&bytes).is_err() {
                        return;
                    }
                    writer
                        .committed
                        .fetch_add(bytes.len() as u64, Ordering::Release);
                    writer
                        .pending_bytes
                        .fetch_sub(bytes.len(), Ordering::Release);
                    writer.pending.fetch_sub(1, Ordering::Release);
                }
                guard.1 = true;
            })
            .is_err()
        {
            shared.failed.store(true, Ordering::Release);
        }
        Self { sender, shared }
    }
    /// Release the writer thread after all admitted records, retaining only disk
    /// history for later focus. Repeated terminal roster events are harmless.
    pub(crate) fn finish(&self) {
        let Ok(mut finished) = self.shared.finished.lock() else {
            self.fail();
            return;
        };
        if !*finished {
            *finished = true;
            if self.sender.try_send(Vec::new()).is_err() {
                self.fail();
            }
        }
    }

    /// ACP prompt acceptance has no native user-message ID, even in v2. Keep
    /// submitted input distinct from all reported updates; never guess matches.
    pub(crate) fn record_submitted_prompt(
        &self,
        owner: &str,
        generation: u64,
        content: &[super::ContentBlock],
    ) {
        self.record(&Value::Object(serde_json::Map::from_iter([
            ("sessionUpdate".into(), Value::String("kit_transcript_partial".into())),
            ("reason".into(), Value::String("Submitted prompt is shown separately. ACP does not identify its echo; reported user messages are preserved and may repeat that prompt".into())),
        ])));
        for content in content {
            let Ok(content) = serde_json::to_value(content) else {
                self.fail();
                continue;
            };
            let update = Value::Object(serde_json::Map::from_iter([
                (
                    "sessionUpdate".into(),
                    Value::String("user_message_chunk".into()),
                ),
                ("content".into(), content),
            ]));
            if let Some(update) = super::inspection_event(owner, generation, update) {
                self.record(&update);
            }
        }
    }

    /// Import the replay that ACP startup already supplies. ChildOutput is not
    /// an exact wire transcript: text is aggregated and rich updates are bounded.
    /// Keep that limitation visible instead of silently discarding available data.
    pub(crate) fn replay(&self, owner: &str, generation: u64, output: &super::ChildOutput) {
        self.record_at(&Value::Object(serde_json::Map::from_iter([
            ("sessionUpdate".into(), Value::String("kit_transcript_partial".into())),
            ("reason".into(), Value::String("Historical ACP replay is partial: assistant text is aggregated, rich updates are bounded, and original user/thought ordering is unavailable".into())),
        ])), None);
        let mut text = output.text.as_str();
        while !text.is_empty() {
            let end = text.floor_char_boundary((32 * 1024).min(text.len()));
            let (chunk, rest) = text.split_at(end);
            self.record_at(
                &Value::Object(serde_json::Map::from_iter([
                    (
                        "sessionUpdate".into(),
                        Value::String("agent_message_chunk".into()),
                    ),
                    (
                        "messageId".into(),
                        Value::String(format!("kit-replay-{generation}")),
                    ),
                    (
                        "content".into(),
                        Value::Object(serde_json::Map::from_iter([
                            ("type".into(), Value::String("text".into())),
                            ("text".into(), Value::String(chunk.into())),
                        ])),
                    ),
                ])),
                None,
            );
            text = rest;
        }
        let mut segments = super::InspectionSegments::default();
        for update in &output.updates {
            let mut update = update.clone();
            if segments.normalize(generation, &mut update).is_err() {
                self.fail();
                return;
            }
            if let Some(update) = super::inspection_event(owner, generation, update) {
                self.record_at(&update, None);
            }
        }
    }

    pub(crate) fn fail(&self) {
        self.shared.failed.store(true, Ordering::Release);
    }
    pub(crate) fn record(&self, update: &Value) {
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        self.record_at(update, observed_at);
    }

    fn record_at(&self, update: &Value, observed_at: Option<u64>) {
        if self.shared.failed.load(Ordering::Acquire) {
            return;
        }
        // Serialization itself is bounded; an enormous tool/image update does
        // not allocate an equally enormous second copy or invalidate the log.
        let mut buffer = BoundedRecord(Vec::new());
        let record = TimedRecord::new(update, observed_at);
        if serde_json::to_writer(&mut buffer, &record).is_err() {
            let notice = Value::Object(serde_json::Map::from_iter([
                ("sessionUpdate".into(), Value::String("kit_transcript_truncated".into())),
                ("reason".into(), Value::String("One inspection update exceeded 1 MiB and was omitted; later updates remain available".into())),
            ]));
            buffer.0.clear();
            if serde_json::to_writer(&mut buffer, &TimedRecord::new(&notice, record.observed_at))
                .is_err()
            {
                self.fail();
                return;
            }
        }
        buffer.0.push(b'\n');
        let bytes = buffer.0;
        let Ok(finished) = self.shared.finished.lock() else {
            self.fail();
            return;
        };
        if *finished {
            self.fail();
            return;
        }
        let size = bytes.len();
        if self.shared.pending_bytes.load(Ordering::Acquire) + size > QUEUE_BYTES {
            self.fail();
            return;
        }
        self.shared.pending_bytes.fetch_add(size, Ordering::Release);
        self.shared.pending.fetch_add(1, Ordering::Release);
        if self.sender.try_send(bytes).is_err() {
            self.shared.pending_bytes.fetch_sub(size, Ordering::Release);
            self.fail();
            self.shared.pending.fetch_sub(1, Ordering::Release);
        }
    }
    pub(crate) async fn read(&self, cursor: u64) -> Result<Page, String> {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.read(cursor))
            .await
            .map_err(|error| format!("transcript reader failed: {error}"))?
    }
}
/// Additive ephemeral record metadata. Every writer emits an unsigned Unix
/// millisecond timestamp or explicit null. Files are never reopened across runs,
/// so there is no persistent migration. Missing legacy metadata means unknown.
/// ACP message updates have no native event timestamp; preserve supplied Kit
/// timing when available rather than replacing it with a replay observation.
/// A supplied value uses the source's wall clock; otherwise this is Kit's local
/// receipt time. Neither is an exact remote execution start. Clock skew or clock
/// adjustments can therefore make a boundary pair unusable, which the UI treats
/// as unknown rather than synthesizing a duration.
struct TimedRecord<'a> {
    update: &'a Value,
    observed_at: Option<u64>,
}
impl<'a> TimedRecord<'a> {
    fn new(update: &'a Value, observed_at: Option<u64>) -> Self {
        Self {
            update,
            observed_at: match update.get("kitObservedAtUnixMs") {
                Some(value) => value.as_u64(),
                None => observed_at,
            },
        }
    }
}
impl Serialize for TimedRecord<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let object = self
            .update
            .as_object()
            .ok_or_else(|| serde::ser::Error::custom("inspection update must be an object"))?;
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in object {
            if key != "kitObservedAtUnixMs" {
                map.serialize_entry(key, value)?;
            }
        }
        map.serialize_entry("kitObservedAtUnixMs", &self.observed_at)?;
        map.end()
    }
}

/// A production serialization boundary: memory cannot grow with arbitrary
/// incoming content. Larger records become an explicit, nonfatal notice.
struct BoundedRecord(Vec<u8>);
impl Write for BoundedRecord {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > FRAME_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "inspection update exceeds record limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn read(&self, cursor: u64) -> Result<Page, String> {
        let unavailable =
            || "transcript spool unavailable (IO, queue overflow, or writer failure)".to_string();
        if self.failed.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let end = self.committed.load(Ordering::Acquire);
        if cursor > end {
            return Err("invalid transcript cursor".into());
        }
        let Some(path) = self.path.get() else {
            return Ok(Page {
                updates: vec![],
                next_cursor: cursor,
                caught_up: false,
            });
        };
        let mut file = File::open(path).map_err(|_| unavailable())?;
        if cursor != 0 {
            file.seek(SeekFrom::Start(cursor - 1))
                .map_err(|_| unavailable())?;
            let mut byte = [0];
            file.read_exact(&mut byte).map_err(|_| unavailable())?;
            if byte[0] != b'\n' {
                return Err("invalid transcript cursor boundary".into());
            }
        }
        file.seek(SeekFrom::Start(cursor))
            .map_err(|_| unavailable())?;
        let mut reader = BufReader::new(file.take(end - cursor));
        let mut updates = Vec::new();
        let mut next_cursor = cursor;
        for _ in 0..PAGE_UPDATES {
            let mut line = Vec::new();
            let size = reader
                .by_ref()
                .take((FRAME_BYTES + 2) as u64)
                .read_until(b'\n', &mut line)
                .map_err(|_| unavailable())?;
            if size == 0 {
                break;
            }
            if line.last() != Some(&b'\n') || size > FRAME_BYTES + 1 {
                return Err(unavailable());
            }
            if !updates.is_empty() && next_cursor - cursor + size as u64 > PAGE_BYTES {
                break;
            }
            updates.push(serde_json::from_slice(&line).map_err(|_| unavailable())?);
            next_cursor += size as u64;
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let caught_up = next_cursor == self.committed.load(Ordering::Acquire)
            && self.pending.load(Ordering::Acquire) == 0;
        Ok(Page {
            updates,
            next_cursor,
            caught_up,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(n: usize) -> Value {
        json!({"n": n, "kitObservedAtUnixMs": 123})
    }
    async fn drain(transcript: &Transcript, mut cursor: u64) -> Page {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut updates = vec![];
            loop {
                let page = transcript.read(cursor).await.unwrap();
                assert!(page.updates.len() <= PAGE_UPDATES);
                assert!(page.next_cursor - cursor <= (FRAME_BYTES + 1) as u64);
                cursor = page.next_cursor;
                updates.extend(page.updates);
                if page.caught_up {
                    return Page {
                        updates,
                        next_cursor: cursor,
                        caught_up: true,
                    };
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[test]
    fn timing_schema_preserves_source_and_normalizes_unknown() {
        let legacy = json!({"sessionUpdate": "agent_message_chunk"});
        assert_eq!(
            serde_json::to_value(TimedRecord::new(&legacy, None)).unwrap(),
            json!({"sessionUpdate": "agent_message_chunk", "kitObservedAtUnixMs": null})
        );
        assert_eq!(
            serde_json::to_value(TimedRecord::new(&legacy, Some(1_700_000_000_123))).unwrap()["kitObservedAtUnixMs"],
            1_700_000_000_123_u64
        );
        for source in [json!(123), json!(null), json!("bad"), json!(-1), json!(1.5)] {
            let current = json!({"kitObservedAtUnixMs": source});
            let encoded = serde_json::to_value(TimedRecord::new(&current, Some(999))).unwrap();
            assert_eq!(
                encoded["kitObservedAtUnixMs"],
                source.as_u64().map(Value::from).unwrap_or(Value::Null)
            );
        }
    }

    #[tokio::test]
    async fn original_observation_survives_disk_pages_and_truncation() {
        let transcript = Transcript::start();
        transcript.record_at(
            &json!({"sessionUpdate":"agent_message_chunk"}),
            Some(1_700_000_000_123),
        );
        transcript.record_at(
            &json!({"text":"x".repeat(FRAME_BYTES + 1)}),
            Some(1_700_000_000_456),
        );
        let first = drain(&transcript, 0).await;
        assert_eq!(
            first.updates[0]["kitObservedAtUnixMs"],
            1_700_000_000_123_u64
        );
        assert_eq!(
            first.updates[1]["kitObservedAtUnixMs"],
            1_700_000_000_456_u64
        );
        assert_eq!(
            first.updates[1]["sessionUpdate"],
            "kit_transcript_truncated"
        );
        transcript.record_at(
            &json!({"sessionUpdate":"agent_message_chunk"}),
            Some(1_700_000_000_789),
        );
        let tail = drain(&transcript, first.next_cursor).await;
        assert_eq!(
            tail.updates[0]["kitObservedAtUnixMs"],
            1_700_000_000_789_u64
        );
        let repeated = drain(&transcript, 0).await;
        assert_eq!(repeated.updates[..2], first.updates);
    }

    #[tokio::test]
    async fn historical_import_preserves_source_time_and_marks_missing_unknown() {
        let transcript = Transcript::start();
        transcript.replay("child", 1, &super::super::ChildOutput {
            text: "historical text".into(),
            updates: vec![
                json!({"sessionUpdate":"agent_thought_chunk", "content":{"type":"text","text":"unknown"}}),
                json!({"sessionUpdate":"agent_thought_chunk", "content":{"type":"text","text":"known"}, "kitObservedAtUnixMs":123}),
            ],
            ..Default::default()
        });
        let history = drain(&transcript, 0).await;
        assert_eq!(history.updates.len(), 4);
        for update in &history.updates[..3] {
            assert_eq!(update.get("kitObservedAtUnixMs"), Some(&Value::Null));
        }
        assert_eq!(history.updates[3]["kitObservedAtUnixMs"], 123);
    }

    #[tokio::test]
    async fn disk_replay_then_live_handoff_is_ordered_without_duplicates() {
        let transcript = Transcript::start();
        for n in 0..150 {
            transcript.record(&update(n));
        }
        let snapshot = drain(&transcript, 0).await;
        assert_eq!(snapshot.updates, (0..150).map(update).collect::<Vec<_>>());
        assert!(
            transcript
                .read(snapshot.next_cursor)
                .await
                .unwrap()
                .updates
                .is_empty()
        );
        for n in 150..200 {
            transcript.record(&update(n));
        }
        let live = drain(&transcript, snapshot.next_cursor).await;
        assert_eq!(live.updates, (150..200).map(update).collect::<Vec<_>>());
        assert_eq!(drain(&transcript, 0).await.updates.len(), 200);
        // A real file backs replay, not an in-memory transcript vector.
        let path = transcript.shared.path.get().unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), live.next_cursor);
        assert!(transcript.read(1).await.unwrap_err().contains("boundary"));
        assert!(
            transcript
                .read(live.next_cursor + 1)
                .await
                .unwrap_err()
                .contains("cursor")
        );
    }

    #[tokio::test]
    async fn generation_and_root_isolation_and_sticky_failure() {
        let first = Transcripts::default();
        let second = Transcripts::default();
        first.start("child", 1);
        let old = first.get("child", 1).unwrap();
        old.record(&update(1));
        drain(&old, 0).await;
        assert!(second.get("child", 1).is_err());
        assert!(first.get("descendant", 1).is_err());
        first.start("child", 2);
        first.start("child", 1); // delayed lifecycle event cannot restore generation 1
        assert!(first.get("child", 1).is_err());
        let current = first.get("child", 2).unwrap();
        assert_eq!(drain(&current, 0).await.updates, vec![update(1)]);
        current.record(&update(2));
        assert_eq!(drain(&current, 0).await.updates, vec![update(1), update(2)]);
        current.fail();
        assert!(current.read(0).await.is_err());
        current.record(&update(3));
        assert!(current.read(0).await.is_err());
    }

    #[tokio::test]
    async fn large_updates_replay_and_oversized_notice_does_not_freeze_history() {
        let transcript = Transcript::start();
        let medium = json!({"text": "m".repeat(16 * 1024), "kitObservedAtUnixMs": 123});
        let large = json!({"text": "l".repeat(512 * 1024), "kitObservedAtUnixMs": 123});
        transcript.record(&medium);
        transcript.record(&large);
        transcript.record(&json!({"text": "x".repeat(FRAME_BYTES + 1)}));
        transcript.record(&update(4));
        let history = drain(&transcript, 0).await;
        assert_eq!(history.updates.len(), 4);
        assert_eq!(history.updates[0], medium);
        assert_eq!(history.updates[1], large);
        assert_eq!(
            history.updates[2]["sessionUpdate"],
            "kit_transcript_truncated"
        );
        assert_eq!(history.updates[3], update(4));
    }

    #[tokio::test]
    async fn supplied_replay_imports_text_and_rich_updates_with_partial_notice() {
        let transcript = Transcript::start();
        let output = super::super::ChildOutput {
            text: "historic reply".into(),
            updates: vec![
                json!({"sessionUpdate":"tool_call", "toolCallId":"old-tool", "title":"Read"}),
            ],
            updates_truncated: true,
            ..Default::default()
        };
        transcript.replay("child", 1, &output);
        transcript.record(&update(1));
        let history = drain(&transcript, 0).await;
        assert_eq!(
            history.updates[0]["sessionUpdate"],
            "kit_transcript_partial"
        );
        assert_eq!(history.updates[1]["content"]["text"], "historic reply");
        assert_eq!(history.updates[2]["sessionUpdate"], "tool_call_update");
        assert_eq!(history.updates[3], update(1));
    }

    fn route(transcript: Transcript) -> super::super::Route {
        super::super::Route {
            transcript: Some(transcript),
            inspection_segments: Arc::default(),
            owner: "child".into(),
            generation: 1,
            output: Arc::default(),
            idle: tokio::sync::watch::channel(super::super::protocol::Foreground::Waiting).0,
        }
    }

    #[tokio::test]
    async fn fragmented_echo_and_identical_steer_are_preserved_with_uncertainty() {
        let transcript = Transcript::start();
        transcript.record_submitted_prompt(
            "child",
            1,
            &[super::super::ContentBlock::Text(
                agentkit_acp::TextContent::new("hello"),
            )],
        );
        let route = route(transcript.clone());
        for text in ["hel", "lo"] {
            route.record_inspection(json!({"sessionUpdate":"user_message_chunk", "content":{"type":"text","text":text}}));
        }
        route.record_inspection(json!({"sessionUpdate":"user_message", "messageId":"steer-accepted", "content":[{"type":"text","text":"hello"}]}));
        let history = drain(&transcript, 0).await;
        assert_eq!(history.updates.len(), 5);
        assert_eq!(
            history.updates[0]["sessionUpdate"],
            "kit_transcript_partial"
        );
        assert!(
            history.updates[0]["reason"]
                .as_str()
                .unwrap()
                .contains("echo")
        );
        assert_eq!(history.updates[1]["content"]["text"], "hello");
        assert_eq!(history.updates[2]["content"]["text"], "hel");
        assert_eq!(history.updates[3]["content"]["text"], "lo");
        assert_eq!(
            history.updates[2]["messageId"],
            history.updates[3]["messageId"]
        );
        assert_ne!(
            history.updates[1]["messageId"],
            history.updates[2]["messageId"]
        );
        assert_eq!(history.updates[4]["messageId"], "steer-accepted");
        assert_eq!(history.updates[4]["content"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn poisoned_segments_and_admission_fail_only_inspection() {
        let transcript = Transcript::start();
        let route = route(transcript.clone());
        let _ = std::panic::catch_unwind(|| {
            let _guard = route.inspection_segments.lock().unwrap();
            panic!("segment writer unwind");
        });
        route.record_inspection(json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"reply"}}));
        assert!(transcript.read(0).await.is_err());
        // The execution accumulator and completion signal remain usable.
        route
            .output
            .lock()
            .unwrap()
            .text
            .push_str("execution continues");
        assert_eq!(route.output.lock().unwrap().text, "execution continues");
        let mut completion = route.idle.subscribe();
        route
            .idle
            .send_replace(super::super::protocol::Foreground::Idle(None));
        completion.changed().await.unwrap();

        let other = Transcript::start();
        let _ = std::panic::catch_unwind(|| {
            let _guard = other.shared.finished.lock().unwrap();
            panic!("admission writer unwind");
        });
        other.record(&update(0));
        other.finish();
        assert!(other.read(0).await.is_err());
        assert!(other.shared.finished.is_poisoned());
    }

    #[tokio::test]
    async fn deleted_spool_is_an_explicit_read_error() {
        let transcript = Transcript::start();
        transcript.record(&update(0));
        drain(&transcript, 0).await;
        std::fs::remove_file(transcript.shared.path.get().unwrap()).unwrap();
        assert!(transcript.read(0).await.is_err());
    }

    #[tokio::test]
    async fn finished_history_survives_writer_exit_and_reader_cancellation() {
        let transcript = Transcript::start();
        for n in 0..10 {
            transcript.record(&update(n));
        }
        transcript.finish();
        transcript.finish();
        let reader = transcript.clone();
        let cancelled = tokio::spawn(async move { reader.read(0).await });
        cancelled.abort();
        let _ = cancelled.await;
        assert_eq!(drain(&transcript, 0).await.updates.len(), 10);
        transcript.record(&update(11));
        assert!(transcript.read(0).await.is_err());
    }

    #[tokio::test]
    async fn full_or_disconnected_admission_fails_closed() {
        for disconnected in [false, true] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let transcript = Transcript {
                sender,
                shared: Arc::default(),
            };
            if disconnected {
                drop(receiver);
            } else {
                transcript.record(&update(0));
                transcript.record(&update(1));
                drop(receiver);
            }
            transcript.record(&update(2));
            assert!(transcript.read(0).await.is_err());
        }
    }

    #[test]
    fn poisoned_registry_is_isolated() {
        let transcripts = Transcripts::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = transcripts.generations.lock().unwrap();
            panic!("writer unwind");
        });
        transcripts.start("child", 1);
        assert!(transcripts.get("child", 1).is_err());
    }
}

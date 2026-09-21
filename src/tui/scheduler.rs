//! Bounded queue turns and event-independent frame pacing for the active session.

use super::{
    ActiveSessionRoute, App, BackgroundCompletion, BackgroundWorkers, ClipboardPastes, NativeVoice,
    QueuedUpdate, Update, accept_queued_update, finish_clipboard_paste, refresh_config_state,
};
use std::sync::{Arc, Mutex};
use tokio::{sync::mpsc, time::Instant};

const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
// Yield to the round-robin event selector even when producers keep queues hot.
const UPDATE_BURST: usize = 64;

pub(super) struct Frames {
    dirty: bool,
    next: Instant,
}

impl Frames {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            dirty: true,
            next: now,
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.dirty.then_some(self.next)
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        self.dirty && now >= self.next
    }

    pub(super) fn drawn(&mut self, now: Instant) {
        self.dirty = false;
        // Do not catch up missed frames after slow draws or blocking actions.
        self.next = now + FRAME_INTERVAL;
    }
}

/// Keep a full worker queue's head ahead of all later updates. No update bypasses
/// image materialization, including non-image updates following an image.
pub(super) fn forward_updates(
    workers: &BackgroundWorkers,
    updates: &mut mpsc::UnboundedReceiver<QueuedUpdate>,
    first: QueuedUpdate,
) -> Result<Option<QueuedUpdate>, ()> {
    for queued in std::iter::once(first)
        .chain(std::iter::from_fn(|| updates.try_recv().ok()))
        .take(UPDATE_BURST)
    {
        match workers.try_update(queued) {
            Ok(()) => {}
            Err(error) => match *error {
                std::sync::mpsc::TrySendError::Full(queued) => return Ok(Some(queued)),
                std::sync::mpsc::TrySendError::Disconnected(_) => return Err(()),
            },
        }
    }
    Ok(None)
}

pub(super) struct Applied {
    pub(super) dirty: bool,
    pub(super) submit_after_paste: bool,
}

/// Apply completions in FIFO order and coalesce their redraw requests. Keep
/// voice/config observation on every accepted update, never just the last one.
pub(super) fn apply_completions(
    app: &mut App,
    route: &Arc<Mutex<ActiveSessionRoute>>,
    voice: &mut NativeVoice,
    pastes: &mut ClipboardPastes,
    completed: &mut mpsc::Receiver<BackgroundCompletion>,
    first: BackgroundCompletion,
) -> Applied {
    let mut applied = Applied {
        dirty: false,
        submit_after_paste: false,
    };
    for completion in std::iter::once(first)
        .chain(std::iter::from_fn(|| completed.try_recv().ok()))
        .take(UPDATE_BURST)
    {
        match completion {
            BackgroundCompletion::Update { queued, images } => {
                if let Some(update) = accept_queued_update(route, queued) {
                    if let Update::ConfigOptions(options) = &update {
                        refresh_config_state(app, Some(options));
                    }
                    voice.observe(&update, app);
                    app.apply_materialized(update, images);
                    applied.dirty = true;
                }
            }
            BackgroundCompletion::Clipboard {
                generation,
                route: clipboard_route,
                result,
            } => {
                applied.submit_after_paste =
                    finish_clipboard_paste(app, route, pastes, generation, clipboard_route, result);
                applied.dirty = true;
                // The synthetic Enter must precede the next completion, just as
                // it did when the active loop consumed one completion per turn.
                if applied.submit_after_paste {
                    break;
                }
            }
        }
    }
    applied
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use crate::tui::{ClipboardResult, VoiceHandoff, app::Block};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn dirty_frames_coalesce_without_postponing_the_deadline() {
        let now = Instant::now();
        let mut frames = Frames::new(now);
        assert!(frames.ready(now));
        frames.drawn(now);
        assert_eq!(frames.deadline(), None);
        assert!(!frames.ready(now + FRAME_INTERVAL));
        frames.invalidate();
        let deadline = frames.deadline().unwrap();
        frames.invalidate();
        assert_eq!(frames.deadline(), Some(deadline));
        assert!(!frames.ready(now));
        assert!(frames.ready(deadline));
        // A late frame schedules from its completion, not from the old deadline.
        let late = deadline + FRAME_INTERVAL;
        frames.drawn(late);
        frames.invalidate();
        assert!(!frames.ready(late));
        assert!(frames.ready(late + FRAME_INTERVAL));
    }

    #[test]
    fn forwarding_preserves_order_across_worker_backpressure() {
        let (sender, worker) = std::sync::mpsc::sync_channel(1);
        let workers = BackgroundWorkers {
            updates: Some(sender),
            clipboard: None,
            stopping: Arc::new(AtomicBool::new(false)),
            threads: vec![],
        };
        let (sender, mut updates) = mpsc::unbounded_channel();
        for text in ["one", "two", "three"] {
            sender
                .send(QueuedUpdate::for_session(7, Update::Log(text.into())))
                .unwrap();
        }
        let first = updates.try_recv().unwrap();
        let mut pending = forward_updates(&workers, &mut updates, first).unwrap();
        let mut received = vec![];
        loop {
            let queued = worker.try_recv().unwrap();
            assert_eq!(queued.generation, Some(7));
            let Update::Log(text) = queued.update else {
                panic!("expected log")
            };
            received.push(text);
            let Some(first) = pending else { break };
            pending = forward_updates(&workers, &mut updates, first).unwrap();
        }
        assert_eq!(received, ["one", "two", "three"]);
        assert!(updates.try_recv().is_err());
        drop(worker);
        assert!(
            forward_updates(
                &workers,
                &mut updates,
                QueuedUpdate::global(Update::Log("closed".into()))
            )
            .is_err()
        );
    }

    #[test]
    fn forwarding_yields_with_capacity_and_never_skips_the_next_head() {
        let total = UPDATE_BURST * 2;
        let (sender, worker) = std::sync::mpsc::sync_channel(total);
        let workers = BackgroundWorkers {
            updates: Some(sender),
            clipboard: None,
            stopping: Arc::new(AtomicBool::new(false)),
            threads: vec![],
        };
        let (sender, mut updates) = mpsc::unbounded_channel();
        for index in 0..total {
            sender
                .send(QueuedUpdate::global(Update::Log(index.to_string())))
                .unwrap();
        }
        let first = updates.try_recv().unwrap();
        assert!(
            forward_updates(&workers, &mut updates, first)
                .unwrap()
                .is_none()
        );
        let next = updates
            .try_recv()
            .expect("a bounded turn leaves queued work");
        assert!(
            forward_updates(&workers, &mut updates, next)
                .unwrap()
                .is_none()
        );
        let received: Vec<_> = worker
            .try_iter()
            .map(|queued| {
                let Update::Log(text) = queued.update else {
                    panic!("expected log")
                };
                text
            })
            .collect();
        assert_eq!(
            received,
            (0..total)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
        );
        assert!(updates.try_recv().is_err());
    }

    #[test]
    fn stale_clipboard_completion_cannot_finish_current_paste() {
        let mut app = app();
        let clipboard_route = app.clipboard_route();
        let mut pastes = ClipboardPastes::default();
        pastes.queued(7, clipboard_route.clone());
        let (sender, mut completed) = mpsc::channel(2);
        sender
            .try_send(BackgroundCompletion::Clipboard {
                generation: 7,
                route: clipboard_route.clone(),
                result: ClipboardResult::Text("current".into()),
            })
            .ok()
            .unwrap();
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut pastes,
            &mut completed,
            BackgroundCompletion::Clipboard {
                generation: 6,
                route: clipboard_route,
                result: ClipboardResult::Text("stale".into()),
            },
        );
        assert!(applied.dirty);
        assert!(!applied.submit_after_paste);
        assert_eq!(app.editor.text(), "current");
        assert!(pastes.pending.is_none());
    }

    fn app() -> App {
        App::new(
            "/tmp".into(),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        )
    }

    fn route() -> Arc<Mutex<ActiveSessionRoute>> {
        Arc::new(Mutex::new(ActiveSessionRoute {
            id: "session".into(),
            generation: 7,
        }))
    }

    fn completion(generation: u64, update: Update) -> BackgroundCompletion {
        BackgroundCompletion::Update {
            queued: QueuedUpdate::for_session(generation, update),
            images: vec![],
        }
    }

    #[test]
    fn completions_apply_ordered_replacements_and_observe_every_voice_update() {
        let mut app = app();
        let mut voice = NativeVoice::default();
        voice.handoff = Some(VoiceHandoff {
            id: "task".into(),
            accepted: false,
            user_message_id: None,
            text: String::new(),
            message_id: String::new(),
        });
        let (sender, mut completed) = mpsc::channel(16);
        let updates = [
            Update::VoicePromptAccepted {
                id: "task".into(),
                result: Ok(()),
            },
            Update::UserMessage {
                id: "user".into(),
                text: "question".into(),
                images: vec![],
                append: false,
            },
            Update::AgentMessage {
                id: "answer".into(),
                text: "old".into(),
                append: false,
            },
            Update::AgentMessage {
                id: "answer".into(),
                text: "new".into(),
                append: false,
            },
            Update::AgentMessage {
                id: "answer".into(),
                text: " answer".into(),
                append: true,
            },
        ];
        for update in updates {
            sender.try_send(completion(7, update)).ok().unwrap();
        }
        // Stale data must not append to either the app or voice's accepted turn.
        sender
            .try_send(completion(
                6,
                Update::AgentMessage {
                    id: "answer".into(),
                    text: " stale".into(),
                    append: true,
                },
            ))
            .ok()
            .unwrap();
        let first = completed.try_recv().unwrap();
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut voice,
            &mut ClipboardPastes::default(),
            &mut completed,
            first,
        );
        assert!(applied.dirty);
        assert!(!applied.submit_after_paste);
        assert!(completed.try_recv().is_err());
        let handoff = voice.handoff.as_ref().unwrap();
        assert!(handoff.accepted);
        assert_eq!(handoff.user_message_id.as_deref(), Some("user"));
        assert_eq!(handoff.text, "new answer");
        assert!(matches!(app.blocks.last(), Some(Block::Agent(text)) if text == "new answer"));
    }

    #[test]
    fn stale_completions_do_not_request_a_frame() {
        let mut app = app();
        let (_sender, mut completed) = mpsc::channel(1);
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut ClipboardPastes::default(),
            &mut completed,
            completion(6, Update::Log("stale".into())),
        );
        assert!(!applied.dirty);
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn clipboard_submission_is_a_batch_boundary() {
        let mut app = app();
        let clipboard_route = app.clipboard_route();
        let mut pastes = ClipboardPastes::default();
        pastes.queued(7, clipboard_route.clone());
        pastes.pending.as_mut().unwrap().submit = true;
        let (sender, mut completed) = mpsc::channel(2);
        sender
            .try_send(completion(7, Update::Log("after paste".into())))
            .ok()
            .unwrap();
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut pastes,
            &mut completed,
            BackgroundCompletion::Clipboard {
                generation: 7,
                route: clipboard_route,
                result: ClipboardResult::Text("pasted".into()),
            },
        );
        assert!(applied.dirty);
        assert!(applied.submit_after_paste);
        assert_eq!(app.editor.text(), "pasted");
        assert!(completed.try_recv().is_ok());
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn completion_turn_leaves_backlog_for_other_event_sources() {
        let mut app = app();
        let (sender, mut completed) = mpsc::channel(UPDATE_BURST * 2);
        for _ in 0..UPDATE_BURST * 2 {
            sender
                .try_send(completion(7, Update::Log("queued".into())))
                .ok()
                .unwrap();
        }
        let first = completed.try_recv().unwrap();
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut ClipboardPastes::default(),
            &mut completed,
            first,
        );
        assert!(applied.dirty);
        assert!(!app.logs.is_empty());
        assert!(app.logs.iter().all(|log| log == "queued"));
        assert!(completed.try_recv().is_ok());
    }
}

//! Bounded queue turns and event-independent frame pacing for the active session.

use super::{
    ActiveSessionRoute, App, BackgroundCompletion, BackgroundWorkers, ClipboardPastes, NativeVoice,
    QueuedUpdate, Update, accept_queued_update, finish_clipboard_paste, refresh_config_state,
};
use std::sync::{Arc, Mutex};
use tokio::{sync::mpsc, time::Instant};

const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
// Yield to input and safety checks even when producers keep queues hot.
const UPDATE_BURST: usize = 64;
const UPDATE_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);

/// Checked between records, before consuming the next queue entry. A single
/// synchronous record can exceed this budget; it is not a latency guarantee.
struct BatchBudget {
    started: Instant,
    records: usize,
}

impl BatchBudget {
    fn new(started: Instant) -> Self {
        Self {
            started,
            records: 0,
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        if self.records >= UPDATE_BURST
            || (self.records > 0 && now.duration_since(self.started) >= UPDATE_BUDGET)
        {
            return false;
        }
        self.records += 1;
        true
    }
}

pub(super) struct Frames {
    dirty: bool,
    urgent: bool,
    next: Instant,
}

impl Frames {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            dirty: true,
            urgent: false,
            next: now,
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn invalidate_input(&mut self) {
        self.dirty = true;
        self.urgent = true;
    }

    pub(super) fn urgent(&self) -> bool {
        self.urgent
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.dirty.then_some(self.next)
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        self.dirty && (self.urgent || now >= self.next)
    }

    pub(super) fn drawn(&mut self, now: Instant) {
        self.dirty = false;
        self.urgent = false;
        // Pace from the start of the draw, without catching up missed frames.
        self.next = now + FRAME_INTERVAL;
    }
}

/// The priority tier polls real event sources with the caller's task waker.
/// A pending result permits the caller to poll the background round-robin tier.
pub(super) enum PriorityEvent {
    Shutdown,
    Stop,
    Frame,
    Terminal(Option<std::io::Result<crossterm::event::Event>>),
}

pub(super) fn poll_priority(
    cx: &mut std::task::Context<'_>,
    mut shutdown: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
    mut stopped: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
    events: &mut (impl futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin),
    frames: &Frames,
    submit_after_paste: &mut bool,
) -> std::task::Poll<PriorityEvent> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use futures_util::StreamExt;
    use std::task::Poll;

    if shutdown.as_mut().poll(cx).is_ready() {
        return Poll::Ready(PriorityEvent::Shutdown);
    }
    if stopped.as_mut().poll(cx).is_ready() {
        return Poll::Ready(PriorityEvent::Stop);
    }
    if frames.urgent() {
        return Poll::Ready(PriorityEvent::Frame);
    }
    if std::mem::take(submit_after_paste) {
        return Poll::Ready(PriorityEvent::Terminal(Some(Ok(Event::Key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )))));
    }
    events.poll_next_unpin(cx).map(PriorityEvent::Terminal)
}

/// Keep a full worker queue's head ahead of all later updates. No update bypasses
/// image materialization, including non-image updates following an image.
pub(super) fn forward_updates(
    workers: &BackgroundWorkers,
    updates: &mut mpsc::UnboundedReceiver<QueuedUpdate>,
    first: QueuedUpdate,
) -> Result<Option<QueuedUpdate>, ()> {
    let mut budget = BatchBudget::new(Instant::now());
    let mut first = Some(first);
    while budget.take(Instant::now()) {
        let Some(queued) = first.take().or_else(|| updates.try_recv().ok()) else {
            break;
        };
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
    pub(super) urgent: bool,
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
        urgent: false,
        submit_after_paste: false,
    };
    let mut budget = BatchBudget::new(Instant::now());
    let mut first = Some(first);
    while budget.take(Instant::now()) {
        let Some(completion) = first.take().or_else(|| completed.try_recv().ok()) else {
            break;
        };
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
                let outcome =
                    finish_clipboard_paste(app, route, pastes, generation, clipboard_route, result);
                if outcome.accepted {
                    applied.submit_after_paste = outcome.submit;
                    applied.dirty = true;
                    applied.urgent = true;
                    // Present paste feedback before consuming more streaming work,
                    // even without deferred Enter. A synthetic submit still precedes
                    // the next completion after this urgent frame.
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
        // A late frame schedules from its start, not from the old deadline.
        let late = deadline + FRAME_INTERVAL;
        frames.drawn(late);
        frames.invalidate();
        assert!(!frames.ready(late));
        assert!(frames.ready(late + FRAME_INTERVAL));
    }

    #[test]
    fn input_frame_bypasses_pacing_and_consumes_pending_stream_invalidation() {
        let now = Instant::now();
        let mut frames = Frames::new(now);
        frames.drawn(now);
        frames.invalidate();
        let input_at = now + std::time::Duration::from_millis(1);
        assert!(!frames.ready(input_at));
        frames.invalidate_input();
        assert!(frames.urgent());
        assert!(frames.ready(input_at));
        frames.drawn(input_at);
        assert!(!frames.urgent());
        assert_eq!(frames.deadline(), None);
        assert!(!frames.ready(now + FRAME_INTERVAL));
        frames.invalidate();
        assert_eq!(frames.deadline(), Some(input_at + FRAME_INTERVAL));
        assert!(!frames.ready(input_at));
    }

    #[test]
    fn batch_budget_checks_elapsed_time_and_count_between_records() {
        let now = Instant::now();
        let mut budget = BatchBudget::new(now);
        assert!(budget.take(now));
        assert!(budget.take(now + UPDATE_BUDGET / 2));
        assert!(!budget.take(now + UPDATE_BUDGET));
        assert!(!budget.take(now + UPDATE_BUDGET * 2));
        let mut budget = BatchBudget::new(now);
        // The cap is an explicit policy, not an observed production work count.
        for _ in 0..UPDATE_BURST {
            assert!(budget.take(now));
        }
        assert!(!budget.take(now));
    }

    #[tokio::test]
    async fn priority_tier_preserves_safety_input_and_synthetic_submit_order() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use std::{
            future::{pending, poll_fn, ready},
            pin::pin,
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let key = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        sender.send(Ok(key.clone())).unwrap();
        let mut events = futures_util::stream::poll_fn(|cx| receiver.poll_recv(cx));
        let mut frames = Frames::new(Instant::now());
        frames.invalidate_input();
        let mut submit = true;
        let mut shutdown = pin!(ready(()));
        let mut stopped = pin!(ready(()));
        assert!(matches!(
            poll_fn(|cx| poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit
            ))
            .await,
            PriorityEvent::Shutdown
        ));
        let mut shutdown = pin!(pending());
        assert!(matches!(
            poll_fn(|cx| poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit
            ))
            .await,
            PriorityEvent::Stop
        ));
        let mut stopped = pin!(pending());
        assert!(matches!(
            poll_fn(|cx| poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit
            ))
            .await,
            PriorityEvent::Frame
        ));
        assert!(submit);
        frames.drawn(Instant::now() - FRAME_INTERVAL);
        frames.invalidate(); // Even a due stream frame must wait for input.
        assert!(frames.ready(Instant::now()));
        assert!(matches!(
            poll_fn(|cx| poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit
            ))
            .await,
            PriorityEvent::Terminal(Some(Ok(Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }))))
        ));
        assert!(!submit);
        let event = poll_fn(|cx| {
            poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit,
            )
        })
        .await;
        assert!(matches!(event, PriorityEvent::Terminal(Some(Ok(event))) if event == key));
        // With no input, the production selector may now service background work.
        poll_fn(|cx| {
            assert!(
                poll_priority(
                    cx,
                    shutdown.as_mut(),
                    stopped.as_mut(),
                    &mut events,
                    &frames,
                    &mut submit
                )
                .is_pending()
            );
            std::task::Poll::Ready(())
        })
        .await;
        // The same source must also wake a pending selector, not just win when
        // already queued. No synthetic waker or polling-count assertion.
        let receive = poll_fn(|cx| {
            poll_priority(
                cx,
                shutdown.as_mut(),
                stopped.as_mut(),
                &mut events,
                &frames,
                &mut submit,
            )
        });
        let send = async {
            tokio::task::yield_now().await;
            sender.send(Ok(Event::Paste("new paste".into()))).unwrap();
        };
        let (event, ()) = tokio::join!(receive, send);
        assert!(
            matches!(event, PriorityEvent::Terminal(Some(Ok(Event::Paste(text)))) if text == "new paste")
        );
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
            let Some(first) = pending.or_else(|| updates.try_recv().ok()) else {
                break;
            };
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
        let mut next = Some(next);
        while let Some(first) = next {
            assert!(
                forward_updates(&workers, &mut updates, first)
                    .unwrap()
                    .is_none()
            );
            next = updates.try_recv().ok();
        }
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
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut pastes,
            &mut completed,
            BackgroundCompletion::Clipboard {
                generation: 6,
                route: clipboard_route.clone(),
                result: ClipboardResult::Text("stale".into()),
            },
        );
        assert!(!applied.dirty);
        assert!(!applied.submit_after_paste);
        assert!(!applied.urgent);
        assert_eq!(app.editor.text(), "");
        assert!(pastes.pending.is_some());
        // Stale results cannot finish the current paste or request a frame.
        sender
            .try_send(BackgroundCompletion::Clipboard {
                generation: 7,
                route: clipboard_route.clone(),
                result: ClipboardResult::Text("current".into()),
            })
            .ok()
            .unwrap();
        let first = completed.try_recv().unwrap();
        let applied = apply_completions(
            &mut app,
            &route(),
            &mut NativeVoice::default(),
            &mut pastes,
            &mut completed,
            first,
        );
        assert!(applied.urgent);
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
        let route = route();
        let mut pastes = ClipboardPastes::default();
        let mut dirty = false;
        while let Ok(first) = completed.try_recv() {
            let applied = apply_completions(
                &mut app,
                &route,
                &mut voice,
                &mut pastes,
                &mut completed,
                first,
            );
            dirty |= applied.dirty;
            assert!(!applied.urgent);
            assert!(!applied.submit_after_paste);
        }
        assert!(dirty);
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
    fn clipboard_without_submit_requests_urgent_frame_and_leaves_stream_queued() {
        let mut app = app();
        let clipboard_route = app.clipboard_route();
        let mut pastes = ClipboardPastes::default();
        pastes.queued(7, clipboard_route.clone());
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
        assert!(applied.urgent);
        assert!(!applied.submit_after_paste);
        assert_eq!(app.editor.text(), "pasted");
        assert!(pastes.pending.is_none());
        assert!(app.logs.is_empty());
        let BackgroundCompletion::Update { queued, .. } = completed.try_recv().unwrap() else {
            panic!("expected queued streaming update");
        };
        assert!(matches!(queued.update, Update::Log(text) if text == "after paste"));
    }

    #[test]
    fn current_clipboard_error_is_urgent_but_cannot_submit() {
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
                result: ClipboardResult::Error("clipboard unavailable".into()),
            },
        );
        assert!(applied.urgent);
        assert!(applied.dirty);
        assert!(!applied.submit_after_paste);
        assert!(pastes.pending.is_none());
        assert!(completed.try_recv().is_ok());
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
        assert!(applied.urgent);
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

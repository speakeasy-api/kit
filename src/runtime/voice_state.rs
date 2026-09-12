//! Session status is sampled only when admitting a genuine user prompt.
use agentkit_core::Item;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

const ACTIVE: &str = "[Kit voice session status] A voice session is active, including while its microphone is muted. This current status supersedes earlier voice status, including resumed or summarized context. This is internal, non-actionable context; do not acknowledge it or start work because of it. Prefer backgrounding long-running compose work to keep voice responsive. Preserve dependency ordering and keep calls foregrounded when their result is needed for the next step. When only waiting for background results and no independent work remains, end the turn; do not poll or issue keepalive calls. The harness resumes work when results arrive.";
const INACTIVE: &str = "[Kit voice session status] No voice session is active on this connection. This current status supersedes any earlier voice status, including in resumed or summarized context. This is internal, non-actionable context; do not acknowledge it or start work because of it. Voice-specific responsiveness guidance no longer applies; ordinary background-work and dependency-ordering instructions still apply. Existing tasks are not cancelled.";

/// The ACP session owns the strong reference; connection removal makes a
/// surviving user-prompt monitor observe inactive. Updates never wake the loop.
#[derive(Default)]
pub(crate) struct VoiceState(Arc<AtomicBool>);

impl VoiceState {
    pub(crate) fn set_active(&self, active: bool) {
        // One scalar is the complete shared invariant; no coordinated state,
        // callbacks, locks, or wakeups participate in this update.
        self.0.store(active, Ordering::Relaxed);
    }

    pub(crate) fn monitor(&self, correct_history: bool) -> VoiceMonitor {
        VoiceMonitor {
            current: Arc::downgrade(&self.0),
            baseline: if correct_history { None } else { Some(false) },
        }
    }
}

/// Actor-local baseline. Resumes/forks force a correction on the first user
/// prompt; fresh sessions start with the known inactive baseline.
/// Autonomous work and tool continuations must never call `submit`.
pub(crate) struct VoiceMonitor {
    current: Weak<AtomicBool>,
    baseline: Option<bool>,
}

impl VoiceMonitor {
    pub(crate) fn submit<E>(
        &mut self,
        mut user_items: Vec<Item>,
        submit: impl FnOnce(Vec<Item>) -> Result<(), E>,
    ) -> Result<(), E> {
        let current = self
            .current
            .upgrade()
            .is_some_and(|state| state.load(Ordering::Relaxed));
        if self.baseline != Some(current) {
            user_items.insert(
                0,
                Item::notification(if current { ACTIVE } else { INACTIVE }),
            );
        }
        submit(user_items)?;
        // A rejection/unwind leaves the baseline untouched. Updates concurrent
        // with submission stay pending when they differ from this sampled value.
        self.baseline = Some(current);
        Ok(())
    }
}
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use agentkit_core::{FinishReason, MetadataMap, ToolCallPart, TurnCancellation};
    use agentkit_core::{ItemKind, Part};
    use agentkit_loop::{
        Agent, LoopDriver, LoopError, LoopInterrupt, LoopStep, ModelAdapter, ModelSession,
        ModelTurn, ModelTurnEvent, ModelTurnResult, SessionConfig, TurnRequest,
    };
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    // Fake only the external model boundary; record the real inference request.
    #[derive(Clone)]
    struct Model {
        requests: mpsc::UnboundedSender<TurnRequest>,
        tool_first: bool,
    }
    struct Turn(std::collections::VecDeque<ModelTurnEvent>);

    #[async_trait]
    impl ModelAdapter for Model {
        type Session = Self;
        async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
            Ok(self.clone())
        }
    }
    #[async_trait]
    impl ModelSession for Model {
        type Turn = Turn;
        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _: Option<TurnCancellation>,
        ) -> Result<Turn, LoopError> {
            self.requests.send(request).unwrap();
            let tool = std::mem::take(&mut self.tool_first);
            let output_items = if tool {
                vec![Item::new(
                    ItemKind::Assistant,
                    vec![Part::ToolCall(ToolCallPart::new(
                        "call",
                        "missing-tool",
                        serde_json::json!({}),
                    ))],
                )]
            } else {
                vec![Item::text(ItemKind::Assistant, "done")]
            };
            let mut events = std::collections::VecDeque::new();
            if tool {
                let Part::ToolCall(call) = &output_items[0].parts[0] else {
                    unreachable!()
                };
                events.push_back(ModelTurnEvent::ToolCall(call.clone()));
            }
            events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
                model: None,
                response_id: None,
                finish_reason: if tool {
                    FinishReason::ToolCall
                } else {
                    FinishReason::Completed
                },
                output_items,
                usage: None,
                metadata: MetadataMap::new(),
            }));
            Ok(Turn(events))
        }
    }
    #[async_trait]
    impl ModelTurn for Turn {
        async fn next_event(
            &mut self,
            _: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.0.pop_front())
        }
    }
    async fn driver(
        transcript: Vec<Item>,
        tool_first: bool,
    ) -> (LoopDriver<Model>, mpsc::UnboundedReceiver<TurnRequest>) {
        let (requests, rx) = mpsc::unbounded_channel();
        let driver = Agent::builder()
            .model(Model {
                requests,
                tool_first,
            })
            .transcript(transcript)
            .build()
            .unwrap()
            .start(SessionConfig::new("voice-test"))
            .await
            .unwrap();
        (driver, rx)
    }
    fn statuses(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .filter(|item| item.kind == ItemKind::Notification)
            .filter_map(|item| match item.parts.as_slice() {
                [Part::Text(text)] if text.text.starts_with("[Kit voice session status] ") => {
                    Some(text.text.as_str())
                }
                _ => None,
            })
            .collect()
    }
    fn user() -> Vec<Item> {
        vec![Item::text(ItemKind::User, "work")]
    }
    async fn prompt(monitor: &mut VoiceMonitor, driver: &mut LoopDriver<Model>) {
        monitor
            .submit(user(), |items| driver.submit_input(items))
            .unwrap();
        assert!(
            matches!(driver.next().await.unwrap(), LoopStep::Finished(result) if result.finish_reason == FinishReason::Completed)
        );
    }

    #[tokio::test]
    async fn voice_state_idle_updates_wait_for_user_and_coalesce() {
        let state = VoiceState::default();
        let mut monitor = state.monitor(false);
        let (mut driver, mut requests) = driver(vec![], false).await;
        state.set_active(true);
        state.set_active(false);
        state.set_active(true);
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_))
        ));
        assert!(requests.try_recv().is_err());
        assert!(driver.snapshot().transcript.is_empty());
        prompt(&mut monitor, &mut driver).await;
        let request = requests.try_recv().unwrap();
        assert_eq!(statuses(&request.transcript), vec![ACTIVE]);
        assert_eq!(request.transcript[0].kind, ItemKind::Notification);
        assert_eq!(request.transcript[1].kind, ItemKind::User);
        state.set_active(false);
        state.set_active(true);
        prompt(&mut monitor, &mut driver).await;
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            vec![ACTIVE]
        );
        state.set_active(false);
        prompt(&mut monitor, &mut driver).await;
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            vec![ACTIVE, INACTIVE]
        );
    }

    #[tokio::test]
    async fn voice_state_resume_and_owner_drop_correct_only_next_user_prompt() {
        let state = VoiceState::default();
        let mut monitor = state.monitor(true);
        let (mut driver, mut requests) = driver(vec![Item::notification(ACTIVE)], false).await;
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_))
        ));
        assert_eq!(statuses(&driver.snapshot().transcript), vec![ACTIVE]);
        prompt(&mut monitor, &mut driver).await;
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            vec![ACTIVE, INACTIVE]
        );
        state.set_active(true);
        prompt(&mut monitor, &mut driver).await;
        requests.try_recv().unwrap();
        drop(state);
        assert_eq!(
            statuses(&driver.snapshot().transcript).last(),
            Some(&ACTIVE)
        );
        prompt(&mut monitor, &mut driver).await;
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript).last(),
            Some(&INACTIVE)
        );
    }

    #[test]
    fn voice_state_failed_submission_preserves_pending_and_concurrent_update() {
        let state = VoiceState::default();
        let mut monitor = state.monitor(false);
        state.set_active(true);
        let failure = monitor.submit(user(), |items| {
            assert_eq!(statuses(&items), vec![ACTIVE]);
            Err("rejected input")
        });
        assert_eq!(failure, Err("rejected input"));
        monitor
            .submit(user(), |items| {
                assert_eq!(statuses(&items), vec![ACTIVE]);
                state.set_active(false);
                Ok::<_, ()>(())
            })
            .unwrap();
        monitor
            .submit(user(), |items| {
                assert_eq!(statuses(&items), vec![INACTIVE]);
                Ok::<_, ()>(())
            })
            .unwrap();
        monitor
            .submit(user(), |items| {
                assert!(statuses(&items).is_empty());
                Ok::<_, ()>(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn voice_state_live_and_autonomous_work_do_not_consume_pending() {
        let state = VoiceState::default();
        let mut monitor = state.monitor(false);
        let (mut driver, mut requests) = driver(vec![], true).await;
        monitor
            .submit(user(), |items| driver.submit_input(items))
            .unwrap();
        loop {
            match driver.next().await.unwrap() {
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => break,
                LoopStep::Finished(result) if result.finish_reason == FinishReason::ToolCall => {}
                other => panic!("expected tool boundary, got {other:?}"),
            }
        }
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            Vec::<&str>::new()
        );
        state.set_active(true);
        assert!(
            matches!(driver.next().await.unwrap(), LoopStep::Finished(result) if result.finish_reason == FinishReason::Completed)
        );
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            Vec::<&str>::new()
        );
        // The real autonomous admission API, deliberately outside user monitor.
        driver
            .submit_input(vec![Item::notification("background task completed")])
            .unwrap();
        driver.next().await.unwrap();
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            Vec::<&str>::new()
        );
        prompt(&mut monitor, &mut driver).await;
        assert_eq!(
            statuses(&requests.try_recv().unwrap().transcript),
            vec![ACTIVE]
        );
    }
}

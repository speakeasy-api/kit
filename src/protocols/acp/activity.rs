use std::sync::{Arc, Mutex};

use agentkit_acp::AcpRuntimeError;
use agentkit_core::FinishReason;
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExecutionOrigin {
    #[default]
    Prompt,
    Autonomous,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Running,
    Settling,
}

/// An ordered snapshot, not another reducer. Projections select only wire format.
#[derive(Clone, Debug)]
pub(super) struct Transition {
    pub id: u64,
    pub origin: ExecutionOrigin,
    pub active: bool,
    pub reason: FinishReason,
    pub error: Option<String>,
}

#[derive(Default)]
struct Activity {
    state: State,
    next_id: u64,
    origin: ExecutionOrigin,
    current: Option<Transition>,
}

/// Session-owned lifecycle instrument shared by the actor and its observer.
/// Admission alone is silent; TurnStarted allocates the activity identity. All
/// logical turns drained by an execution share that identity until settlement.
/// Projection runs synchronously under the state lock: Running precedes content,
/// and the actor must flush final content/diagnostics before settling to Idle.
/// Projections only enqueue wire notifications; they must not reenter this
/// instrument while its ordered transition is being projected.
#[derive(Clone)]
pub(super) struct SessionActivity {
    state: Arc<Mutex<Activity>>,
    project: Arc<dyn Fn(Transition) -> Result<(), AcpRuntimeError> + Send + Sync>,
}

impl SessionActivity {
    pub(super) fn new(
        project: impl Fn(Transition) -> Result<(), AcpRuntimeError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(Activity::default())),
            project: Arc::new(project),
        }
    }

    pub(super) fn begin(&self, origin: ExecutionOrigin) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Continuations cannot redefine the origin of an existing interval.
        if state.state == State::Idle {
            state.origin = origin;
        }
    }

    pub(super) fn observe(&self, event: &AgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::TurnStarted { .. } => {
                let was_idle = state.state == State::Idle;
                state.state = State::Running;
                if was_idle {
                    state.next_id = state.next_id.wrapping_add(1);
                    let transition = Transition {
                        id: state.next_id,
                        origin: state.origin,
                        active: true,
                        reason: FinishReason::Completed,
                        error: None,
                    };
                    state.current = Some(transition.clone());
                    if let Err(error) = (self.project)(transition) {
                        tracing::debug!(%error, "failed to project session activity");
                    }
                }
            }
            AgentEvent::TurnFinished(result) if state.state != State::Idle => {
                state.state = State::Settling;
                if let Some(current) = &mut state.current {
                    current.reason = result.finish_reason.clone();
                }
            }
            _ => {}
        }
    }

    pub(super) fn settle(
        &self,
        reason: Option<FinishReason>,
        error: Option<String>,
    ) -> Result<(), AcpRuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.origin = ExecutionOrigin::Prompt;
        state.state = State::Idle;
        if let Some(mut terminal) = state.current.take() {
            terminal.active = false;
            if let Some(reason) = reason {
                terminal.reason = reason;
            }
            if error.is_some() {
                terminal.reason = FinishReason::Error;
            }
            terminal.error = error;
            (self.project)(terminal)?;
        }
        Ok(())
    }

    /// Both protocols use this boundary. The operation includes transport drain
    /// and diagnostics; only then may the single session interval terminalize.
    pub(super) async fn execute<T>(
        &self,
        origin: ExecutionOrigin,
        operation: impl std::future::Future<Output = Result<T, AcpRuntimeError>>,
        reason: impl FnOnce(&T) -> Option<FinishReason>,
    ) -> Result<T, AcpRuntimeError> {
        self.begin(origin);
        let result = operation.await;
        let terminal = result.as_ref().ok().and_then(reason);
        let settled = self.settle(terminal, result.as_ref().err().map(ToString::to_string));
        match result {
            Err(error) => Err(error),
            Ok(value) => settled.map(|()| value),
        }
    }
}

impl LoopObserver for SessionActivity {
    fn handle_event(&self, event: ObservedEvent) {
        self.observe(&event.event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{MetadataMap, SessionId, TurnId};

    fn started(id: &str) -> AgentEvent {
        AgentEvent::TurnStarted {
            session_id: SessionId::new("activity"),
            turn_id: TurnId::new(id),
        }
    }

    fn finished(reason: FinishReason) -> AgentEvent {
        AgentEvent::TurnFinished(agentkit_loop::TurnResult {
            turn_id: TurnId::new("logical-turn"),
            finish_reason: reason,
            items: Vec::new(),
            usage: None,
            metadata: MetadataMap::new(),
        })
    }

    #[tokio::test]
    async fn execution_owns_identity_origin_and_ordered_settlement() {
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let output = transitions.clone();
        let activity = SessionActivity::new(move |transition| {
            output.lock().unwrap().push(transition);
            Ok(())
        });
        activity
            .execute(ExecutionOrigin::Autonomous, async { Ok(()) }, |_| None)
            .await
            .unwrap();
        assert!(transitions.lock().unwrap().is_empty());
        for (index, reason) in [
            FinishReason::Completed,
            FinishReason::Cancelled,
            FinishReason::MaxTokens,
            FinishReason::Blocked,
            FinishReason::Error,
        ]
        .into_iter()
        .enumerate()
        {
            activity
                .execute(
                    ExecutionOrigin::Prompt,
                    async {
                        activity.observe(&started("first"));
                        assert!(transitions.lock().unwrap().last().unwrap().active);
                        activity.observe(&finished(FinishReason::ToolCall));
                        // Steering and background synthesis cannot redefine the interval.
                        activity.begin(ExecutionOrigin::Autonomous);
                        activity.observe(&started("continuation"));
                        activity.observe(&finished(reason.clone()));
                        assert_eq!(transitions.lock().unwrap().len(), index * 2 + 1);
                        Ok(())
                    },
                    |_| None,
                )
                .await
                .unwrap();
            activity.settle(None, None).unwrap();
            activity.observe(&finished(FinishReason::Error));
            let events = transitions.lock().unwrap();
            let running = &events[index * 2];
            let idle = &events[index * 2 + 1];
            assert_eq!(running.id, index as u64 + 1);
            assert_eq!(running.id, idle.id);
            assert_eq!(idle.origin, ExecutionOrigin::Prompt);
            assert!(!idle.active);
            assert_eq!(idle.reason, reason);
        }
        activity
            .execute(
                ExecutionOrigin::Autonomous,
                async {
                    activity.observe(&started("error"));
                    Err::<(), _>(AcpRuntimeError::Loop("terminal error".into()))
                },
                |_| None,
            )
            .await
            .unwrap_err();
        activity.settle(None, None).unwrap();
        let events = transitions.lock().unwrap();
        assert_eq!(events.len(), 12);
        let idle = events.last().unwrap();
        assert_eq!(idle.id, 6);
        assert_eq!(idle.origin, ExecutionOrigin::Autonomous);
        assert_eq!(idle.reason, FinishReason::Error);
        assert!(idle.error.as_ref().unwrap().contains("terminal error"));
    }

    #[tokio::test]
    async fn finalization_normalizes_cancellation_and_never_skips_failed_drain() {
        let diagnostic = Arc::new(Mutex::new(Vec::new()));
        let reason = finalize(
            ExecutionOutcome::new(Err(AcpRuntimeError::Loop("provider failed".into())), true),
            None,
            async { Ok(()) },
            |_| panic!("cancelled model failure is not an error"),
        )
        .await
        .unwrap();
        assert_eq!(reason, FinishReason::Cancelled);

        let order = Arc::new(Mutex::new(Vec::new()));
        let output = order.clone();
        let activity = SessionActivity::new(move |transition| {
            output
                .lock()
                .unwrap()
                .push(if transition.active { "running" } else { "idle" });
            Ok(())
        });
        let result = activity
            .execute(
                ExecutionOrigin::Autonomous,
                async {
                    activity.observe(&started("flush-failure"));
                    finalize(
                        ExecutionOutcome::new(Ok(FinishReason::Completed), false),
                        None,
                        async {
                            order.lock().unwrap().push("flush");
                            Err(AcpRuntimeError::ClientClosed)
                        },
                        |error| {
                            order.lock().unwrap().push("diagnostic");
                            diagnostic.lock().unwrap().push(error.to_string());
                            Ok(())
                        },
                    )
                    .await
                },
                |reason| Some(reason.clone()),
            )
            .await;
        assert!(matches!(result, Err(AcpRuntimeError::ClientClosed)));
        activity.settle(None, None).unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            ["running", "flush", "diagnostic", "idle"]
        );
        assert_eq!(diagnostic.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_terminal_projection_is_not_retried() {
        let calls = Arc::new(Mutex::new(0));
        let output = calls.clone();
        let activity = SessionActivity::new(move |_| {
            *output.lock().unwrap() += 1;
            Err(AcpRuntimeError::ClientClosed)
        });
        activity
            .execute(
                ExecutionOrigin::Autonomous,
                async {
                    activity.observe(&started("first"));
                    Ok(())
                },
                |_| Some(FinishReason::Cancelled),
            )
            .await
            .unwrap_err();
        activity.settle(None, None).unwrap();
        assert_eq!(*calls.lock().unwrap(), 2);
    }
}

/// Domain outcome selected before any protocol representation. Cancellation wins
/// over model/approval errors; cleanup or delivery failure cannot become success.
pub(super) struct ExecutionOutcome {
    result: Result<FinishReason, AcpRuntimeError>,
}

impl ExecutionOutcome {
    pub(super) fn new(result: Result<FinishReason, AcpRuntimeError>, cancelled: bool) -> Self {
        Self {
            result: if cancelled || matches!(result, Err(AcpRuntimeError::Cancelled)) {
                Ok(FinishReason::Cancelled)
            } else {
                result
            },
        }
    }
}

/// Shared finalization order: cancel/drain structured work, drain all content even
/// on failure, render a diagnostic, then return the outcome for single settlement.
/// Hooks are transport only; neither hook chooses lifecycle or cleanup policy.
pub(super) async fn finalize(
    outcome: ExecutionOutcome,
    structured: Option<(
        &agentkit_task_manager::TaskManagerHandle,
        &crate::runtime::BackgroundJobs,
    )>,
    flush: impl std::future::Future<Output = Result<(), AcpRuntimeError>>,
    diagnostic: impl FnOnce(&AcpRuntimeError) -> Result<(), AcpRuntimeError>,
) -> Result<FinishReason, AcpRuntimeError> {
    let mut result = outcome.result;
    if (result.is_err() || matches!(result, Ok(FinishReason::Cancelled)))
        && let Some((tasks, jobs)) = structured
    {
        super::cancel_background_jobs(tasks, jobs).await;
        if let Err(error) = super::settle_background_jobs(tasks, jobs).await {
            result = Err(error);
        }
    }
    if let Err(error) = flush.await {
        result = Err(error);
    }
    if let Err(error) = &result {
        diagnostic(error)?;
    }
    result
}

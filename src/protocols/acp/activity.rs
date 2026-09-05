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
    Unavailable,
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
    projecting: bool,
    executing: bool,
}

/// Session-owned lifecycle instrument shared by the actor and its observer.
/// Admission alone is silent; TurnStarted allocates the activity identity. All
/// logical turns drained by an execution share that identity until settlement.
/// Projection runs synchronously outside the state lock. An in-flight claim
/// prevents another projection from overtaking it, including callback reentry.
/// The actor must flush final content/diagnostics before settling to Idle.
/// Abandoned executions/projections isolate this owner: external delivery cannot
/// be rolled back or safely retried after unwind or cancellation.
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

    #[cfg(test)]
    pub(super) fn begin(&self, origin: ExecutionOrigin) {
        let Ok(mut state) = self.state.lock() else {
            return; // A poisoned owner is never reused.
        };
        // Continuations cannot redefine the origin of an existing interval.
        if state.state == State::Idle && !state.projecting && !state.executing {
            state.origin = origin;
        }
    }

    pub(super) fn observe(&self, event: &AgentEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.state == State::Unavailable || state.projecting {
            return;
        }
        let transition = match event {
            AgentEvent::TurnStarted { .. } => {
                let was_idle = state.state == State::Idle;
                if was_idle {
                    let Some(id) = state.next_id.checked_add(1) else {
                        state.state = State::Unavailable;
                        return;
                    };
                    let transition = Transition {
                        id,
                        origin: state.origin,
                        active: true,
                        reason: FinishReason::Completed,
                        error: None,
                    };
                    state.current = Some(transition.clone());
                    state.next_id = id;
                    state.state = State::Running;
                    state.projecting = true;
                    Some(transition)
                } else {
                    state.state = State::Running;
                    None
                }
            }
            AgentEvent::TurnFinished(result) if state.state != State::Idle => {
                state.state = State::Settling;
                if let Some(current) = &mut state.current {
                    current.reason = result.finish_reason.clone();
                }
                None
            }
            _ => None,
        };
        drop(state);
        if let Some(transition) = transition
            && let Err(error) = self.project_transition(transition)
        {
            tracing::debug!(%error, "failed to project session activity");
        }
    }

    fn project_transition(&self, transition: Transition) -> Result<(), AcpRuntimeError> {
        // The claim is already installed. Drop isolates the owner if the client
        // callback unwinds; it never calls client code during unwind.
        let mut claim = ActivityClaim {
            activity: self,
            projection: true,
            completed: false,
        };
        let result = (self.project)(transition);
        claim.completed = true;
        result
    }

    pub(super) fn settle(
        &self,
        reason: Option<FinishReason>,
        error: Option<String>,
    ) -> Result<(), AcpRuntimeError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AcpRuntimeError::Loop("session activity is poisoned".into()))?;
        if state.state == State::Unavailable || state.projecting {
            return Err(AcpRuntimeError::Loop(
                "session activity is unavailable".into(),
            ));
        }
        state.origin = ExecutionOrigin::Prompt;
        state.state = State::Idle;
        let terminal = state.current.take().map(|mut terminal| {
            terminal.active = false;
            if let Some(reason) = reason {
                terminal.reason = reason;
            }
            if error.is_some() {
                terminal.reason = FinishReason::Error;
            }
            terminal.error = error;
            terminal
        });
        state.projecting = terminal.is_some();
        drop(state);
        if let Some(terminal) = terminal {
            // A returned transport error is an at-most-once projection attempt,
            // not a reason to replay an already consumed terminal transition.
            self.project_transition(terminal)?;
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
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| AcpRuntimeError::Loop("session activity is poisoned".into()))?;
            if state.state == State::Unavailable || state.projecting || state.executing {
                return Err(AcpRuntimeError::Loop(
                    "session activity is unavailable".into(),
                ));
            }
            if state.state == State::Idle {
                state.origin = origin;
            }
            state.executing = true;
        }
        let mut claim = ActivityClaim {
            activity: self,
            projection: false,
            completed: false,
        };
        let result = operation.await;
        let terminal = result.as_ref().ok().and_then(reason);
        let settled = self.settle(terminal, result.as_ref().err().map(ToString::to_string));
        claim.completed = true;
        match result {
            Err(error) => Err(error),
            Ok(value) => settled.map(|()| value),
        }
    }
}

/// No callbacks, awaits, or external effects occur while releasing a claim.
/// Poison is left isolated rather than recovering potentially incomplete state.
struct ActivityClaim<'a> {
    activity: &'a SessionActivity,
    projection: bool,
    completed: bool,
}

impl Drop for ActivityClaim<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.activity.state.lock() {
            if self.projection {
                state.projecting = false;
            } else {
                state.executing = false;
            }
            if !self.completed || (!self.projection && state.state != State::Idle) {
                // A failed settlement must not hand an unterminated interval
                // to another execution, even if a concurrent projection won.
                state.state = State::Unavailable;
            }
        }
    }
}

impl LoopObserver for SessionActivity {
    fn handle_event(&self, event: ObservedEvent) {
        self.observe(&event.event);
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

    #[test]
    fn projection_unwind_isolates_without_poison_or_replay() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for fail_active in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            let output = calls.clone();
            let activity = SessionActivity::new(move |transition| {
                output.fetch_add(1, Ordering::Relaxed);
                assert_ne!(transition.active, fail_active, "projection failed");
                Ok(())
            });
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                activity.observe(&started("first"));
                activity.settle(None, None).unwrap();
            }));
            assert!(result.is_err());
            assert!(!activity.state.is_poisoned());
            activity.begin(ExecutionOrigin::Autonomous);
            activity.observe(&started("later"));
            assert!(matches!(
                activity.settle(None, None),
                Err(AcpRuntimeError::Loop(_))
            ));
            assert_eq!(
                calls.load(Ordering::Relaxed),
                if fail_active { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn projection_reentry_is_rejected_without_deadlock_or_reordering() {
        let owner = Arc::new(std::sync::OnceLock::<SessionActivity>::new());
        let callback_owner = Arc::downgrade(&owner);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let output = calls.clone();
        let activity = SessionActivity::new(move |_| {
            let owner = callback_owner.upgrade().unwrap();
            let activity = owner.get().unwrap();
            assert!(activity.state.try_lock().is_ok());
            assert!(matches!(
                activity.settle(None, None),
                Err(AcpRuntimeError::Loop(_))
            ));
            activity.observe(&started("reentrant"));
            output.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        assert!(owner.set(activity.clone()).is_ok());
        activity.observe(&started("first"));
        activity.settle(None, None).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(activity.state.lock().unwrap().next_id, 1);
    }

    #[tokio::test]
    async fn dropped_execution_isolates_before_another_operation_can_run() {
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let output = transitions.clone();
        let activity = SessionActivity::new(move |transition| {
            output.lock().unwrap().push(transition);
            Ok(())
        });
        let mut execution = Box::pin(activity.execute(
            ExecutionOrigin::Autonomous,
            async {
                activity.observe(&started("first"));
                std::future::pending::<Result<(), AcpRuntimeError>>().await
            },
            |_| None,
        ));
        assert!(futures_util::poll!(execution.as_mut()).is_pending());
        // A concurrent execution must not steal the live interval either.
        assert!(
            activity
                .execute(
                    ExecutionOrigin::Prompt,
                    async { panic!("must not run") },
                    |_: &()| None
                )
                .await
                .is_err()
        );
        drop(execution);
        activity.observe(&started("stale"));
        assert!(matches!(
            activity
                .execute(
                    ExecutionOrigin::Prompt,
                    async { panic!("must not run") },
                    |_: &()| None
                )
                .await,
            Err(AcpRuntimeError::Loop(_))
        ));
        assert_eq!(transitions.lock().unwrap().len(), 1);
        assert!(!activity.state.is_poisoned());
    }

    #[test]
    fn exhausted_identity_and_poison_never_resume_projection() {
        let activity = SessionActivity::new(|_| panic!("must not project"));
        activity.state.lock().unwrap().next_id = u64::MAX;
        activity.observe(&started("overflow"));
        assert!(activity.settle(None, None).is_err());
        let poisoned = activity.clone();
        assert!(
            std::thread::spawn(move || {
                let _state = poisoned.state.lock().unwrap();
                panic!("state mutation interrupted");
            })
            .join()
            .is_err()
        );
        activity.begin(ExecutionOrigin::Prompt);
        activity.observe(&started("after-poison"));
        assert!(matches!(
            activity.settle(None, None),
            Err(AcpRuntimeError::Loop(_))
        ));
    }

    #[tokio::test]
    async fn outcome_callback_unwind_isolates_the_execution_owner() {
        use futures_util::FutureExt;
        let activity = SessionActivity::new(|_| Ok(()));
        let result = std::panic::AssertUnwindSafe(activity.execute(
            ExecutionOrigin::Prompt,
            async {
                activity.observe(&started("first"));
                Ok(())
            },
            |_| panic!("outcome callback failed"),
        ))
        .catch_unwind()
        .await;
        assert!(result.is_err());
        assert!(!activity.state.is_poisoned());
        assert!(activity.settle(None, None).is_err());
        assert!(
            activity
                .execute(
                    ExecutionOrigin::Prompt,
                    async { panic!("must not run") },
                    |_: &()| None
                )
                .await
                .is_err()
        );
    }
}

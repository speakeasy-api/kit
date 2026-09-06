//! Persist acknowledged steering without executing its cancelled continuation.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use agentkit_core::FinishReason;
use agentkit_loop::{
    LoopCtx, LoopDriver, LoopError, LoopMutator, LoopStep, ModelSession, TranscriptCursor,
};
use async_trait::async_trait;

/// A per-driver fence, registered FIRST, before compaction or any other mutator.
/// Clones share the fence between the registered mutator and its driver wrapper;
/// do not share it between drivers.
#[derive(Clone, Default)]
pub(crate) struct InputSettlement(Arc<AtomicBool>);

impl InputSettlement {
    /// Wrap the driver on which a clone of this fence was registered first.
    pub(crate) fn wrap<S: ModelSession>(self, driver: LoopDriver<S>) -> InputSettlingDriver<S> {
        InputSettlingDriver {
            driver,
            settlement: self,
            settlement_failed: false,
        }
    }
}

#[async_trait]
impl LoopMutator for InputSettlement {
    async fn mutate(
        &self,
        _cursor: &mut TranscriptCursor<'_>,
        _ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        if self.0.load(Ordering::Acquire) {
            Err(LoopError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Owns the only arming scope. Drop resets the fence on errors, unwind, and
/// cancellation of the settlement future, without calling external code.
struct ArmedSettlement<'a>(&'a AtomicBool);

impl Drop for ArmedSettlement<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) struct InputSettlingDriver<S: ModelSession> {
    driver: LoopDriver<S>,
    settlement: InputSettlement,
    // Set before settlement can suspend; only verified success restores use.
    settlement_failed: bool,
}

impl<S: ModelSession> InputSettlingDriver<S> {
    /// False after incomplete settlement, including a dropped settlement future.
    /// Actor owners must retire unavailable drivers rather than drive them again.
    pub(crate) fn is_available(&self) -> bool {
        !self.settlement_failed
    }

    /// Transfer ownership to a protocol that does not settle injected input.
    pub(crate) fn into_inner(self) -> LoopDriver<S> {
        self.driver
    }
}

impl<S: ModelSession> Deref for InputSettlingDriver<S> {
    type Target = LoopDriver<S>;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

impl<S: ModelSession> DerefMut for InputSettlingDriver<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.driver
    }
}

impl<S: ModelSession + Send + 'static> InputSettlingDriver<S> {
    /// Settle only successfully delivered steering after cancellation at an
    /// injection boundary whose pending-input baseline was verified empty.
    /// Original unstarted prompts and synthetic compaction input must not use
    /// this path. The caller must stop driving on error or an aborted future.
    ///
    /// Retirement removes foreground resumptions before the single fenced
    /// step. That step appends input through the normal transcript observer,
    /// then the first mutator cancels before compaction or model execution.
    /// It produces a fresh cancelled logical turn and its normal diagnostic.
    pub(crate) async fn settle_delivered_input(&mut self) -> Result<(), LoopError> {
        if !self.is_available() {
            return Err(LoopError::InvalidState(
                "driver is unavailable after incomplete input settlement".into(),
            ));
        }
        self.settlement_failed = true;
        if self.driver.snapshot().pending_input.is_empty() {
            return Err(LoopError::InvalidState(
                "input settlement requires delivered pending input".into(),
            ));
        }
        self.driver.retire_interrupted_turn().await?;
        self.settlement
            .0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                LoopError::InvalidState("input settlement fence is already armed".into())
            })?;
        let _armed = ArmedSettlement(&self.settlement.0);
        let step = self.driver.next().await?;
        if !matches!(step, LoopStep::Finished(turn) if turn.finish_reason == FinishReason::Cancelled)
            || !self.driver.snapshot().pending_input.is_empty()
        {
            return Err(LoopError::InvalidState(
                "input settlement did not finish cancelled with empty pending input".into(),
            ));
        }
        self.settlement_failed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agentkit_core::{Item, ItemKind, TurnCancellation};
    use agentkit_loop::{
        Agent, ModelAdapter, ModelTurn, ModelTurnEvent, SessionConfig, TurnRequest,
    };

    use super::*;

    struct UnavailableProvider;

    #[async_trait]
    impl ModelAdapter for UnavailableProvider {
        type Session = Self;

        async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
            Ok(Self)
        }
    }

    #[async_trait]
    impl ModelSession for UnavailableProvider {
        type Turn = Self;

        async fn begin_turn(
            &mut self,
            _: TurnRequest,
            _: Option<TurnCancellation>,
        ) -> Result<Self, LoopError> {
            Err(LoopError::Provider("provider unavailable".into()))
        }
    }

    #[async_trait]
    impl ModelTurn for UnavailableProvider {
        async fn next_event(
            &mut self,
            _: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            unreachable!("the unavailable provider cannot start a turn")
        }
    }

    struct UnavailableCompactor;

    #[async_trait]
    impl LoopMutator for UnavailableCompactor {
        async fn mutate(
            &self,
            _: &mut TranscriptCursor<'_>,
            _: LoopCtx<'_>,
        ) -> Result<(), LoopError> {
            Err(LoopError::Mutator("compactor unavailable".into()))
        }
    }

    /// An unavailable task service can either fail its update read or leave it
    /// pending. All other task operations use the real synchronous manager.
    struct UnavailableTaskService {
        inner: agentkit_task_manager::SimpleTaskManager,
        pending: bool,
    }

    #[async_trait]
    impl agentkit_task_manager::TaskManager for UnavailableTaskService {
        async fn start_task(
            &self,
            request: agentkit_task_manager::TaskLaunchRequest,
            ctx: agentkit_task_manager::TaskStartContext,
        ) -> Result<agentkit_task_manager::TaskStartOutcome, agentkit_task_manager::TaskManagerError>
        {
            self.inner.start_task(request, ctx).await
        }

        async fn wait_for_turn(
            &self,
            turn_id: &agentkit_core::TurnId,
            cancellation: Option<TurnCancellation>,
        ) -> Result<
            Option<agentkit_task_manager::TurnTaskUpdate>,
            agentkit_task_manager::TaskManagerError,
        > {
            self.inner.wait_for_turn(turn_id, cancellation).await
        }

        async fn take_pending_loop_updates(
            &self,
        ) -> Result<
            agentkit_task_manager::PendingLoopUpdates,
            agentkit_task_manager::TaskManagerError,
        > {
            if self.pending {
                std::future::pending().await
            } else {
                Err(agentkit_task_manager::TaskManagerError::Internal(
                    "task service unavailable".into(),
                ))
            }
        }

        async fn on_turn_interrupted(
            &self,
            turn_id: &agentkit_core::TurnId,
        ) -> Result<(), agentkit_task_manager::TaskManagerError> {
            self.inner.on_turn_interrupted(turn_id).await
        }

        fn handle(&self) -> agentkit_task_manager::TaskManagerHandle {
            self.inner.handle()
        }
    }

    #[tokio::test]
    async fn failed_or_aborted_settlement_keeps_driver_unavailable() {
        use std::{future::Future, task::Poll};

        for pending in [false, true] {
            let settlement = InputSettlement::default();
            let raw = Agent::builder()
                .model(UnavailableProvider)
                .mutator(settlement.clone())
                .task_manager(UnavailableTaskService {
                    inner: agentkit_task_manager::SimpleTaskManager::new(),
                    pending,
                })
                .build()
                .unwrap()
                .start(SessionConfig::new("failed-settlement"))
                .await
                .unwrap();
            let mut driver = settlement.clone().wrap(raw);
            driver
                .submit_input(vec![Item::text(ItemKind::User, "delivered steering")])
                .unwrap();
            assert!(driver.is_available());
            if pending {
                let mut settling = Box::pin(driver.settle_delivered_input());
                std::future::poll_fn(|cx| {
                    assert!(settling.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(settling);
            } else {
                assert!(matches!(
                    driver.settle_delivered_input().await,
                    Err(LoopError::Tool(_))
                ));
            }
            assert!(!driver.is_available());
            assert!(!settlement.0.load(Ordering::Acquire));
            assert!(matches!(
                driver.settle_delivered_input().await,
                Err(LoopError::InvalidState(_))
            ));
            assert!(!driver.is_available());
        }
    }

    #[tokio::test]
    async fn settlement_persists_without_compactor_or_provider_and_disarms() {
        for with_compactor in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let id = "input-settlement";
            let opened = crate::session::open_in(
                root.path(),
                storage.path(),
                id,
                false,
                false,
                vec![Item::text(ItemKind::System, "system")],
            )
            .unwrap();
            let settlement = InputSettlement::default();
            let builder = Agent::builder()
                .model(UnavailableProvider)
                .mutator(settlement.clone())
                .transcript(opened.transcript)
                .transcript_observer(opened.observer);
            let builder = if with_compactor {
                builder.mutator(UnavailableCompactor)
            } else {
                builder
            };
            let raw = builder
                .build()
                .unwrap()
                .start(SessionConfig::new(id))
                .await
                .unwrap();
            let mut driver = settlement.wrap(raw);
            let delivered = Item::text(ItemKind::User, "delivered steering");
            driver.submit_input(vec![delivered.clone()]).unwrap();
            driver.settle_delivered_input().await.unwrap();
            assert!(driver.is_available());
            let snapshot = driver.snapshot();
            assert!(snapshot.pending_input.is_empty());
            assert_eq!(snapshot.transcript[1].parts, delivered.parts);
            assert!(snapshot.transcript[1].created_at.is_some());
            assert_eq!(
                crate::session::load_in(root.path(), storage.path(), id).unwrap(),
                snapshot.transcript
            );

            driver
                .submit_input(vec![Item::text(ItemKind::User, "fresh prompt")])
                .unwrap();
            let error = driver.next().await.unwrap_err();
            if with_compactor {
                assert!(matches!(error, LoopError::Mutator(_)));
            } else {
                assert!(matches!(error, LoopError::Provider(_)));
            }
        }
    }
}

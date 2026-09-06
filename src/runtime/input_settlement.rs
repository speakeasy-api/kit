//! Persist acknowledged steering without executing its cancelled continuation.

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use agentkit_core::{FinishReason, Item};
use agentkit_loop::{
    LoopCtx, LoopDriver, LoopError, LoopMutator, LoopStep, ModelSession, TranscriptCursor,
    TranscriptEvent, TranscriptObserver,
};
use async_trait::async_trait;

/// A per-driver fence, registered FIRST, before compaction or any other mutator.
/// Clones share the fence between the registered mutator and its driver wrapper;
/// do not share it between drivers.
#[derive(Clone, Default)]
pub(crate) struct InputSettlement(Arc<Mutex<SettlementState>>);

// Only the wrapper arms and RAII disarms. Idle has no exclusion; an armed
// fence is either full (no exclusion) or partial. The observer advances the
// pending position; the first mutator verifies and strips. No guard spans an
// await or an external callback. Poison isolates all users except reset-only Drop.
#[derive(Default)]
struct SettlementState {
    active: bool,
    exclusion: Option<ExcludedInput>,
}

/// Disposition of the raw queue, tracked by position rather than item identity.
struct ExcludedInput {
    pending: Vec<Item>,
    acknowledged: usize,
    observed: usize,
    transcript_len: usize,
    rejected: bool,
    stripped: bool,
}

fn matches_stamped(raw: &Item, stamped: &Item) -> bool {
    let mut normalized = stamped.clone();
    if raw.created_at.is_none() {
        normalized.created_at = None;
    }
    raw == &normalized
}

struct SettlementObserver<O> {
    settlement: InputSettlement,
    inner: O,
}

impl<O: TranscriptObserver> TranscriptObserver for SettlementObserver<O> {
    fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
        let forward = {
            let Ok(mut state) = self.settlement.0.lock() else {
                // An uncertain disposition must never reach persistence.
                return;
            };
            match state.exclusion.as_mut() {
                None => true,
                // The first mutator verified and removed the raw suffix. The
                // pinned driver now appends its normal cancellation diagnostic.
                Some(plan) if plan.stripped => true,
                Some(plan) => {
                    if plan.rejected
                        || !plan
                            .pending
                            .get(plan.observed)
                            .is_some_and(|raw| matches_stamped(raw, event.item))
                    {
                        plan.rejected = true;
                        false
                    } else {
                        let forward = plan.observed < plan.acknowledged;
                        plan.observed += 1;
                        forward
                    }
                }
            }
        };
        // Persistence may panic or acquire locks. Never call it under the
        // disposition lock; RAII isolates the driver on unwind.
        if forward {
            self.inner.on_transcript_event(event);
        }
    }
}

impl InputSettlement {
    /// Register this around the real persistence observer on the same driver.
    pub(crate) fn observer<O: TranscriptObserver>(
        &self,
        inner: O,
    ) -> impl TranscriptObserver + use<O> {
        SettlementObserver {
            settlement: self.clone(),
            inner,
        }
    }

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
        cursor: &mut TranscriptCursor<'_>,
        _ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| LoopError::InvalidState("input settlement state poisoned".into()))?;
        if !state.active {
            return Ok(());
        }
        if let Some(plan) = state.exclusion.as_mut() {
            if plan.rejected
                || plan.stripped
                || plan.observed != plan.pending.len()
                || cursor.len() != plan.transcript_len + plan.pending.len()
                || !plan
                    .pending
                    .iter()
                    .zip(&cursor[plan.transcript_len..])
                    .all(|(raw, stamped)| matches_stamped(raw, stamped))
            {
                return Err(LoopError::InvalidState(
                    "input settlement did not observe the expected pending tail".into(),
                ));
            }
            cursor.truncate(plan.transcript_len + plan.acknowledged);
            plan.stripped = true;
        }
        Err(LoopError::Cancelled)
    }
}

/// Owns the only arming scope. Drop resets all disposition state on errors,
/// unwind, and cancellation, without calling external code.
struct ArmedSettlement<'a>(&'a InputSettlement);

impl Drop for ArmedSettlement<'_> {
    fn drop(&mut self) {
        let old = {
            // Recovery is reset-only: discard EVERY field, never continue a
            // possibly incomplete transition. The owning driver stays unavailable.
            let mut state = self.0.0.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *state)
        };
        drop(old);
    }
}

pub(crate) struct InputSettlingDriver<S: ModelSession> {
    driver: LoopDriver<S>,
    settlement: InputSettlement,
    // Set before settlement can suspend; only verified full success restores use.
    settlement_failed: bool,
}

impl<S: ModelSession> InputSettlingDriver<S> {
    /// False after partial or incomplete settlement, including a dropped future.
    /// Actor owners must retire unavailable drivers rather than drive them again.
    pub(crate) fn is_available(&self) -> bool {
        !self.settlement_failed
    }

    /// Isolate a driver whose input acknowledgement boundary is uncertain.
    pub(crate) fn make_unavailable(&mut self) {
        self.settlement_failed = true;
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
        self.settle_input(None).await
    }

    /// Settle an exactly acknowledged prefix of the raw pending queue. The caller
    /// supplies the original submitted items in order (including duplicates).
    /// Requires the same empty injection-boundary baseline as full settlement.
    /// An empty prefix discards the entire queue. Register `settlement.observer`
    /// around persistence. A proper partial settlement leaves this driver
    /// unavailable even on success: the actor MUST retire it, never retry the suffix.
    pub(crate) async fn settle_acknowledged_input(
        &mut self,
        acknowledged: &[Item],
    ) -> Result<(), LoopError> {
        self.settle_input(Some(acknowledged)).await
    }

    async fn settle_input(&mut self, acknowledged: Option<&[Item]>) -> Result<(), LoopError> {
        if !self.is_available() {
            return Err(LoopError::InvalidState(
                "driver is unavailable after partial or incomplete input settlement".into(),
            ));
        }
        self.settlement_failed = true;
        let pending = self.driver.snapshot().pending_input;
        if pending.is_empty() {
            return Err(LoopError::InvalidState(
                "input settlement requires delivered pending input".into(),
            ));
        }
        let acknowledged = acknowledged.unwrap_or(&pending);
        if !pending.starts_with(acknowledged) {
            return Err(LoopError::InvalidState(
                "acknowledged input is not an exact pending prefix".into(),
            ));
        }
        let partial = acknowledged.len() < pending.len();
        let acknowledged_count = acknowledged.len();
        self.driver.retire_interrupted_turn().await?;
        let exclusion = partial.then(|| ExcludedInput {
            pending,
            acknowledged: acknowledged_count,
            observed: 0,
            transcript_len: self.driver.snapshot().transcript.len(),
            rejected: false,
            stripped: false,
        });
        {
            let mut state =
                self.settlement.0.lock().map_err(|_| {
                    LoopError::InvalidState("input settlement state poisoned".into())
                })?;
            if state.active {
                return Err(LoopError::InvalidState(
                    "input settlement fence is already armed".into(),
                ));
            }
            *state = SettlementState {
                active: true,
                exclusion,
            };
        }
        let _armed = ArmedSettlement(&self.settlement);
        let step = self.driver.next().await?;
        if !matches!(step, LoopStep::Finished(turn) if turn.finish_reason == FinishReason::Cancelled)
            || !self.driver.snapshot().pending_input.is_empty()
        {
            return Err(LoopError::InvalidState(
                "input settlement did not finish cancelled with empty pending input".into(),
            ));
        }
        if partial {
            let state =
                self.settlement.0.lock().map_err(|_| {
                    LoopError::InvalidState("input settlement state poisoned".into())
                })?;
            if !state
                .exclusion
                .as_ref()
                .is_some_and(|plan| plan.stripped && !plan.rejected)
            {
                return Err(LoopError::InvalidState(
                    "input settlement exclusion was not verified".into(),
                ));
            }
        }
        self.settlement_failed = partial;
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
            assert!(!settlement.0.lock().unwrap().active);
            assert!(matches!(
                driver.settle_delivered_input().await,
                Err(LoopError::InvalidState(_))
            ));
            assert!(!driver.is_available());
        }
    }

    fn persisted_records(storage: &std::path::Path, id: &str) -> String {
        let directory = std::fs::read_dir(storage)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        std::fs::read_to_string(directory.join(format!("{id}.jsonl"))).unwrap()
    }

    #[tokio::test]
    async fn acknowledged_prefix_persists_by_position_without_temporary_suffix() {
        for stamped in [false, true] {
            for accepted in [0, 1, 2, 3, 4] {
                let root = tempfile::tempdir().unwrap();
                let storage = tempfile::tempdir().unwrap();
                let id = "partial-settlement";
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
                let raw = Agent::builder()
                    .model(UnavailableProvider)
                    .mutator(settlement.clone())
                    .mutator(UnavailableCompactor)
                    .transcript(opened.transcript)
                    .transcript_observer(settlement.observer(opened.observer))
                    .build()
                    .unwrap()
                    .start(SessionConfig::new(id))
                    .await
                    .unwrap();
                let mut driver = settlement.clone().wrap(raw);
                let mut same = Item::text(ItemKind::User, "queued-identical");
                if stamped {
                    same.created_at = Some(agentkit_core::Timestamp::now());
                }
                let pending = vec![
                    same.clone(),
                    same.clone(),
                    same,
                    Item::text(ItemKind::User, "unacknowledged-distinct"),
                ];
                driver.submit_input(pending.clone()).unwrap();
                driver
                    .settle_acknowledged_input(&pending[..accepted])
                    .await
                    .unwrap();
                assert_eq!(driver.is_available(), accepted == pending.len());
                let snapshot = driver.snapshot();
                assert!(snapshot.pending_input.is_empty());
                assert_eq!(snapshot.transcript.len(), 2 + accepted);
                for (raw, actual) in pending[..accepted]
                    .iter()
                    .zip(&snapshot.transcript[1..1 + accepted])
                {
                    assert!(matches_stamped(raw, actual));
                    assert!(actual.created_at.is_some());
                }
                assert_eq!(
                    crate::session::load_in(root.path(), storage.path(), id).unwrap(),
                    snapshot.transcript
                );
                // Inspect append history, not only the canonical reload: a later
                // replacement must not conceal a temporarily persisted suffix.
                let records = persisted_records(storage.path(), id);
                assert_eq!(records.matches("queued-identical").count(), accepted.min(3));
                assert_eq!(records.contains("unacknowledged-distinct"), accepted == 4);
                {
                    let state = settlement.0.lock().unwrap();
                    assert!(!state.active && state.exclusion.is_none());
                }
                if accepted < pending.len() {
                    assert!(driver.settle_delivered_input().await.is_err());
                    assert_eq!(persisted_records(storage.path(), id), records);
                }
            }
        }
    }

    #[tokio::test]
    async fn partial_failure_abort_and_wrong_prefix_never_persist_suffix() {
        use std::{future::Future, task::Poll};

        for mode in [
            "failure",
            "abort",
            "wrong-prefix",
            "too-long",
            "missing-observer",
        ] {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let id = "failed-partial";
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
                .transcript(opened.transcript);
            // Missing filtering registration must fail closed at the mutator.
            let builder = if mode == "missing-observer" {
                builder
            } else {
                builder
                    .transcript_observer(settlement.observer(opened.observer.clone()))
                    .task_manager(UnavailableTaskService {
                        inner: agentkit_task_manager::SimpleTaskManager::new(),
                        pending: mode == "abort",
                    })
            };
            let raw = builder
                .build()
                .unwrap()
                .start(SessionConfig::new(id))
                .await
                .unwrap();
            let mut driver = settlement.clone().wrap(raw);
            let a = Item::text(ItemKind::User, "acknowledged");
            let b = Item::text(ItemKind::User, "excluded");
            let pending = vec![a.clone(), b.clone()];
            driver.submit_input(pending.clone()).unwrap();
            let witness = match mode {
                "wrong-prefix" => vec![b],
                "too-long" => vec![a.clone(), b, a],
                _ => vec![a],
            };
            if mode == "abort" {
                let mut settling = Box::pin(driver.settle_acknowledged_input(&witness));
                std::future::poll_fn(|cx| {
                    assert!(settling.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(settling);
            } else {
                assert!(driver.settle_acknowledged_input(&witness).await.is_err());
            }
            assert!(!driver.is_available());
            {
                let state = settlement.0.lock().unwrap();
                assert!(!state.active && state.exclusion.is_none());
            }
            let records = persisted_records(storage.path(), id);
            assert!(!records.contains("acknowledged") && !records.contains("excluded"));
            assert_eq!(
                crate::session::load_in(root.path(), storage.path(), id)
                    .unwrap()
                    .len(),
                1
            );
            if mode != "missing-observer" {
                assert_eq!(driver.snapshot().pending_input, pending);
            }
            assert!(driver.settle_acknowledged_input(&witness).await.is_err());
            assert_eq!(persisted_records(storage.path(), id), records);
        }
    }

    struct PanicAfterPersist<O>(O);

    impl<O: TranscriptObserver> TranscriptObserver for PanicAfterPersist<O> {
        fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
            self.0.on_transcript_event(event);
            panic!("observer failed after persistence");
        }
    }

    #[tokio::test]
    async fn partial_observer_unwind_disarms_without_poison_or_retry() {
        use futures_util::FutureExt;

        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let id = "unwind-partial";
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
        let raw = Agent::builder()
            .model(UnavailableProvider)
            .mutator(settlement.clone())
            .transcript(opened.transcript)
            .transcript_observer(settlement.observer(PanicAfterPersist(opened.observer)))
            .build()
            .unwrap()
            .start(SessionConfig::new(id))
            .await
            .unwrap();
        let mut driver = settlement.clone().wrap(raw);
        let a = Item::text(ItemKind::User, "acknowledged");
        driver
            .submit_input(vec![a.clone(), Item::text(ItemKind::User, "excluded")])
            .unwrap();
        assert!(
            std::panic::AssertUnwindSafe(driver.settle_acknowledged_input(&[a]))
                .catch_unwind()
                .await
                .is_err()
        );
        assert!(!driver.is_available());
        {
            let state = settlement.0.lock().unwrap();
            assert!(!state.active && state.exclusion.is_none());
        }
        let records = persisted_records(storage.path(), id);
        assert_eq!(records.matches("acknowledged").count(), 1);
        assert!(!records.contains("excluded"));
        assert_eq!(
            crate::session::load_in(root.path(), storage.path(), id)
                .unwrap()
                .len(),
            2
        );
        assert!(driver.settle_delivered_input().await.is_err());
        assert_eq!(persisted_records(storage.path(), id), records);
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
                .transcript_observer(settlement.observer(opened.observer));
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

//! Speculation owns only snapshots and a cancellable task, never live transcripts.
use super::{context_window, latest_context_tokens, policy::CompactionConfig};
use agentkit_compaction::{CompactionError, CompactionReason, Compactor, StrategyCompactor};
use agentkit_core::{Item, Part, TurnCancellation};
use futures_util::future::{Either, select};
use std::{
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

type Factory = dyn Fn() -> Result<(StrategyCompactor, u64), String> + Send + Sync;
type Revision = dyn Fn() -> Result<u64, String> + Send + Sync;

pub(super) struct Preparation {
    factory: Box<Factory>,
    revision: Box<Revision>,
    state: Mutex<State>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Checkpoint {
    usage_position: usize,
    used: u64,
    window: u64,
    generation: Option<u64>,
    revision: u64,
}

#[derive(Default)]
struct State {
    // Writers: arm reserves/publishes, take transfers ownership, cancel invalidates.
    // Reservations survive callback failure/unwind. A distinct usage checkpoint,
    // cancellation generation, or selection revision can reserve the second (last)
    // attempt; only successful foreground compaction resets the cycle.
    // All guarded fields are scalars except task, which is always moved out before
    // drop. Task/Preparation destruction and unwinding abort without a held guard.
    attempts: u8,
    checkpoint: Option<Checkpoint>,
    ticket: u64,
    task: Option<Task>,
}

struct Task {
    snapshot: Vec<Item>,
    revision: u64,
    cancellation: Option<TurnCancellation>,
    deadline: tokio::time::Instant,
    // Completion outcome only; not a second ownership registry. The worker is
    // the sole writer and publishes success before returning. Readers use it
    // only after handle.is_finished(); panic/abort leave the default false.
    // No destructor or unwind path acquires the registry lock.
    succeeded: Arc<AtomicBool>,
    handle: JoinHandle<Result<Vec<Item>, CompactionError>>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Preparation {
    pub(super) fn new(
        factory: impl Fn() -> Result<(StrategyCompactor, u64), String> + Send + Sync + 'static,
        revision: impl Fn() -> Result<u64, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Box::new(factory),
            revision: Box::new(revision),
            state: Mutex::new(State::default()),
        }
    }

    pub(super) fn cancel(&self, reset: bool) {
        let task = if let Ok(mut state) = self.state.lock() {
            state.ticket = state.ticket.wrapping_add(1);
            if reset {
                state.attempts = 0;
                state.checkpoint = None;
            }
            state.task.take()
        } else {
            None
        };
        // Task abort/drop and all callbacks run outside the state lock. Poison
        // disables speculation; we never repair or trust a poisoned registry.
        drop(task);
    }

    pub(super) fn arm(
        &self,
        live: &[Item],
        policy: &CompactionConfig,
        cancellation: Option<TurnCancellation>,
    ) {
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(crate::request_budget::RequestBudget::current().seconds());
        let Ok(revision) = (self.revision)() else {
            self.cancel(false);
            return;
        };
        // Provider measurements alone determine eligibility; resolve policy and
        // inspect the immutable transcript before taking the registry lock.
        let Some(used) = latest_context_tokens(live) else {
            return;
        };
        let Some(window) = context_window(live) else {
            return;
        };
        let thresholds = policy.resolve(window);
        let eligible = used >= thresholds.prepare_tokens
            && used < thresholds.apply_tokens
            && !cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled);
        let checkpoint = Checkpoint {
            usage_position: live
                .iter()
                .rposition(|item| item.usage.is_some())
                .unwrap_or(0),
            used,
            window,
            generation: cancellation.as_ref().map(TurnCancellation::generation),
            revision,
        };
        let (ticket, old_task, eligible) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let invalid = state.task.as_ref().is_some_and(|task| {
                task.revision != revision
                    // New provider usage permits one retry of a completed
                    // failure, never replacement of a paid successful summary.
                    || (eligible
                        && state.attempts < 2
                        && task.handle.is_finished()
                        && !task.succeeded.load(Ordering::Acquire)
                        && state.checkpoint != Some(checkpoint))
                    || task
                        .cancellation
                        .as_ref()
                        .is_some_and(TurnCancellation::is_cancelled)
            });
            let old_task = if invalid { state.task.take() } else { None };
            let eligible = eligible
                && state.task.is_none()
                && state.attempts < 2
                && state.checkpoint != Some(checkpoint);
            if invalid || eligible {
                state.ticket = state.ticket.wrapping_add(1);
            }
            if eligible {
                state.attempts += 1;
                state.checkpoint = Some(checkpoint);
            }
            (state.ticket, old_task, eligible)
        };
        drop(old_task);
        if !eligible {
            return;
        }
        let Ok((compactor, frozen_revision)) = (self.factory)() else {
            return;
        };
        if frozen_revision != revision {
            return;
        }
        let snapshot = live.to_vec();
        let input = snapshot.clone();
        let arm_cancellation = cancellation.clone();
        let succeeded = Arc::new(AtomicBool::new(false));
        let completion = succeeded.clone();
        let handle = tokio::spawn(async move {
            let cancelled = async {
                if let Some(cancellation) = &cancellation {
                    cancellation.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let compact = compactor.compact(
                &input,
                CompactionReason::Custom("speculative context preparation".into()),
                cancellation.clone(),
            );
            // Poll cancellation first when both futures are ready.
            let compact = tokio::time::timeout_at(deadline, compact);
            let result = match select(pin!(cancelled), pin!(compact)).await {
                Either::Left(_) => Err(CompactionError::Cancelled),
                Either::Right((result, _)) => result.unwrap_or_else(|_| {
                    Err(CompactionError::Failed(
                        "preparation request budget expired".into(),
                    ))
                }),
            };
            completion.store(
                result.as_ref().is_ok_and(|items| !items.is_empty()),
                Ordering::Release,
            );
            result
        });
        let mut task = Some(Task {
            snapshot,
            revision,
            cancellation: arm_cancellation,
            deadline,
            succeeded,
            handle,
        });
        if let Ok(mut state) = self.state.lock()
            && state.ticket == ticket
            && state.task.is_none()
        {
            state.task = task.take();
        }
        drop(task);
    }

    pub(super) async fn take(
        &self,
        live: &[Item],
        cancellation: Option<TurnCancellation>,
    ) -> Option<Vec<Item>> {
        let task = {
            let mut state = self.state.lock().ok()?;
            state.ticket = state.ticket.wrapping_add(1);
            state.task.take()
        };
        let mut task = task?;
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
            || task.cancellation.as_ref().is_some_and(|c| c.is_cancelled())
            || (self.revision)().ok()? != task.revision
        {
            return None;
        }
        if !live.starts_with(&task.snapshot) {
            return None;
        }
        let cancelled = async {
            if let Some(cancellation) = &cancellation {
                cancellation.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        // The whole preparation shares one request budget measured from arm,
        // including multi-step/subscription pipelines. Taking never extends it.
        // Timeout polls the handle first: an already completed result remains
        // reusable after this computation deadline, which is not a cache TTL.
        // Timeout drops Task (aborting it), allowing foreground fallback.
        let wait = tokio::time::timeout_at(task.deadline, &mut task.handle);
        let compacted = match select(pin!(cancelled), pin!(wait)).await {
            Either::Left(_) => return None,
            Either::Right((result, _)) => result.ok()?.ok()?.ok()?,
        };
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
            || task.cancellation.as_ref().is_some_and(|c| c.is_cancelled())
            || (self.revision)().ok()? != task.revision
        {
            return None;
        }
        merge(&task.snapshot, compacted, live)
    }
}

fn merge(snapshot: &[Item], mut compacted: Vec<Item>, live: &[Item]) -> Option<Vec<Item>> {
    if compacted.is_empty() || !live.starts_with(snapshot) {
        return None;
    }
    for mut item in live[snapshot.len()..].iter().cloned() {
        item.parts
            .retain(|part| !matches!(part, Part::Reasoning(_)));
        item.usage = None;
        if !item.parts.is_empty() {
            compacted.push(item);
        }
    }
    // Pipeline output should already be clean, but never retain stale occupancy.
    for item in &mut compacted {
        item.usage = None;
    }
    Some(compacted)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;
    use agentkit_compaction::{
        CompactionContext, CompactionPipeline, CompactionRequest, CompactionResult,
        CompactionStrategy, DropReasoningStrategy,
    };
    use agentkit_core::{CancellationController, ItemKind, MetadataMap, TokenUsage, Usage};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn response(used: u64) -> Item {
        let mut metadata = MetadataMap::new();
        metadata.insert("context_window".into(), 200_000u64.into());
        metadata.insert("context_used".into(), used.into());
        Item::text(ItemKind::Assistant, "response")
            .with_usage(Usage::new(TokenUsage::new(used, 0)).with_metadata(metadata))
    }

    #[test]
    fn exact_prefix_required_even_when_item_identity_matches() {
        let snapshot = vec![response(145_000)];
        let mut live = snapshot.clone();
        live[0].parts = vec![Part::text("edited in place")];
        assert!(
            merge(
                &snapshot,
                vec![Item::text(ItemKind::Assistant, "summary")],
                &live,
            )
            .is_none()
        );
    }

    #[test]
    fn merges_new_tail_without_stale_usage() {
        let snapshot = vec![response(145_000)];
        let mut live = snapshot.clone();
        live.push(response(160_000));
        live.push(Item::text(ItemKind::User, "new instruction"));
        let result = merge(
            &snapshot,
            vec![Item::text(ItemKind::Assistant, "summary")],
            &live,
        )
        .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[2], live[2]);
        assert!(result.iter().all(|item| item.usage.is_none()));
    }

    #[tokio::test]
    async fn unmeasured_tail_does_not_override_provider_usage() {
        let (preparation, started, dropped) = pending();
        let mut live = vec![response(145_000)];
        live.push(Item::text(ItemKind::User, "x".repeat(200_000)));
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        let result = merge(
            &live[..1],
            vec![Item::text(ItemKind::Assistant, "summary")],
            &live,
        )
        .unwrap();
        assert_eq!(result[1], live[1]);
        preparation.cancel(false);
        observed(&dropped).await;
    }

    struct Pending {
        started: Arc<Notify>,
        dropped: Arc<Notify>,
    }
    struct OnDrop(Arc<Notify>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    #[async_trait]
    impl CompactionStrategy for Pending {
        async fn apply(
            &self,
            _: CompactionRequest,
            _: &mut CompactionContext<'_>,
        ) -> Result<CompactionResult, CompactionError> {
            let _guard = OnDrop(self.dropped.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    fn pending() -> (Preparation, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let (start, drop) = (started.clone(), dropped.clone());
        let preparation = Preparation::new(
            move || {
                Ok((
                    StrategyCompactor::new(
                        |_, _| None,
                        Pending {
                            started: start.clone(),
                            dropped: drop.clone(),
                        },
                    ),
                    0,
                ))
            },
            || Ok(0),
        );
        (preparation, started, dropped)
    }

    async fn observed(notify: &Notify) {
        tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_drops_pending_pipeline() {
        let (preparation, started, dropped) = pending();
        let cancellation = CancellationController::new();
        preparation.arm(
            &[response(145_000)],
            &CompactionConfig::default(),
            Some(cancellation.handle().checkpoint()),
        );
        observed(&started).await;
        cancellation.interrupt();
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn dropping_owner_aborts_pipeline() {
        let (preparation, started, dropped) = pending();
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        observed(&started).await;
        drop(preparation);
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn current_turn_cancellation_interrupts_pending_candidate() {
        let (preparation, started, dropped) = pending();
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        let cancellation = CancellationController::new();
        let checkpoint = cancellation.handle().checkpoint();
        cancellation.interrupt();
        assert!(preparation.take(&live, Some(checkpoint)).await.is_none());
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn manual_cancellation_spends_attempt_until_foreground_success() {
        let (preparation, started, dropped) = pending();
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        observed(&started).await;
        preparation.cancel(false);
        observed(&dropped).await;
        assert_eq!(preparation.state.lock().unwrap().attempts, 1);
        preparation.cancel(true);
        assert_eq!(preparation.state.lock().unwrap().attempts, 0);
    }

    #[tokio::test]
    async fn ready_candidate_is_reused_only_at_same_revision() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let revision = Arc::new(AtomicU64::new(0));
        let current = revision.clone();
        let preparation = Preparation::new(
            || {
                Ok((
                    StrategyCompactor::new(
                        |_, _| None,
                        CompactionPipeline::new().with_strategy(DropReasoningStrategy::new()),
                    ),
                    0,
                ))
            },
            move || Ok(current.load(Ordering::SeqCst)),
        );
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        // Join completion is an explicit task boundary, not a timing assumption.
        loop {
            if preparation
                .state
                .lock()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .handle
                .is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        revision.store(1, Ordering::SeqCst);
        assert!(preparation.take(&live, None).await.is_none());
    }

    struct Gated {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }
    #[async_trait]
    impl CompactionStrategy for Gated {
        async fn apply(
            &self,
            _: CompactionRequest,
            _: &mut CompactionContext<'_>,
        ) -> Result<CompactionResult, CompactionError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(CompactionResult {
                transcript: vec![Item::text(ItemKind::Assistant, "prepared summary")],
                replaced_items: 1,
                metadata: MetadataMap::new(),
            })
        }
    }

    fn gated() -> (Preparation, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (start, gate) = (started.clone(), release.clone());
        let preparation = Preparation::new(
            move || {
                Ok((
                    StrategyCompactor::new(
                        |_, _| None,
                        Gated {
                            started: start.clone(),
                            release: gate.clone(),
                        },
                    ),
                    0,
                ))
            },
            || Ok(0),
        );
        (preparation, started, release)
    }

    #[tokio::test]
    async fn arms_without_waiting_and_reuses_pending_pipeline() {
        let (preparation, started, release) = gated();
        let mut live = vec![response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        live.push(Item::text(ItemKind::User, "appended instruction"));
        let take = preparation.take(&live, None);
        let release_work = async {
            release.notify_one();
        };
        let (result, ()) = tokio::join!(take, release_work);
        let result = result.unwrap();
        assert_eq!(result[0].parts, vec![Part::text("prepared summary")]);
        assert_eq!(result[1], live[1]);
    }

    #[tokio::test]
    async fn ready_result_is_discarded_after_arm_turn_cancelled() {
        let (preparation, started, release) = gated();
        let cancellation = CancellationController::new();
        let live = [response(145_000)];
        preparation.arm(
            &live,
            &CompactionConfig::default(),
            Some(cancellation.handle().checkpoint()),
        );
        observed(&started).await;
        release.notify_one();
        loop {
            if preparation
                .state
                .lock()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .handle
                .is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancellation.interrupt();
        assert!(
            preparation
                .take(&live, Some(cancellation.handle().checkpoint()))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn live_prefix_edit_cancels_pending_candidate() {
        let (preparation, started, dropped) = pending();
        let mut live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        live[0].parts.push(Part::text("edited"));
        assert!(preparation.take(&live, None).await.is_none());
        observed(&dropped).await;
    }

    #[test]
    fn media_and_files_are_preserved_in_prepared_output_and_tail() {
        use agentkit_core::{DataRef, Modality};
        for part in [
            Part::media(
                Modality::Image,
                "image/png",
                DataRef::Uri("https://x/i".into()),
            ),
            Part::file(DataRef::Uri("https://x/f".into())),
            Part::media(
                Modality::Image,
                "image/png",
                DataRef::InlineBytes(vec![1, 2, 3]),
            ),
        ] {
            let snapshot = vec![response(145_000)];
            let item = Item::new(ItemKind::User, vec![part]);
            let mut live = snapshot.clone();
            live.push(item.clone());
            let result = merge(&snapshot, vec![item.clone()], &live).unwrap();
            assert_eq!(result, vec![item.clone(), item]);
        }
    }

    #[test]
    fn large_retained_system_message_does_not_reject_prepared_output() {
        let snapshot = vec![response(145_000)];
        let system = Item::new(ItemKind::System, vec![Part::text("x".repeat(1_000_000))]);
        let prepared = vec![
            system,
            Item::new(ItemKind::Assistant, vec![Part::text("summary")]),
        ];
        assert_eq!(
            merge(&snapshot, prepared.clone(), &snapshot),
            Some(prepared)
        );
    }

    #[test]
    fn appended_reasoning_is_removed_before_installation() {
        let snapshot = vec![response(145_000)];
        let mut live = snapshot.clone();
        live.push(Item::new(
            ItemKind::Assistant,
            vec![Part::reasoning("private"), Part::text("answer")],
        ));
        let result = merge(
            &snapshot,
            vec![Item::text(ItemKind::Assistant, "summary")],
            &live,
        )
        .unwrap();
        assert_eq!(result[1].parts, vec![Part::text("answer")]);
    }

    #[test]
    fn factory_unwind_spends_reservation_without_poisoning_registry() {
        let preparation = Preparation::new(|| panic!("factory failed"), || Ok(0));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        }));
        assert!(outcome.is_err());
        assert!(!preparation.state.is_poisoned());
        let state = preparation.state.lock().unwrap();
        assert_eq!(state.attempts, 1);
        assert!(state.task.is_none());
        drop(state);
        // A second boundary cannot call the panicking factory again this cycle.
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
    }

    #[tokio::test]
    async fn dropping_take_future_aborts_owned_pending_pipeline() {
        let (preparation, started, dropped) = pending();
        let live = [response(145_000)];
        let policy = CompactionConfig::default();
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        let mut take = Box::pin(preparation.take(&live, None));
        assert!(futures_util::poll!(&mut take).is_pending());
        drop(take);
        observed(&dropped).await;
        assert!(preparation.state.lock().unwrap().task.is_none());
    }

    #[tokio::test]
    async fn poisoned_registry_disables_speculation_and_drop_still_aborts() {
        let (preparation, started, dropped) = pending();
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        observed(&started).await;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = preparation.state.lock().unwrap();
            panic!("registry poisoned");
        }));
        assert!(outcome.is_err());
        assert!(preparation.state.is_poisoned());
        preparation.cancel(true);
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        assert!(preparation.take(&[response(145_000)], None).await.is_none());
        drop(preparation);
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn revision_change_while_awaiting_rejects_completed_candidate() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let revision = Arc::new(AtomicU64::new(0));
        let current = revision.clone();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (start, gate) = (started.clone(), release.clone());
        let preparation = Preparation::new(
            move || {
                Ok((
                    StrategyCompactor::new(
                        |_, _| None,
                        Gated {
                            started: start.clone(),
                            release: gate.clone(),
                        },
                    ),
                    0,
                ))
            },
            move || Ok(current.load(Ordering::SeqCst)),
        );
        let live = [response(145_000)];
        let policy = CompactionConfig::default();
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        let mut take = Box::pin(preparation.take(&live, None));
        assert!(futures_util::poll!(&mut take).is_pending());
        revision.store(1, Ordering::SeqCst);
        release.notify_one();
        assert!(take.await.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn preparation_expires_without_a_taker() {
        let (preparation, started, dropped) = pending();
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        observed(&started).await;
        tokio::time::advance(Duration::from_secs(
            crate::request_budget::RequestBudget::current().seconds(),
        ))
        .await;
        observed(&dropped).await;
        assert!(preparation.take(&[response(145_000)], None).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn take_waits_only_for_remaining_arm_budget() {
        let (preparation, started, dropped) = pending();
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        let budget = Duration::from_secs(crate::request_budget::RequestBudget::current().seconds());
        tokio::time::advance(budget - Duration::from_millis(1)).await;
        let mut take = Box::pin(preparation.take(&live, None));
        assert!(futures_util::poll!(&mut take).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(take.await.is_none());
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn cancellation_rearms_once_on_new_checkpoint_not_each_boundary() {
        let (preparation, started, dropped) = pending();
        let policy = CompactionConfig::default();
        let mut live = vec![response(145_000)];
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        preparation.cancel(false);
        observed(&dropped).await;
        preparation.arm(&live, &policy, None);
        assert!(preparation.state.lock().unwrap().task.is_none());
        live.push(response(145_001));
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        preparation.cancel(false);
        observed(&dropped).await;
        live.push(response(145_002));
        preparation.arm(&live, &policy, None);
        assert!(preparation.state.lock().unwrap().task.is_none());
        preparation.cancel(true);
        preparation.arm(&live, &policy, None);
        observed(&started).await;
    }

    #[tokio::test]
    async fn cancelled_generation_rearms_without_new_usage() {
        let (preparation, started, dropped) = pending();
        let controller = CancellationController::new();
        let live = [response(145_000)];
        let policy = CompactionConfig::default();
        preparation.arm(&live, &policy, Some(controller.handle().checkpoint()));
        observed(&started).await;
        controller.interrupt();
        observed(&dropped).await;
        preparation.arm(&live, &policy, Some(controller.handle().checkpoint()));
        observed(&started).await;
        assert_eq!(preparation.state.lock().unwrap().attempts, 2);
    }

    #[test]
    fn factory_failure_rearms_only_on_new_usage_checkpoint() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let preparation = Preparation::new(
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                Err("unavailable".into())
            },
            || Ok(0),
        );
        let policy = CompactionConfig::default();
        let mut live = vec![response(145_000)];
        preparation.arm(&live, &policy, None);
        preparation.arm(&live, &policy, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        live.push(response(145_000));
        preparation.arm(&live, &policy, None);
        live.push(response(145_000));
        preparation.arm(&live, &policy, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn selection_revision_can_rearm_after_failure_without_new_usage() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let revision = Arc::new(AtomicU64::new(0));
        let factory_revision = revision.clone();
        let current = revision.clone();
        let preparation = Preparation::new(
            move || {
                if factory_revision.load(Ordering::SeqCst) == 0 {
                    return Err("old selection unavailable".into());
                }
                Ok((
                    StrategyCompactor::new(|_, _| None, DropReasoningStrategy::new()),
                    1,
                ))
            },
            move || Ok(current.load(Ordering::SeqCst)),
        );
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        revision.store(1, Ordering::SeqCst);
        preparation.arm(&live, &CompactionConfig::default(), None);
        assert!(preparation.take(&live, None).await.is_some());
    }

    #[tokio::test]
    async fn only_provider_prepare_band_arms() {
        let (preparation, _, _) = pending();
        let policy = CompactionConfig::default();
        for live in [
            vec![Item::text(ItemKind::User, "x".repeat(200_000))],
            vec![response(1), Item::text(ItemKind::User, "x".repeat(200_000))],
            vec![response(160_000)],
        ] {
            preparation.arm(&live, &policy, None);
            assert!(preparation.state.lock().unwrap().task.is_none());
        }
        assert_eq!(preparation.state.lock().unwrap().attempts, 0);
    }

    #[tokio::test]
    async fn cancellation_while_taking_aborts_pending_task() {
        let (preparation, started, dropped) = pending();
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        let controller = CancellationController::new();
        let mut take = Box::pin(preparation.take(&live, Some(controller.handle().checkpoint())));
        assert!(futures_util::poll!(&mut take).is_pending());
        controller.interrupt();
        assert!(take.await.is_none());
        observed(&dropped).await;
    }
    #[tokio::test(start_paused = true)]
    async fn completed_summary_remains_reusable_after_preparation_deadline() {
        let (preparation, started, release) = gated();
        let live = [response(145_000)];
        preparation.arm(&live, &CompactionConfig::default(), None);
        observed(&started).await;
        release.notify_one();
        loop {
            if preparation
                .state
                .lock()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .handle
                .is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(
            crate::request_budget::RequestBudget::current().seconds() + 1,
        ))
        .await;
        assert!(preparation.take(&live, None).await.is_some());
    }
    #[tokio::test(start_paused = true)]
    async fn timed_out_task_rearms_once_at_new_usage_checkpoint() {
        let (preparation, started, dropped) = pending();
        let mut live = vec![response(145_000)];
        let policy = CompactionConfig::default();
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        tokio::time::advance(Duration::from_secs(
            crate::request_budget::RequestBudget::current().seconds(),
        ))
        .await;
        observed(&dropped).await;
        // Wait for task completion, not just pipeline-future destruction.
        loop {
            if preparation
                .state
                .lock()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .handle
                .is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        live.push(response(145_001));
        preparation.arm(&live, &policy, None);
        observed(&started).await;
        assert_eq!(preparation.state.lock().unwrap().attempts, 2);
    }
    #[tokio::test]
    async fn successful_preparation_survives_new_usage_on_either_attempt() {
        for cancel_first in [false, true] {
            let (preparation, started, release) = gated();
            let policy = CompactionConfig::default();
            let mut live = vec![response(145_000)];
            if cancel_first {
                preparation.arm(&live, &policy, None);
                observed(&started).await;
                preparation.cancel(false);
                live.push(response(145_001));
            }
            preparation.arm(&live, &policy, None);
            observed(&started).await;
            release.notify_one();
            loop {
                if preparation
                    .state
                    .lock()
                    .unwrap()
                    .task
                    .as_ref()
                    .unwrap()
                    .handle
                    .is_finished()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let snapshot_len = live.len();
            for used in [146_000, 147_000, 160_000] {
                live.push(response(used));
                preparation.arm(&live, &policy, None);
            }
            let result = preparation.take(&live, None).await.unwrap();
            assert_eq!(result[0].parts, vec![Part::text("prepared summary")]);
            assert_eq!(result.len(), 1 + live.len() - snapshot_len);
            for (actual, expected) in result[1..].iter().zip(&live[snapshot_len..]) {
                assert_eq!(actual.parts, expected.parts);
                assert!(actual.usage.is_none());
            }
            assert_eq!(
                preparation.state.lock().unwrap().attempts,
                if cancel_first { 2 } else { 1 }
            );
        }
    }
}

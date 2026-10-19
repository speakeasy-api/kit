//! Speculation owns only snapshots and a cancellable task, never live transcripts.
use super::{context_window, latest_context_tokens, policy::CompactionConfig};
use agentkit_compaction::{CompactionError, CompactionReason, Compactor, StrategyCompactor};
use agentkit_core::{Item, Part, ToolOutput, TurnCancellation};
use futures_util::future::{Either, select};
use std::{pin::pin, sync::Mutex};
use tokio::task::JoinHandle;

type Factory = dyn Fn() -> Result<(StrategyCompactor, u64), String> + Send + Sync;
type Revision = dyn Fn() -> Result<u64, String> + Send + Sync;

pub(super) struct Preparation {
    factory: Box<Factory>,
    revision: Box<Revision>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    // All writers are arm, take, and cancel. One attempt is reserved before
    // invoking external code; failures/panics leave it spent until foreground
    // compaction succeeds. Tickets only coordinate task ownership, never content.
    attempted: bool,
    ticket: u64,
    revision: Option<u64>,
    task: Option<Task>,
}

struct Task {
    snapshot: Vec<Item>,
    revision: u64,
    cancellation: Option<TurnCancellation>,
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
            state.attempted = !reset;
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
        let Ok(revision) = (self.revision)() else {
            self.cancel(false);
            return;
        };
        let (ticket, old_task, eligible) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let changed = state.revision.is_some_and(|previous| previous != revision);
            state.revision = Some(revision);
            if changed {
                // Usage may still describe the previous selection. Do not arm
                // again in this cycle; the normal foreground path is authoritative.
                state.attempted = true;
                state.ticket = state.ticket.wrapping_add(1);
            }
            let old_task = if changed { state.task.take() } else { None };
            let eligible = !state.attempted
                && context_window(live).is_some_and(|window| {
                    let thresholds = policy.resolve(window);
                    occupancy(live).is_some_and(|used| {
                        used >= thresholds.prepare_tokens && used < thresholds.apply_tokens
                    })
                });
            if eligible {
                state.attempted = true;
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
            match select(pin!(cancelled), pin!(compact)).await {
                Either::Left(_) => Err(CompactionError::Cancelled),
                Either::Right((result, _)) => result,
            }
        });
        let mut task = Some(Task {
            snapshot,
            revision,
            cancellation: arm_cancellation,
            handle,
        });
        if let Ok(mut state) = self.state.lock()
            && state.ticket == ticket
            && state.attempted
            && state.task.is_none()
        {
            state.task = task.take();
        }
        drop(task);
    }

    pub(super) async fn take(
        &self,
        live: &[Item],
        policy: &CompactionConfig,
        cancellation: Option<TurnCancellation>,
    ) -> Option<Vec<Item>> {
        let task = {
            let mut state = self.state.lock().ok()?;
            state.ticket = state.ticket.wrapping_add(1);
            state.task.take()
        };
        let mut task = task?;
        if task.cancellation.as_ref().is_some_and(|c| c.is_cancelled())
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
        let compacted = match select(pin!(cancelled), &mut task.handle).await {
            Either::Left(_) => return None,
            Either::Right((result, _)) => result.ok()?.ok()?,
        };
        if task.cancellation.as_ref().is_some_and(|c| c.is_cancelled())
            || (self.revision)().ok()? != task.revision
        {
            return None;
        }
        merge(&task.snapshot, compacted, live, policy)
    }
}

/// Usage covers its request/response, not items appended after that response.
pub(super) fn occupancy(live: &[Item]) -> Option<u64> {
    let position = live.iter().rposition(|item| item.usage.is_some())?;
    let tail = conservative_tokens(&live[position + 1..]);
    // Unknown content costs reject speculation. Foreground triggering can
    // separately retain the provider baseline instead of inventing occupancy.
    if tail == u64::MAX {
        return None;
    }
    Some(latest_context_tokens(live)?.saturating_add(tail))
}

/// One serialized byte per token deliberately overestimates text, metadata and
/// structured content. Unlike a calibrated ratio it cannot hide an uncounted
/// post-usage tool result or assume that a model's tokenizer matches another's.
fn conservative_tokens(items: &[Item]) -> u64 {
    items.iter().fold(0u64, |total, item| {
        if item.parts.iter().any(opaque) {
            return u64::MAX;
        }
        total.saturating_add(
            serde_json::to_vec(item)
                .ok()
                .and_then(|bytes| u64::try_from(bytes.len()).ok())
                .unwrap_or(u64::MAX),
        )
    })
}

// A reference length (or compressed inline byte length) says nothing about the
// provider's image/file token cost. Unknown provider extensions are equally
// unbounded. Reject these candidates rather than guessing at their occupancy.
fn opaque(part: &Part) -> bool {
    match part {
        Part::Media(_) | Part::File(_) | Part::Custom(_) => true,
        Part::Reasoning(reasoning) => reasoning.data.is_some(),
        Part::ToolResult(result) => match &result.output {
            ToolOutput::Parts(parts) => parts.iter().any(opaque),
            ToolOutput::Files(files) => !files.is_empty(),
            ToolOutput::Text(_) | ToolOutput::Structured(_) => false,
        },
        Part::Text(_) | Part::Structured(_) | Part::ToolCall(_) => false,
    }
}

fn merge(
    snapshot: &[Item],
    mut compacted: Vec<Item>,
    live: &[Item],
    policy: &CompactionConfig,
) -> Option<Vec<Item>> {
    if compacted.is_empty() || !live.starts_with(snapshot) {
        return None;
    }
    let threshold = policy.resolve(context_window(live)?).prepare_tokens;
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
    (conservative_tokens(&compacted) < threshold).then_some(compacted)
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
                &CompactionConfig::default()
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
            &CompactionConfig::default(),
        )
        .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[2], live[2]);
        assert!(result.iter().all(|item| item.usage.is_none()));
    }

    #[test]
    fn fit_counts_the_entire_post_usage_tail() {
        let snapshot = vec![response(145_000)];
        let mut live = snapshot.clone();
        live.push(Item::text(ItemKind::User, "x".repeat(200_000)));
        assert!(occupancy(&live).unwrap() > 320_000);
        assert!(
            merge(
                &snapshot,
                vec![Item::text(ItemKind::Assistant, "summary")],
                &live,
                &CompactionConfig::default()
            )
            .is_none()
        );
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
        assert!(
            preparation
                .take(&live, &CompactionConfig::default(), Some(checkpoint))
                .await
                .is_none()
        );
        observed(&dropped).await;
    }

    #[tokio::test]
    async fn manual_cancellation_spends_attempt_until_foreground_success() {
        let (preparation, started, dropped) = pending();
        preparation.arm(&[response(145_000)], &CompactionConfig::default(), None);
        observed(&started).await;
        preparation.cancel(false);
        observed(&dropped).await;
        assert!(preparation.state.lock().unwrap().attempted);
        preparation.cancel(true);
        assert!(!preparation.state.lock().unwrap().attempted);
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
        assert!(
            preparation
                .take(&live, &CompactionConfig::default(), None)
                .await
                .is_none()
        );
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
        let policy = CompactionConfig::default();
        let take = preparation.take(&live, &policy, None);
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
                .take(
                    &live,
                    &CompactionConfig::default(),
                    Some(cancellation.handle().checkpoint())
                )
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
        assert!(
            preparation
                .take(&live, &CompactionConfig::default(), None)
                .await
                .is_none()
        );
        observed(&dropped).await;
    }

    #[test]
    fn short_media_and_file_references_are_not_assumed_to_fit() {
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
            let mut live = snapshot.clone();
            live.push(Item::new(ItemKind::User, vec![part.clone()]));
            assert_eq!(occupancy(&live), None);
            assert!(
                merge(
                    &snapshot,
                    vec![Item::text(ItemKind::Assistant, "summary")],
                    &live,
                    &CompactionConfig::default()
                )
                .is_none()
            );
            assert!(
                merge(
                    &snapshot,
                    vec![Item::new(ItemKind::User, vec![part])],
                    &snapshot,
                    &CompactionConfig::default()
                )
                .is_none()
            );
        }
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
            &CompactionConfig::default(),
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
        assert!(state.attempted);
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
        let mut take = Box::pin(preparation.take(&live, &policy, None));
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
        assert!(
            preparation
                .take(&[response(145_000)], &CompactionConfig::default(), None)
                .await
                .is_none()
        );
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
        let mut take = Box::pin(preparation.take(&live, &policy, None));
        assert!(futures_util::poll!(&mut take).is_pending());
        revision.store(1, Ordering::SeqCst);
        release.notify_one();
        assert!(take.await.is_none());
    }
}

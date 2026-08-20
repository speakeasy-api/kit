//! Automatic, usage-driven transcript compaction.

use agentkit_compaction::{
    CompactionBackend, CompactionContext, CompactionError, CompactionPipeline, CompactionReason,
    CompactionRequest, CompactionResult, CompactionStrategy, Compactor, DropReasoningStrategy,
    StrategyCompactor, SummaryRequest, SummaryResult,
};
use agentkit_core::{FinishReason, Item, ItemKind, MetadataMap, Part, SessionId, Timestamp};
use agentkit_loop::{
    Agent, AgentEvent, LoopCtx, LoopError, LoopInterrupt, LoopMutator, LoopStep, ModelAdapter,
    MutationPoint, SessionConfig, TelemetryConfig, TranscriptCursor,
};
use async_trait::async_trait;
use std::time::Instant;

use crate::{
    events::{self, RuntimeEvent},
    session::SessionObserver,
};

const COMPACTION_PERCENT: u64 = 80;
// The TUI sends this model-invisible control input through ACP. NUL cannot be
// entered in its editor, so an ordinary user prompt cannot collide with it.
const MANUAL_PREFIX: &str = "\0kit:compact\0";

/// Encodes a manual-compaction request as an ACP text prompt. The mutator
/// consumes the marker before provider dispatch and retains only `next`.
pub fn manual_prompt(next: Option<&str>) -> String {
    format!("{MANUAL_PREFIX}{}", next.unwrap_or_default())
}

fn manual_message(item: &Item) -> Option<&str> {
    if item.kind != ItemKind::User || item.parts.len() != 1 {
        return None;
    }
    let Part::Text(text) = &item.parts[0] else {
        return None;
    };
    text.text.strip_prefix(MANUAL_PREFIX)
}

/// Builds the standard Kit compactor.
///
/// The provider-reported window and occupancy on the latest assistant item
/// are authoritative. If model discovery did not report a window, compaction
/// remains disabled rather than guessing at a safe limit.
pub fn automatic<M>(
    adapter: M,
    telemetry: TelemetryConfig,
    persistence: Option<SessionObserver>,
    session_id: impl Into<SessionId>,
) -> Result<AutomaticCompactor, String>
where
    M: ModelAdapter + Clone + 'static,
{
    let backend = KitCompactionBackend {
        adapter,
        telemetry,
        session_id: session_id.into(),
    };
    let inner = StrategyCompactor::new(
        |transcript: &[Item], _point: MutationPoint| {
            transcript
                .last()
                .and_then(manual_message)
                .map(|_| CompactionReason::Manual)
                .or_else(|| compaction_reason(transcript))
        },
        CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(SummarizeForContinuation),
    )
    .with_backend(backend);
    Ok(AutomaticCompactor { inner, persistence })
}

struct KitCompactionBackend<M> {
    adapter: M,
    telemetry: TelemetryConfig,
    session_id: SessionId,
}

#[async_trait]
impl<M> CompactionBackend for KitCompactionBackend<M>
where
    M: ModelAdapter + Clone + 'static,
{
    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: Option<agentkit_core::TurnCancellation>,
    ) -> Result<SummaryResult, CompactionError> {
        if cancellation
            .as_ref()
            .is_some_and(|value| value.is_cancelled())
        {
            return Err(CompactionError::Cancelled);
        }
        // Serializing the complete items, rather than only their text parts, is
        // essential: compose calls and their structured outputs carry most of
        // the durable state in a coding session.
        let rendered = serde_json::to_string_pretty(&request.items)
            .map_err(|error| CompactionError::Failed(error.to_string()))?;
        let mut builder = Agent::builder()
            .model(self.adapter.clone())
            .input(vec![
                Item::text(
                    ItemKind::System,
                    "Compress the supplied transcript into a durable context note for a coding \
                 agent that will not see the original messages. Preserve requirements, exact \
                 paths and symbols, decisions, edits, command results, failures, and unfinished \
                 work. Drop chatter and chain-of-thought. Return only the context note.",
                ),
                Item::text(ItemKind::User, rendered),
            ])
            .telemetry(self.telemetry);
        if let Some(cancellation) = &cancellation {
            builder = builder.cancellation(cancellation.handle().clone());
        }
        let mut driver = builder
            .build()
            .map_err(|error| CompactionError::Failed(error.to_string()))?
            .start(SessionConfig::new(self.session_id.clone()).without_cache())
            .await
            .map_err(|error| CompactionError::Failed(error.to_string()))?;
        let summary = loop {
            match driver.next().await {
                Ok(LoopStep::Finished(result)) => {
                    if result.finish_reason == FinishReason::Cancelled {
                        return Err(CompactionError::Cancelled);
                    }
                    break result
                        .items
                        .iter()
                        .flat_map(|item| &item.parts)
                        .filter_map(|part| match part {
                            Part::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                }
                Ok(LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_))) => continue,
                Ok(LoopStep::Interrupt(_)) => {
                    return Err(CompactionError::Failed(
                        "compaction agent unexpectedly interrupted".into(),
                    ));
                }
                Err(LoopError::Cancelled) => return Err(CompactionError::Cancelled),
                Err(error) => return Err(CompactionError::Failed(error.to_string())),
            }
        };
        if summary.trim().is_empty() {
            return Err(CompactionError::Failed(
                "compaction agent returned an empty summary".into(),
            ));
        }
        let mut metadata = MetadataMap::new();
        metadata.insert("kit.compaction.summary".into(), true.into());
        Ok(SummaryResult::new(vec![
            Item::text(ItemKind::Developer, summary).with_metadata(metadata),
        ]))
    }
}

/// Summarizes every mutable historical item while retaining exact bootstrap
/// instructions and the latest user input. This avoids a fixed item-count tail:
/// one large tool result must still be compactable, and generated summaries must
/// be folded into later summaries rather than accumulating forever.
struct SummarizeForContinuation;

#[async_trait]
impl CompactionStrategy for SummarizeForContinuation {
    async fn apply(
        &self,
        request: CompactionRequest,
        ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let backend = ctx.backend.ok_or_else(|| {
            CompactionError::MissingBackend("automatic compaction requires a backend".into())
        })?;
        let manual = (request.reason == CompactionReason::Manual)
            .then(|| {
                request
                    .transcript
                    .last()
                    .and_then(manual_message)
                    .map(str::to_string)
            })
            .flatten();
        let latest_user = request
            .transcript
            .iter()
            .rposition(|item| item.kind == ItemKind::User);
        let summarized_indices = request
            .transcript
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (!matches!(item.kind, ItemKind::System | ItemKind::Context)
                    && Some(index) != latest_user)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if summarized_indices.is_empty() {
            let mut transcript = request.transcript;
            if let Some(next) = manual {
                let marker = transcript.pop().expect("manual marker is the last item");
                if !next.is_empty() {
                    transcript.push(user_message_from_marker(marker, &next));
                }
                return Ok(CompactionResult::new(transcript, 1));
            }
            return Ok(CompactionResult::new(transcript, 0));
        }
        let first = summarized_indices[0];
        let summarized = summarized_indices
            .iter()
            .map(|index| request.transcript[*index].clone())
            .collect();
        let mut summary = backend
            .summarize(
                SummaryRequest::new(summarized, request.reason),
                ctx.cancellation.clone(),
            )
            .await?;
        let now = Timestamp::now();
        for item in &mut summary.items {
            if item.created_at.is_none() {
                item.created_at = Some(now);
            }
        }
        let summarized = summarized_indices
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let mut replacement = Vec::new();
        for (index, item) in request.transcript.into_iter().enumerate() {
            if index == first {
                replacement.extend(summary.items.clone());
            }
            if !summarized.contains(&index) {
                if Some(index) == latest_user
                    && let Some(next) = &manual
                {
                    if !next.is_empty() {
                        replacement.push(user_message_from_marker(item, next));
                    }
                } else {
                    replacement.push(item);
                }
            }
        }
        Ok(CompactionResult::new(replacement, summarized.len()))
    }
}

fn user_message_from_marker(mut marker: Item, message: &str) -> Item {
    marker.parts = Item::text(ItemKind::User, message).parts;
    marker
}

pub struct AutomaticCompactor {
    inner: StrategyCompactor,
    persistence: Option<SessionObserver>,
}

#[async_trait]
impl LoopMutator for AutomaticCompactor {
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        let Some(reason) = self.inner.should_compact(cursor.as_slice(), ctx.point) else {
            return Ok(());
        };
        ctx.emitter.emit(AgentEvent::MutationStarted {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.cloned(),
            mutator: "automatic-compaction".into(),
            point: ctx.point,
        });
        let reason_label = format!("{reason:?}");
        let started = Instant::now();
        let report_runtime = self.persistence.is_some();
        if report_runtime {
            events::emit(&RuntimeEvent::CompactionStarted {
                reason: reason_label.clone(),
                at: events::now_millis(),
            });
        }
        let finish = |ok, compacted| {
            if report_runtime {
                events::emit(&RuntimeEvent::CompactionFinished {
                    reason: reason_label.clone(),
                    ok,
                    compacted,
                    millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                });
            }
        };

        let before = cursor.len();
        let result = self
            .inner
            .compact(cursor.as_slice(), reason.clone(), ctx.cancellation.clone())
            .await;
        let mut metadata = MetadataMap::new();
        metadata.insert("reason".into(), format!("{reason:?}").into());

        match result {
            Ok(compacted) => {
                if compacted == **cursor {
                    ctx.emitter.emit(AgentEvent::MutationFinished {
                        session_id: ctx.session_id.clone(),
                        turn_id: ctx.turn_id.cloned(),
                        mutator: "automatic-compaction".into(),
                        dirty: false,
                        metadata,
                    });
                    finish(true, false);
                    return Ok(());
                }
                if let Some(persistence) = &self.persistence
                    && let Err(error) = persistence.replace(&compacted)
                {
                    metadata.insert("error".into(), error.clone().into());
                    ctx.emitter.emit(AgentEvent::MutationFinished {
                        session_id: ctx.session_id.clone(),
                        turn_id: ctx.turn_id.cloned(),
                        mutator: "automatic-compaction".into(),
                        dirty: false,
                        metadata,
                    });
                    finish(false, false);
                    return Err(LoopError::Mutator(error));
                }
                metadata.insert(
                    "replaced_items".into(),
                    (before.saturating_sub(compacted.len()) as u64).into(),
                );
                **cursor = compacted;
                ctx.emitter.emit(AgentEvent::MutationFinished {
                    session_id: ctx.session_id.clone(),
                    turn_id: ctx.turn_id.cloned(),
                    mutator: "automatic-compaction".into(),
                    dirty: true,
                    metadata,
                });
                finish(true, true);
                Ok(())
            }
            Err(error) => {
                metadata.insert("error".into(), error.to_string().into());
                ctx.emitter.emit(AgentEvent::MutationFinished {
                    session_id: ctx.session_id.clone(),
                    turn_id: ctx.turn_id.cloned(),
                    mutator: "automatic-compaction".into(),
                    dirty: false,
                    metadata,
                });
                finish(false, false);
                match error {
                    CompactionError::Cancelled => Err(LoopError::Cancelled),
                    other => Err(LoopError::Mutator(other.to_string())),
                }
            }
        }
    }
}

fn compaction_reason(transcript: &[Item]) -> Option<CompactionReason> {
    let usage = transcript
        .iter()
        .rev()
        .find_map(|item| item.usage.as_ref())?;
    let used = usage
        .metadata
        .get("context_used")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.input_tokens.checked_add(tokens.output_tokens))
        })?;
    let window = usage
        .metadata
        .get("context_window")
        .and_then(serde_json::Value::as_u64)?;
    if window == 0 || u128::from(used) * 100 < u128::from(window) * u128::from(COMPACTION_PERCENT) {
        return None;
    }
    Some(CompactionReason::Custom(format!(
        "context_used={used} reached {COMPACTION_PERCENT}% of context_window={window}"
    )))
}

#[cfg(test)]
mod tests {
    use agentkit_core::{TokenUsage, Usage};
    use serde_json::json;

    use super::*;

    fn measured(used: u64, window: Option<u64>) -> Item {
        let mut metadata = MetadataMap::new();
        metadata.insert("context_used".into(), json!(used));
        if let Some(window) = window {
            metadata.insert("context_window".into(), json!(window));
        }
        Item::text(ItemKind::Assistant, "done")
            .with_usage(Usage::new(TokenUsage::new(used, 0)).with_metadata(metadata))
    }

    struct FixedBackend;

    #[async_trait]
    impl CompactionBackend for FixedBackend {
        async fn summarize(
            &self,
            request: SummaryRequest,
            _cancellation: Option<agentkit_core::TurnCancellation>,
        ) -> Result<SummaryResult, CompactionError> {
            assert!(!request.items.is_empty());
            Ok(SummaryResult::new(vec![Item::text(
                ItemKind::Developer,
                "summary",
            )]))
        }
    }

    #[tokio::test]
    async fn strategy_preserves_bootstrap_context_and_latest_user() {
        let transcript = vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::Context, "AGENTS"),
            Item::text(ItemKind::User, "old request"),
            Item::text(ItemKind::Assistant, "old answer"),
            Item::text(ItemKind::User, "current request"),
        ];
        let backend = FixedBackend;
        let mut context = CompactionContext::new().with_backend(&backend);
        let result = SummarizeForContinuation
            .apply(
                CompactionRequest::new(transcript, CompactionReason::TranscriptTooLong),
                &mut context,
            )
            .await
            .unwrap();

        assert_eq!(
            result
                .transcript
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            [
                ItemKind::System,
                ItemKind::Context,
                ItemKind::Developer,
                ItemKind::User,
            ]
        );
        let summary = &result.transcript[2];
        assert!(summary.created_at.is_some());
        let current = result.transcript.last().unwrap();
        let Part::Text(text) = &current.parts[0] else {
            panic!("latest user should remain text");
        };
        assert_eq!(text.text, "current request");
    }

    #[tokio::test]
    async fn manual_compaction_consumes_command_without_starting_a_model_turn() {
        let transcript = vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::User, "old request"),
            Item::text(ItemKind::Assistant, "old answer"),
            Item::text(ItemKind::User, manual_prompt(None)),
        ];
        let backend = FixedBackend;
        let mut context = CompactionContext::new().with_backend(&backend);
        let result = SummarizeForContinuation
            .apply(
                CompactionRequest::new(transcript, CompactionReason::Manual),
                &mut context,
            )
            .await
            .unwrap();

        assert_eq!(
            result
                .transcript
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            [ItemKind::System, ItemKind::Developer]
        );
    }

    #[tokio::test]
    async fn manual_compaction_retains_the_optional_next_user_message() {
        let transcript = vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::User, "old request"),
            Item::text(ItemKind::Assistant, "old answer"),
            Item::text(ItemKind::User, manual_prompt(Some("next request"))),
        ];
        let backend = FixedBackend;
        let mut context = CompactionContext::new().with_backend(&backend);
        let result = SummarizeForContinuation
            .apply(
                CompactionRequest::new(transcript, CompactionReason::Manual),
                &mut context,
            )
            .await
            .unwrap();

        let next = result.transcript.last().unwrap();
        assert_eq!(next.kind, ItemKind::User);
        let Part::Text(text) = &next.parts[0] else {
            panic!("next message should remain text");
        };
        assert_eq!(text.text, "next request");
    }

    #[test]
    fn manual_prompt_triggers_without_usage() {
        assert_eq!(
            manual_message(&Item::text(ItemKind::User, manual_prompt(None))),
            Some("")
        );
    }

    #[test]
    fn triggers_at_eighty_percent() {
        assert!(compaction_reason(&[measured(79, Some(100))]).is_none());
        assert!(compaction_reason(&[measured(80, Some(100))]).is_some());
    }

    #[test]
    fn unknown_window_does_not_guess() {
        assert!(compaction_reason(&[measured(100_000, None)]).is_none());
    }

    #[test]
    fn latest_reported_usage_is_authoritative() {
        assert!(compaction_reason(&[measured(90, Some(100)), measured(10, Some(100)),]).is_none());
    }
}

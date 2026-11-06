use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agenthooksprotocol::generated::Event;
use agenthooksprotocol::inputs::{TurnStartInput, UserMessageInboundInput};
use agenthooksprotocol::{
    EventType, TurnStartInputTrigger, TurnStartInputTurn, UserMessageInboundInputMessage,
    UserMessageInboundInputTurn,
};
use agentkit_core::{ItemKind, Part};
use agentkit_loop::{
    AgentBuilder, AgentEvent, LoopCtx, LoopError, LoopMutator, ModelAdapter, MutationPoint,
    TranscriptCursor, TranscriptEvent, TranscriptObserver,
};
use async_trait::async_trait;

use super::Hooks;
use crate::session::{SessionObserver, TransientTranscript};

const ACP_MESSAGE_ID: &str = "dev.kit.acp.message_id";

/// Runs arrival policy before `turn.start` on the effective admitted prompt.
///
/// Removing a denied prompt leaves no pending input, so the loop finishes the
/// turn without calling the model.
pub(crate) struct PromptHook {
    hooks: Arc<Hooks>,
    channel: &'static str,
    record: PromptRecord,
    batch: PromptBatch,
}

/// Counts the unconsumed appended input suffix, not passive transcript history.
/// The driver serializes callbacks and mutation; no guard spans hook dispatch.
#[derive(Clone, Default)]
struct PromptBatch(Arc<AtomicUsize>);

impl TranscriptObserver for PromptBatch {
    fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
        if matches!(
            event.item.kind,
            ItemKind::User | ItemKind::Notification | ItemKind::Context
        ) {
            self.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.0.store(0, Ordering::Relaxed);
        }
    }
}

/// Installs both halves of prompt admission so the batch boundary is exact.
/// Register before transcript-rewriting mutators, including compaction: admission
/// owns the appended batch before another policy can consume or relocate it.
pub(crate) trait PromptHookBuilder {
    fn prompt_hook(self, hook: PromptHook) -> Self;
}

impl<M: ModelAdapter> PromptHookBuilder for AgentBuilder<M> {
    fn prompt_hook(self, hook: PromptHook) -> Self {
        self.transcript_observer(hook.batch.clone()).mutator(hook)
    }
}

/// Where the transcript a prompt edit or denial rewrites is recorded.
pub(crate) enum PromptRecord {
    Session(SessionObserver),
    Transient(TransientTranscript),
}

impl PromptRecord {
    fn replace(&self, transcript: &[agentkit_core::Item]) -> Result<(), String> {
        match self {
            Self::Session(observer) => observer.replace(transcript),
            Self::Transient(recorded) => {
                recorded.replace(transcript);
                Ok(())
            }
        }
    }
}

impl PromptHook {
    pub(crate) fn new(hooks: Arc<Hooks>, channel: &'static str, record: PromptRecord) -> Self {
        Self {
            hooks,
            channel,
            record,
            batch: PromptBatch::default(),
        }
    }
}

impl PromptHook {
    async fn apply(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: &LoopCtx<'_>,
        event_type: EventType,
        index: Option<usize>,
        start: usize,
        trigger: TurnStartInputTrigger,
    ) -> Result<bool, LoopError> {
        let message = index.map(|index| super::message(&cursor[index], index));
        let original: String = message
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                agenthooksprotocol::inputs::PartInput::Text(body) => editable_text(body),
                _ => None,
            })
            .collect();
        let Some(turn_id) = ctx.turn_id else {
            return Ok(true);
        };
        let result = if event_type == EventType::UserMessageInbound {
            let mut input = UserMessageInboundInput::new(UserMessageInboundInputMessage::new(
                self.channel,
                Vec::new(),
                "user",
            ));
            input.session = Some(self.hooks.session(&ctx.session_id.to_string()));
            input.turn = Some(UserMessageInboundInputTurn::new(turn_id.to_string()));
            super::dispatch(
                self.hooks.sdk.user_message_inbound(
                    input
                        .with_sources()
                        .with_message_messages(message.into_iter().collect()),
                ),
                ctx.cancellation.as_ref(),
            )
            .await
        } else {
            let items = cursor[start..]
                .iter()
                .enumerate()
                .map(|(offset, item)| super::message(item, start + offset))
                .collect();
            let input = TurnStartInput::new(
                Vec::new(),
                trigger,
                TurnStartInputTurn::new(turn_id.to_string()),
            )
            .with_session(self.hooks.session(&ctx.session_id.to_string()));
            super::dispatch(
                self.hooks
                    .sdk
                    .turn_start(input.with_sources().with_items(items)),
                ctx.cancellation.as_ref(),
            )
            .await
        };
        if matches!(&result, Err(LoopError::Cancelled)) {
            return Err(LoopError::Cancelled);
        }
        let edited = match self
            .hooks
            .settle(event_type, result, &ctx.session_id.to_string())
        {
            Some(outcome) if outcome.outcome.is_denied() => Err(super::denial(&outcome.outcome)),
            Some(outcome)
                if event_type == EventType::TurnStart && !super::has_modification(&outcome) =>
            {
                return Ok(true);
            }
            Some(outcome) => match outcome.event {
                Event::TurnStart(event) => {
                    // `prompt` replaces the entire admitted canonical item list,
                    // not just the final user's text. Reuse model-effect validation
                    // to retain authorized opaque parts and reject fabricated media.
                    if index.is_none() {
                        return Ok(true);
                    }
                    let mut replacement = super::model::canonical_items_at(
                        event.0.items.iter().map(|item| item.0.as_ref()),
                        &cursor[start..],
                        start,
                    )?;
                    // Canonical IDs belong to the hook; ACP replay identity belongs
                    // to the host and survives a replacement with a new ID.
                    if let Some(identity) =
                        index.and_then(|index| cursor[index].metadata.get(ACP_MESSAGE_ID))
                        && let Some(user) = replacement
                            .iter_mut()
                            .rev()
                            .find(|item| item.kind == ItemKind::User)
                    {
                        user.metadata
                            .insert(ACP_MESSAGE_ID.into(), identity.clone());
                    }
                    cursor.splice(start.., replacement);
                    self.persist(cursor, ctx);
                    return Ok(true);
                }
                Event::UserMessageInbound(event) => Ok(event
                    .message
                    .messages
                    .iter()
                    .map(|message| message_text(&message.parts))
                    .collect::<Vec<_>>()
                    .join("\n")),
                _ => return Ok(true),
            },
            None => Err("agent hook dispatch failed".into()),
        };
        let Some(index) = index else {
            return Ok(true);
        };
        let accepted = edited.is_ok();
        match edited {
            Ok(text) => {
                if text == original {
                    return Ok(true);
                }
                if let Some(prompt) = cursor.get_mut(index) {
                    prompt.parts.retain(|part| super::part_text(part).is_none());
                    prompt.parts.insert(0, Part::text(text));
                }
            }
            Err(reason) => {
                cursor.truncate(start);
                ctx.emitter.emit(AgentEvent::Warning {
                    message: format!("prompt blocked by agent hook: {reason}"),
                });
            }
        }
        self.persist(cursor, ctx);
        Ok(accepted)
    }

    fn persist(&self, cursor: &[agentkit_core::Item], ctx: &LoopCtx<'_>) {
        if let Err(error) = self.record.replace(cursor) {
            ctx.emitter.emit(AgentEvent::Warning {
                message: format!("could not persist agent hook prompt edit: {error}"),
            });
        }
    }
}

#[async_trait]
impl LoopMutator for PromptHook {
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        if !matches!(
            ctx.point,
            MutationPoint::AfterTurnEnded | MutationPoint::AfterToolResult
        ) {
            return Ok(());
        }
        self.hooks
            .drain_turn_events(&ctx.session_id.to_string())
            .await;
        // Consume the append count before awaiting policy, including cancellation.
        let pending = self.batch.0.swap(0, Ordering::Relaxed);
        let start = cursor
            .len()
            .checked_sub(pending)
            .ok_or_else(|| LoopError::InvalidState("prompt batch exceeds transcript".into()))?;
        let index = cursor.len().checked_sub(1).filter(|&index| {
            index >= start && matches!(cursor[index].kind, ItemKind::User | ItemKind::Notification)
        });
        // Arrival and admission are separate boundaries with shared message identity.
        // A2A and delegated sessions carry agent-originated input; foreground
        // CLI and ACP expose user prompts. Internal notifications are external.
        let trigger = if index.is_none_or(|index| cursor[index].kind != ItemKind::User)
            || self.channel == "a2a"
            || self.hooks.injections.is_child(&ctx.session_id.to_string())
        {
            TurnStartInputTrigger::External
        } else {
            TurnStartInputTrigger::User
        };
        // Notifications are execution input, not transport arrivals. A user
        // interjection after tools is an arrival within the existing turn.
        let arrival = if index.is_some_and(|index| cursor[index].kind == ItemKind::User) {
            self.apply(
                cursor,
                &ctx,
                EventType::UserMessageInbound,
                index,
                start,
                trigger.clone(),
            )
            .await
        } else {
            Ok(true)
        };
        let index = match arrival {
            Ok(true) => index,
            Ok(false) => None,
            Err(LoopError::Cancelled) => {
                if ctx.point == MutationPoint::AfterTurnEnded {
                    // The native turn still ends. Deliver its start without accepting
                    // effects from the cancelled admission or changing the transcript.
                    if let Some(turn_id) = ctx.turn_id {
                        let input = TurnStartInput::new(
                            Vec::new(),
                            trigger,
                            TurnStartInputTurn::new(turn_id.to_string()),
                        )
                        .with_session(self.hooks.session(&ctx.session_id.to_string()));
                        super::settle(
                            EventType::TurnStart,
                            super::dispatch(self.hooks.sdk.turn_start(input.with_sources()), None)
                                .await,
                        );
                    }
                }
                return Err(LoopError::Cancelled);
            }
            Err(error) => return Err(error),
        };
        if ctx.point == MutationPoint::AfterTurnEnded {
            // Even rejected input owns a native turn. Report an empty admitted
            // input rather than inventing a prompt or leaving an unmatched end.
            self.apply(cursor, &ctx, EventType::TurnStart, index, start, trigger)
                .await?;
        }
        Ok(())
    }
}

fn message_text(parts: &[Box<agenthooksprotocol::ContentItem>]) -> String {
    parts
        .iter()
        .filter_map(|part| match part.as_ref() {
            agenthooksprotocol::ContentItem::TextBodyPart(body) => editable_text(body),
            _ => None,
        })
        .collect()
}

// Reference descriptors describe retained non-text input; do not inject their
// URI/base64/filename representation into the model's text when a hook edits it.
fn editable_text(body: &agenthooksprotocol::TextBodyPart) -> Option<&str> {
    // TextBodyPart has no descriptor discriminator and forbids extra wire
    // fields. Host-generated text IDs end in a numeric part index; reserve
    // /data and /name for descriptors, independently of disclosure categories.
    if body.id.ends_with("/data") || body.id.ends_with("/name") {
        return None;
    }
    Some(&body.text)
}

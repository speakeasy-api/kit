use std::sync::Arc;

use agenthooksprotocol::generated::Event;
use agenthooksprotocol::inputs::{TurnStartInput, UserMessageInboundInput};
use agenthooksprotocol::{
    EventType, TurnStartInputTrigger, TurnStartInputTurn, UserMessageInboundInputMessage,
    UserMessageInboundInputTurn,
};
use agentkit_core::{ItemKind, Part};
use agentkit_loop::{AgentEvent, LoopCtx, LoopError, LoopMutator, MutationPoint, TranscriptCursor};
use async_trait::async_trait;

use super::Hooks;
use crate::session::{SessionObserver, TransientTranscript};

const ACP_RESOURCE: &str = "acp.resource.uri";

/// Runs arrival policy before `turn.start` on the effective admitted prompt.
///
/// Removing a denied prompt leaves no pending input, so the loop finishes the
/// turn without calling the model.
pub(crate) struct PromptHook {
    hooks: Arc<Hooks>,
    channel: &'static str,
    record: PromptRecord,
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
        let mut start = index.unwrap_or(cursor.len());
        while start > 0
            && index.is_some()
            && cursor[start - 1].kind == ItemKind::Context
            && cursor[start - 1].metadata.contains_key(ACP_RESOURCE)
        {
            start -= 1;
        }
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
                    let replacement = super::model::canonical_items_at(
                        event.0.items.iter().map(|item| item.0.as_ref()),
                        &cursor[start..],
                        start,
                    )?;
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
                cursor.pop();
                while cursor.last().is_some_and(|item| {
                    item.kind == ItemKind::Context && item.metadata.contains_key(ACP_RESOURCE)
                }) {
                    cursor.pop();
                }
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
        let index = cursor
            .len()
            .checked_sub(1)
            .filter(|&index| matches!(cursor[index].kind, ItemKind::User | ItemKind::Notification));
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
        let index = if index.is_some_and(|index| cursor[index].kind == ItemKind::User)
            && !self
                .apply(
                    cursor,
                    &ctx,
                    EventType::UserMessageInbound,
                    index,
                    trigger.clone(),
                )
                .await?
        {
            None
        } else {
            index
        };
        if ctx.point == MutationPoint::AfterTurnEnded {
            // Even rejected input owns a native turn. Report an empty admitted
            // input rather than inventing a prompt or leaving an unmatched end.
            self.apply(cursor, &ctx, EventType::TurnStart, index, trigger)
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

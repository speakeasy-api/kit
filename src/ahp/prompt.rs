use std::sync::Arc;

use agenthooksprotocol::generated::Event;
use agenthooksprotocol::inputs::UserMessageInboundInput;
use agenthooksprotocol::{EventType, Session, UserMessageInboundInputMessage};
use agentkit_core::{ItemKind, Part};
use agentkit_loop::{AgentEvent, LoopCtx, LoopError, LoopMutator, MutationPoint, TranscriptCursor};
use async_trait::async_trait;

use super::Hooks;
use crate::session::SessionObserver;

/// Runs `user.message.inbound` over the prompt a new turn just admitted.
///
/// Removing a denied prompt leaves no pending input, so the loop finishes the
/// turn without calling the model.
pub(crate) struct PromptHook {
    hooks: Arc<Hooks>,
    channel: &'static str,
    persistence: SessionObserver,
}

impl PromptHook {
    pub(crate) fn new(
        hooks: Arc<Hooks>,
        channel: &'static str,
        persistence: SessionObserver,
    ) -> Self {
        Self {
            hooks,
            channel,
            persistence,
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
        if ctx.point != MutationPoint::AfterTurnEnded {
            return Ok(());
        }
        let Some(prompt) = cursor.last().filter(|item| item.kind == ItemKind::User) else {
            return Ok(());
        };
        let Some(message) = super::message(prompt, cursor.len().saturating_sub(1)) else {
            return Ok(());
        };
        let mut input = UserMessageInboundInput::new(UserMessageInboundInputMessage::new(
            self.channel,
            Vec::new(),
            "user",
        ));
        input.session = Some(Box::new(Session::new(ctx.session_id.to_string())));
        let boundary = self
            .hooks
            .sdk
            .user_message_inbound(input.with_sources().with_message_messages(vec![message]));
        let result = match &ctx.cancellation {
            Some(cancellation) => boundary.cancel_when(cancellation.cancelled()).await,
            None => boundary.await,
        };
        let edited = match super::settle(EventType::UserMessageInbound, result) {
            Some(outcome) if outcome.outcome.is_denied() => Err(super::denial(&outcome.outcome)),
            Some(outcome) => match outcome.settled_event {
                Event::UserMessageInbound(event) => Ok(event
                    .message
                    .messages
                    .iter()
                    .map(|message| super::text(&message.parts))
                    .collect::<Vec<_>>()
                    .join("\n")),
                _ => return Ok(()),
            },
            None => Err("agent hook dispatch failed".into()),
        };
        if ctx
            .cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
        {
            return Err(LoopError::Cancelled);
        }
        match edited {
            Ok(text) => {
                let original = prompt
                    .parts
                    .iter()
                    .filter_map(super::part_text)
                    .collect::<String>();
                if text == original {
                    return Ok(());
                }
                if let Some(prompt) = cursor.last_mut() {
                    prompt.parts.retain(|part| super::part_text(part).is_none());
                    prompt.parts.insert(0, Part::text(text));
                }
            }
            Err(reason) => {
                cursor.pop();
                ctx.emitter.emit(AgentEvent::Warning {
                    message: format!("prompt blocked by agent hook: {reason}"),
                });
            }
        }
        if let Err(error) = self.persistence.replace(cursor) {
            ctx.emitter.emit(AgentEvent::Warning {
                message: format!("could not persist agent hook prompt edit: {error}"),
            });
        }
        Ok(())
    }
}

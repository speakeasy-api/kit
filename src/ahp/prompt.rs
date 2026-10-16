use std::sync::Arc;

use agenthooksprotocol::generated::Event;
use agenthooksprotocol::inputs::UserMessageInboundInput;
use agenthooksprotocol::{EventType, UserMessageInboundInputMessage};
use agentkit_core::{ItemKind, Part};
use agentkit_loop::{AgentEvent, LoopCtx, LoopError, LoopMutator, MutationPoint, TranscriptCursor};
use async_trait::async_trait;

use super::Hooks;
use crate::session::{SessionObserver, TransientTranscript};

const ACP_RESOURCE: &str = "acp.resource.uri";

/// Runs `user.message.inbound` over the prompt a new turn just admitted.
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
        self.hooks
            .drain_turn_events(&ctx.session_id.to_string())
            .await;
        let Some(prompt) = cursor.last().filter(|item| item.kind == ItemKind::User) else {
            return Ok(());
        };
        let index = cursor.len().saturating_sub(1);
        let message = super::message(prompt, index);
        let original: String = message
            .parts
            .iter()
            .filter_map(|part| match part {
                agenthooksprotocol::inputs::PartInput::Text(body) => editable_text(body),
                _ => None,
            })
            .collect();
        let mut input = UserMessageInboundInput::new(UserMessageInboundInputMessage::new(
            self.channel,
            Vec::new(),
            "user",
        ));
        input.session = Some(self.hooks.session(&ctx.session_id.to_string()));
        input.turn = ctx
            .turn_id
            .map(|id| agenthooksprotocol::UserMessageInboundInputTurn::new(id.to_string()));
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
            Some(outcome) => match outcome.event {
                Event::UserMessageInbound(event) => Ok(event
                    .message
                    .messages
                    .iter()
                    .map(|message| {
                        message
                            .parts
                            .iter()
                            .filter_map(|part| match part.as_ref() {
                                agenthooksprotocol::ContentItem::TextBodyPart(body) => {
                                    editable_text(body)
                                }
                                _ => None,
                            })
                            .collect::<String>()
                    })
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
        if let Err(error) = self.record.replace(cursor) {
            ctx.emitter.emit(AgentEvent::Warning {
                message: format!("could not persist agent hook prompt edit: {error}"),
            });
        }
        Ok(())
    }
}

// Reference descriptors describe retained non-text input; do not inject their
// URI/base64/filename representation into the model's text when a hook edits it.
fn editable_text(body: &agenthooksprotocol::TextBodyPart) -> Option<&str> {
    if let agenthooksprotocol::Presence::Present(category) = &body.category
        && (category.ends_with("-reference") || category == "file-name")
    {
        return None;
    }
    Some(&body.text)
}

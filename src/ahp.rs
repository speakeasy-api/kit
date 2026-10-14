//! Agent Hooks Protocol integration at Kit's native lifecycle seams.
mod model;
mod prompt;
mod tool;
mod turn;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use agenthooksprotocol::capability::{self, HostCapabilities, Modify};
use agenthooksprotocol::client::{
    DeliveryDiagnostic, HookError, ProtocolOutcome, SettledCandidate,
};
use agenthooksprotocol::generated::{Effect, Event, Presence};
use agenthooksprotocol::hooks::{EventOutcome, HooksOptions};
use agenthooksprotocol::inputs::{
    ContextCompactAfterInput, ContextCompactBeforeInput, MessageInput, PartInput, SessionEndInput,
    SessionStartInput,
};
use agenthooksprotocol::{
    Attachment, CanonicalMessageRole, ContentItem, ContextCompactAfterInputRemovedItem,
    ContextCompactBeforeInputTrigger, EventType, ExecutionEventExecution,
    ExecutionEventExecutionExecuted, Harness, Registration, Session, SessionEndInputOutcome,
    SessionStartInputTrigger, TextBodyPart, TextPart, TextParts,
};
use agentkit_core::{FinishReason, Item, ItemKind, Part, ToolOutput, TurnCancellation};
use agentkit_loop::{AgentEvent, LoopError};

pub(crate) use model::{HookedAdapter, HookedSession};
pub(crate) use prompt::{PromptHook, PromptRecord};
pub(crate) use tool::hooked_tools;
pub(crate) use turn::TurnHook;

const SOURCE: &str = "urn:dev:kit:harness";
const CONFIG_FILE: &str = "hooks.json";

/// Registered hook backends with the event grants Kit implements.
pub struct Hooks {
    sdk: agenthooksprotocol::Hooks,
    turn_events: Mutex<HashMap<String, VecDeque<AgentEvent>>>,
}

impl Hooks {
    /// Loads `~/.agents/hooks.json`, then `<root>/.agents/hooks.json`. Project
    /// hooks replace global hooks with the same id. Invalid files are skipped.
    pub(crate) fn load(root: &Path) -> Option<Arc<Self>> {
        let mut merged: Option<Registration> = None;
        for directory in crate::runtime::agents_directories(root).iter().rev() {
            let path = directory.join(CONFIG_FILE);
            match read(&path) {
                Ok(Some(registration)) => merge(&mut merged, registration),
                Ok(None) => {}
                Err(error) => eprintln!("kit: ignoring {}: {error}", path.display()),
            }
        }
        let registration = merged?;
        match options().and_then(|options| Self::new(registration, options)) {
            Ok(hooks) => Some(Arc::new(hooks)),
            Err(error) => {
                eprintln!("kit: agent hooks disabled: {error}");
                None
            }
        }
    }

    fn new(registration: Registration, options: HooksOptions) -> Result<Self, String> {
        let sdk = agenthooksprotocol::Hooks::new(registration, options)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            sdk,
            turn_events: Mutex::default(),
        })
    }

    pub(crate) async fn session_start(&self, session: &str, trigger: SessionStartInputTrigger) {
        let input = SessionStartInput::new(
            Vec::new(),
            "allow-all".into(),
            Box::new(Session::new(session)),
            trigger,
        );
        settle(EventType::SessionStart, self.sdk.session_start(input).await);
    }

    pub(crate) async fn session_end(&self, session: &str, outcome: SessionEndInputOutcome) {
        self.drain_turn_events(session).await;
        let reason = outcome.as_str().to_owned();
        let input = SessionEndInput::new(outcome, reason, Box::new(Session::new(session)));
        settle(EventType::SessionEnd, self.sdk.session_end(input).await);
    }

    /// Runs `context.compact.before`; a hook may supply the summary or replace
    /// the summarizer instructions.
    pub(crate) async fn compact_before(
        &self,
        items: &[Item],
        manual: bool,
        instructions: &str,
        cancellation: Option<&TurnCancellation>,
    ) -> Option<Compaction> {
        let trigger = if manual {
            ContextCompactBeforeInputTrigger::Manual
        } else {
            ContextCompactBeforeInputTrigger::Auto
        };
        let mut input = ContextCompactBeforeInput::new(Vec::new(), trigger);
        input.instructions = Some(Box::new(text_parts("instructions", instructions)));
        let boundary = self
            .sdk
            .context_compact_before(input.with_sources().with_items(messages(items)));
        let result = match cancellation {
            Some(cancellation) => boundary.cancel_when(cancellation.cancelled()).await,
            None => boundary.await,
        };
        let outcome = settle(EventType::ContextCompactBefore, result)?;
        if let Some(SettledCandidate::Summary(summary)) = outcome.outcome.candidate() {
            return Some(Compaction::Summary(parts_text(summary)));
        }
        match outcome.event {
            Event::ContextCompactBefore(event) => match &event.0.instructions {
                Presence::Present(edited) if parts_text(edited) != instructions => {
                    Some(Compaction::Instructions(parts_text(edited)))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Runs `context.compact.after`; a hook may replace the summary before it
    /// is installed.
    pub(crate) async fn compact_after(&self, items: &[Item], summary: String) -> String {
        let removed = items
            .iter()
            .enumerate()
            .map(|(index, item)| ContextCompactAfterInputRemovedItem::new(item_id(item, index)))
            .collect();
        let input = ContextCompactAfterInput::new(
            Box::new(ExecutionEventExecution::Executed(
                ExecutionEventExecutionExecuted::new(),
            )),
            removed,
            Box::new(text_parts("summary", &summary)),
        );
        match settle(
            EventType::ContextCompactAfter,
            self.sdk.context_compact_after(input).await,
        )
        .map(|outcome| outcome.event)
        {
            Some(Event::ContextCompactAfter(event)) => parts_text(&event.0.summary),
            _ => summary,
        }
    }
}

pub(crate) fn session_outcome<T>(result: &Result<T, LoopError>) -> SessionEndInputOutcome {
    match result {
        Ok(_) => SessionEndInputOutcome::Completed,
        Err(LoopError::Cancelled) => SessionEndInputOutcome::Cancelled,
        Err(_) => SessionEndInputOutcome::Error,
    }
}

pub(crate) enum Compaction {
    Summary(String),
    Instructions(String),
}

fn options() -> Result<HooksOptions, String> {
    let capabilities = HostCapabilities {
        session_start: Some(capability::session_start().observe()),
        session_end: Some(capability::session_end().observe()),
        turn_start: Some(capability::turn_start().observe()),
        turn_end: Some(capability::turn_end().observe()),
        user_message_inbound: Some(
            capability::user_message_inbound()
                .deny()
                .modify_prompt(Modify::Replace),
        ),
        model_request_before: Some(capability::model_request_before().deny()),
        model_response_after: Some(capability::model_response_after().observe()),
        model_error: Some(capability::model_error().observe()),
        tool_before: Some(
            capability::tool_before()
                .allow()
                .deny()
                .return_result()
                .modify_input(Modify::ReplaceAndMerge),
        ),
        tool_after: Some(capability::tool_after().observe()),
        context_compact_before: Some(
            capability::context_compact_before()
                .return_result()
                .modify_instructions(Modify::Replace),
        ),
        context_compact_after: Some(
            capability::context_compact_after().modify_summary(Modify::Replace),
        ),
        ..HostCapabilities::default()
    };
    let mut options = HooksOptions::new(
        SOURCE,
        Harness::new("kit", env!("CARGO_PKG_VERSION")),
        capabilities,
    );
    options.backend.inherit_environment = true;
    Ok(options)
}

fn read(path: &Path) -> Result<Option<Registration>, String> {
    match crate::config_files::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn merge(merged: &mut Option<Registration>, next: Registration) {
    let Some(registration) = merged else {
        *merged = Some(next);
        return;
    };
    for backend in next.hooks {
        match registration
            .hooks
            .iter_mut()
            .find(|existing| existing.id == backend.id)
        {
            Some(existing) => *existing = backend,
            None => registration.hooks.push(backend),
        }
    }
    registration.protocol_version = next.protocol_version;
}

fn report(event: EventType, diagnostics: &[DeliveryDiagnostic]) {
    for diagnostic in diagnostics {
        tracing::warn!(
            event = event.as_str(),
            ?diagnostic,
            "agent hook delivery failed"
        );
    }
}

/// Reports delivery diagnostics; a dispatch error is reported and yields `None`.
fn settle(event: EventType, result: Result<EventOutcome, HookError>) -> Option<EventOutcome> {
    match result {
        Ok(outcome) => {
            report(event, &outcome.diagnostics);
            Some(outcome)
        }
        Err(error) => {
            tracing::warn!(event = event.as_str(), %error, "agent hook dispatch failed");
            None
        }
    }
}

fn denial<C, P>(outcome: &ProtocolOutcome<C, P>) -> String {
    outcome
        .responses
        .iter()
        .filter_map(|response| match &response.result.effects {
            Presence::Present(effects) => Some(effects),
            Presence::Missing => None,
        })
        .flatten()
        .find_map(|effect| match effect.as_ref() {
            Effect::Deny(deny) => Some(deny.reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "denied by agent hook".into())
}

fn messages(items: &[Item]) -> Vec<MessageInput<Attachment>> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| message(item, index))
        .collect()
}

fn item_id(item: &Item, index: usize) -> String {
    item.id
        .as_ref()
        .map_or_else(|| format!("item-{index}"), ToString::to_string)
}

fn message(item: &Item, index: usize) -> Option<MessageInput<Attachment>> {
    let id = item_id(item, index);
    let parts: Vec<_> = item
        .parts
        .iter()
        .filter_map(part_text)
        .enumerate()
        .map(|(index, text)| text_part(&format!("{id}/{index}"), text))
        .collect();
    (!parts.is_empty()).then(|| MessageInput::new(id, role(item.kind), parts))
}

fn text_part(id: &str, text: impl Into<String>) -> PartInput<Attachment> {
    PartInput::text(TextBodyPart::new(id, text))
}

fn role(kind: ItemKind) -> CanonicalMessageRole {
    match kind {
        ItemKind::System => CanonicalMessageRole::System,
        ItemKind::Developer | ItemKind::Context => CanonicalMessageRole::Developer,
        ItemKind::User | ItemKind::Notification => CanonicalMessageRole::User,
        ItemKind::Assistant => CanonicalMessageRole::Assistant,
        ItemKind::Tool => CanonicalMessageRole::Tool,
    }
}

fn part_text(part: &Part) -> Option<String> {
    match part {
        Part::Text(text) => Some(text.text.clone()),
        Part::Structured(structured) => Some(structured.value.to_string()),
        Part::ToolCall(call) => Some(
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("tool".to_owned(), call.name.clone().into()),
                ("input".to_owned(), call.input.clone()),
            ]))
            .to_string(),
        ),
        Part::ToolResult(result) => output_text(&result.output),
        Part::Media(_) | Part::File(_) | Part::Reasoning(_) | Part::Custom(_) => None,
    }
}

fn output_text(output: &ToolOutput) -> Option<String> {
    match output {
        ToolOutput::Text(text) => Some(text.clone()),
        ToolOutput::Structured(value) => Some(value.to_string()),
        ToolOutput::Parts(parts) => {
            let text: Vec<_> = parts.iter().filter_map(part_text).collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        ToolOutput::Files(_) => None,
    }
}

fn finish_reason(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Completed => "stop",
        FinishReason::ToolCall => "tool_calls",
        FinishReason::MaxTokens => "length",
        FinishReason::Cancelled => "cancelled",
        FinishReason::Blocked => "content_filter",
        FinishReason::Error => "error",
        FinishReason::Other(other) => other,
    }
    .to_owned()
}

fn text(parts: &[Box<ContentItem>]) -> String {
    parts
        .iter()
        .filter_map(|part| match part.as_ref() {
            ContentItem::TextBodyPart(part) => Some(part.text.as_str()),
            _ => None,
        })
        .collect()
}

fn text_parts(id: &str, text: &str) -> TextParts {
    TextParts::new(vec![Box::new(TextPart::from(TextBodyPart::new(
        format!("{id}/0"),
        text,
    )))])
}

fn parts_text(parts: &TextParts) -> String {
    parts
        .0
        .iter()
        .filter_map(|part| match part.as_ref() {
            TextPart::TextBodyPart(part) => Some(part.text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
pub(crate) mod test_support;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;

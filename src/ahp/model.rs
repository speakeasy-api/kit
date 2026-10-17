use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use agenthooksprotocol::client::SettledCandidate;
use agenthooksprotocol::generated::Event;
use agenthooksprotocol::inputs::{
    ModelErrorInput, ModelRequestBeforeInput, ModelResponseAfterInput,
};
use agenthooksprotocol::{CanonicalMessage, CanonicalMessageRole, ContentItem, Presence};
use agenthooksprotocol::{
    EventType, ExecutionEventAttempt, ExecutionEventError, ExecutionEventExecution,
    ExecutionEventExecutionExecuted, ExecutionEventModel, Integer, ModelErrorInputExecution,
    ModelErrorInputExecutionExecuted, ModelRequestBeforeInputParams, SessionStartInputTrigger,
};
use agentkit_core::{FinishReason, Item, ItemKind, Part, TurnCancellation};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    RetryObserver, SessionConfig, TurnRequest,
};
use async_trait::async_trait;

use super::Hooks;

/// Model adapter that reports session start and model calls to agent hooks.
#[derive(Clone)]
pub(crate) struct HookedAdapter<A> {
    inner: A,
    hooks: Option<Arc<Hooks>>,
    trigger: SessionStartInputTrigger,
}

impl<A> HookedAdapter<A> {
    pub(crate) fn new(
        inner: A,
        hooks: Option<Arc<Hooks>>,
        trigger: SessionStartInputTrigger,
    ) -> Self {
        Self {
            inner,
            hooks,
            trigger,
        }
    }
}

#[async_trait]
impl<A: ModelAdapter> ModelAdapter for HookedAdapter<A> {
    type Session = HookedSession<A::Session>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        let session_id = config.session_id.to_string();
        let inner = self.inner.start_session(config).await?;
        let injection_session = if let Some(hooks) = &self.hooks {
            Some(
                hooks
                    .session_start(&session_id, self.trigger.clone(), inner.model_name())
                    .await
                    .map_err(LoopError::InvalidState)?,
            )
        } else {
            None
        };
        Ok(HookedSession {
            inner,
            hooks: self.hooks.clone(),
            _injection_session: injection_session,
            attempts: 0,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub(crate) struct HookedSession<S> {
    inner: S,
    _injection_session: Option<super::injection::SessionLease>,
    hooks: Option<Arc<Hooks>>,
    attempts: u64,
}

#[derive(Clone)]
struct Call {
    hooks: Arc<Hooks>,
    attempt: ExecutionEventAttempt,
    model: ExecutionEventModel,
    session: Box<agenthooksprotocol::Session>,
    turn: String,
    started: std::time::Instant,
    missing_model_fields: Vec<&'static str>,
}

impl Call {
    async fn failed(&self, error: &LoopError, usage: Option<&agentkit_core::Usage>) {
        if matches!(error, LoopError::Cancelled) {
            return;
        }
        let class = match error {
            LoopError::ProviderFailure(_) | LoopError::Provider(_) => "provider",
            _ => "adapter",
        };
        let mut input = ModelErrorInput::new(
            Box::new(self.attempt.clone()),
            Box::new(ExecutionEventError::new(class, error.to_string())),
            ModelErrorInputExecution::Executed(ModelErrorInputExecutionExecuted::new()),
            Box::new(self.model.clone()),
        );
        if !self.missing_model_fields.is_empty() {
            input.gaps = Some(
                self.missing_model_fields
                    .iter()
                    .map(|path| {
                        agenthooksprotocol::ModelErrorInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect(),
            );
        }
        input.session = Some(self.session.clone());
        input.turn = Some(agenthooksprotocol::ModelErrorInputTurn::new(&self.turn));
        if let LoopError::ProviderFailure(failure) = error {
            // ProviderFailure is explicitly sanitized and payload-free in agentkit:
            // retain real transport-attempt accounting, not our synthesized index.
            let mut extensions = agenthooksprotocol::Extensions::new();
            if let Ok(failure) = serde_json::to_value(failure) {
                extensions
                    .additional_properties
                    .insert("dev.kit.retry".into(), failure);
            }
            input.extensions = Some(Box::new(extensions));
            if let Some(status) = failure.upstream.http_status
                && let Some(status) = Integer::from_i64(i64::from(status))
            {
                input.error.status = agenthooksprotocol::Presence::Present(
                    agenthooksprotocol::ExecutionEventErrorStatus::Integer(status),
                );
            }
            if failure.upstream.code != agentkit_loop::UpstreamErrorKind::Unknown
                && let Ok(serde_json::Value::String(code)) =
                    serde_json::to_value(failure.upstream.code)
            {
                input.error.code = agenthooksprotocol::Presence::Present(code);
            }
        }
        input.usage = usage.map(super::usage::attempt).map(Box::new);
        input.latency_ms =
            serde_json::Number::from_f64(self.started.elapsed().as_secs_f64() * 1000.0);
        self.hooks.settle(
            EventType::ModelError,
            super::dispatch(self.hooks.sdk.model_error(input), None).await,
            &self.session.id,
        );
    }
}

#[async_trait]
impl<S: ModelSession> ModelSession for HookedSession<S> {
    type Turn = HookedTurn<S::Turn>;

    fn set_retry_observer(&mut self, observer: Option<Arc<dyn RetryObserver>>) {
        self.inner.set_retry_observer(observer);
    }

    async fn begin_turn(
        &mut self,
        mut request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let Some(hooks) = &self.hooks else {
            let inner = self.inner.begin_turn(request, cancellation).await?;
            return Ok(HookedTurn::live(inner, None));
        };
        hooks
            .drain_turn_events(&request.session_id.to_string())
            .await;
        self.attempts += 1;
        let Some(number) = Integer::new(self.attempts.into()) else {
            let inner = self.inner.begin_turn(request, cancellation).await?;
            return Ok(HookedTurn::live(inner, None));
        };
        let mut call = Call {
            hooks: Arc::clone(hooks),
            session: hooks.session(&request.session_id.to_string()),
            turn: request.turn_id.to_string(),
            started: std::time::Instant::now(),
            missing_model_fields: [
                ("model.id", self.inner.model_name()),
                ("model.provider", self.inner.provider_name()),
            ]
            .into_iter()
            .filter_map(|(field, value)| value.is_none().then_some(field))
            .collect(),
            attempt: ExecutionEventAttempt::new(
                format!("{}/{}", request.turn_id, self.attempts),
                number,
            )
            .with_synthesized(true),
            model: ExecutionEventModel::new(
                self.inner.model_name().unwrap_or("unknown"),
                self.inner.provider_name().unwrap_or("unknown"),
            ),
        };
        let mut input = ModelRequestBeforeInput::new(
            Box::new(call.attempt.clone()),
            Vec::new(),
            Box::new(call.model.clone()),
            ModelRequestBeforeInputParams::new(),
        );
        // Host request settings, not an invented provider wire request. Do not
        // forward arbitrary metadata (which can include authentication bindings).
        let tools = request
            .available_tools
            .iter()
            .map(|tool| {
                super::object([
                    ("name", tool.name.to_string().into()),
                    ("description", tool.description.clone().into()),
                    ("inputSchema", tool.input_schema.clone()),
                ])
            })
            .collect();
        input
            .params
            .additional_properties
            .insert("dev.kit.tools".into(), serde_json::Value::Array(tools));
        if let Some(cache) = &request.cache
            && let (Ok(mode), Ok(retention), Ok(strategy)) = (
                serde_json::to_value(cache.mode),
                serde_json::to_value(cache.retention),
                serde_json::to_value(&cache.strategy),
            )
        {
            input.params.additional_properties.insert(
                "dev.kit.cache".into(),
                super::object([
                    ("mode", mode),
                    ("retention", retention),
                    ("strategy", strategy),
                ]),
            );
        }
        if !call.missing_model_fields.is_empty() {
            input.gaps = Some(
                call.missing_model_fields
                    .iter()
                    .map(|path| {
                        agenthooksprotocol::ModelRequestBeforeInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect(),
            );
        }
        input.session = Some(call.session.clone());
        input.turn = Some(agenthooksprotocol::ModelRequestBeforeInputTurn::new(
            &call.turn,
        ));
        let input = input
            .with_sources()
            .with_items(super::messages(&request.transcript));
        let producer = hooks
            .injections
            .producer(&call.session.id)
            .ok_or_else(|| invalid("injection session is closed"))?;
        let boundary = hooks.sdk.model_request_before(input);
        let result = super::dispatch(boundary, cancellation.as_ref()).await?;
        let outcome = call
            .hooks
            .settle(EventType::ModelRequestBefore, Ok(result), &call.session.id)
            .ok_or_else(|| invalid("model request hook dispatch failed"))?;
        if outcome.outcome.is_denied() {
            return Err(invalid(&format!(
                "model request denied by agent hook: {}",
                super::denial(&outcome.outcome)
            )));
        }
        if outcome.outcome.stopped {
            return Err(invalid("model request stopped by agent hook"));
        }
        producer
            .enqueue(&outcome.outcome)
            .map_err(LoopError::InvalidState)?;
        if let Some(SettledCandidate::Messages(messages)) = outcome.outcome.candidate() {
            let output_items = canonical_items(messages.0.iter().map(Box::as_ref), &[])?;
            return Ok(HookedTurn {
                source: TurnSource::Supplied(Some(Box::new(ModelTurnResult {
                    finish_reason: FinishReason::Completed,
                    output_items,
                    usage: None,
                    metadata: Default::default(),
                    model: None,
                    response_id: None,
                }))),
                call: Some(call),
                latest_usage: None,
                approved_events: VecDeque::new(),
            });
        }
        if super::has_modification(&outcome)
            && let Event::ModelRequestBefore(event) = &outcome.event
        {
            request.transcript = canonical_items(
                event.0.items.iter().map(|item| item.0.as_ref()),
                &request.transcript,
            )?;
        }
        // The delivery reservation rolls back if begin_turn fails, panics, or
        // its future is cancelled. No queue guard crosses this await.
        let delivery = hooks
            .injections
            .claim(&call.session.id)
            .map_err(LoopError::InvalidState)?;
        delivery.append_to(&mut request.transcript);
        call.started = std::time::Instant::now();
        match self.inner.begin_turn(request, cancellation).await {
            Ok(inner) => {
                delivery.commit().map_err(LoopError::InvalidState)?;
                Ok(HookedTurn::live(inner, Some(call)))
            }
            Err(error) => {
                call.failed(&error, None).await;
                Err(error)
            }
        }
    }

    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

enum TurnSource<T> {
    Live(T),
    Supplied(Option<Box<ModelTurnResult>>),
    Done,
}

pub(crate) struct HookedTurn<T> {
    source: TurnSource<T>,
    call: Option<Call>,
    latest_usage: Option<agentkit_core::Usage>,
    approved_events: VecDeque<ModelTurnEvent>,
}

impl<T> HookedTurn<T> {
    fn live(inner: T, call: Option<Call>) -> Self {
        Self {
            source: TurnSource::Live(inner),
            call,
            latest_usage: None,
            approved_events: VecDeque::new(),
        }
    }
}

#[async_trait]
impl<T: ModelTurn> ModelTurn for HookedTurn<T> {
    fn on_cancelled(&mut self) {
        self.approved_events.clear();
        if let TurnSource::Live(mut inner) = std::mem::replace(&mut self.source, TurnSource::Done) {
            inner.on_cancelled();
        }
    }

    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            self.on_cancelled();
            return Err(LoopError::Cancelled);
        }
        if let Some(event) = self.approved_events.pop_front() {
            return Ok(Some(event));
        }
        // Finished owns complete output. Withhold previews until policy settles:
        // edited or blocked text and tool calls must never reach observers.
        let supplied = matches!(self.source, TurnSource::Supplied(_));
        let mut event = loop {
            let event = match &mut self.source {
                TurnSource::Done => return Ok(None),
                TurnSource::Live(inner) => inner.next_event(cancellation.clone()).await,
                TurnSource::Supplied(result) => Ok(result
                    .take()
                    .map(|result| ModelTurnEvent::Finished(*result))),
            };
            if self
                .call
                .as_ref()
                .is_some_and(|call| call.hooks.intercepts_model_response)
                && matches!(
                    event,
                    Ok(Some(ModelTurnEvent::Delta(_) | ModelTurnEvent::ToolCall(_)))
                )
            {
                continue;
            }
            break event;
        };
        let Some(call) = &self.call else {
            return event;
        };
        match &mut event {
            Ok(Some(ModelTurnEvent::Usage(usage))) => self.latest_usage = Some(usage.clone()),
            Ok(Some(ModelTurnEvent::ResponseAttemptSuperseded)) => self.latest_usage = None,
            Ok(Some(ModelTurnEvent::Finished(result))) => {
                let reported =
                    super::usage::reported(result.usage.as_ref(), self.latest_usage.as_ref());
                let mut input = ModelResponseAfterInput::new(
                    Box::new(call.attempt.clone()),
                    Box::new(if supplied {
                        ExecutionEventExecution::SkippedSuppliedResult(
                            agenthooksprotocol::ExecutionEventExecutionSkippedSuppliedResult::new(),
                        )
                    } else {
                        ExecutionEventExecution::Executed(ExecutionEventExecutionExecuted::new())
                    }),
                    super::finish_reason(&result.finish_reason),
                    Vec::new(),
                    Box::new(call.model.clone()),
                );
                let gaps: Vec<_> = call
                    .missing_model_fields
                    .iter()
                    .filter(|path| **path != "model.id" || result.model.is_none())
                    .map(|path| {
                        agenthooksprotocol::ModelResponseAfterInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect();
                if !gaps.is_empty() {
                    input.gaps = Some(gaps);
                }
                input.session = Some(call.session.clone());
                input.turn = Some(agenthooksprotocol::ModelResponseAfterInputTurn::new(
                    &call.turn,
                ));
                // SDK 0.8 model.response.after forbids usage and latencyMs
                // whenever execution is skipped, including supplied results.
                if !supplied {
                    input.usage = reported.as_ref().map(super::usage::attempt).map(Box::new);
                    input.latency_ms =
                        serde_json::Number::from_f64(call.started.elapsed().as_secs_f64() * 1000.0);
                }
                let mut extensions = agenthooksprotocol::Extensions::new();
                if let Some(response_id) = &result.response_id {
                    extensions.additional_properties.insert(
                        "dev.kit.response".into(),
                        super::object([("id", response_id.clone().into())]),
                    );
                }
                if let Some(reasoning) = reported
                    .as_ref()
                    .and_then(|usage| usage.tokens.as_ref())
                    .and_then(|tokens| tokens.reasoning_tokens)
                {
                    extensions.additional_properties.insert(
                        "dev.kit.usage".into(),
                        super::object([("reasoningTokens", reasoning.into())]),
                    );
                }
                if !extensions.additional_properties.is_empty() {
                    input.extensions = Some(Box::new(extensions));
                }
                if let Some(model) = &result.model {
                    input.model.id = model.clone();
                }
                let input = input
                    .with_sources()
                    .with_items(super::messages(&result.output_items));
                let boundary = call.hooks.sdk.model_response_after(input);
                let outcome = super::dispatch(boundary, cancellation.as_ref()).await?;
                let outcome = call
                    .hooks
                    .settle(EventType::ModelResponseAfter, Ok(outcome), &call.session.id)
                    .ok_or_else(|| invalid("model response hook dispatch failed"))?;
                if outcome.outcome.stopped || outcome.outcome.is_denied() {
                    return Err(invalid("model response stopped by agent hook"));
                }
                if super::has_modification(&outcome)
                    && let Event::ModelResponseAfter(edited) = outcome.event
                {
                    result.output_items = canonical_items(
                        edited.0.items.iter().map(|item| item.0.as_ref()),
                        &result.output_items,
                    )?;
                    if result.finish_reason == FinishReason::ToolCall
                        && !result
                            .output_items
                            .iter()
                            .flat_map(|item| &item.parts)
                            .any(|part| matches!(part, Part::ToolCall(_)))
                    {
                        result.finish_reason = FinishReason::Completed;
                    }
                }
            }
            Err(error) => call.failed(error, self.latest_usage.as_ref()).await,
            _ => {}
        }
        if call.hooks.intercepts_model_response
            && let Ok(Some(ModelTurnEvent::Finished(result))) = event
        {
            // agentkit-loop gates tool execution on a ToolCall event, not only
            // Finished.output_items. Reconstruct notifications from approved
            // output, never from withheld (possibly removed) provider previews.
            self.approved_events.extend(
                result
                    .output_items
                    .iter()
                    .flat_map(|item| &item.parts)
                    .filter_map(|part| match part {
                        Part::ToolCall(call) => Some(ModelTurnEvent::ToolCall(call.clone())),
                        _ => None,
                    }),
            );
            self.approved_events
                .push_back(ModelTurnEvent::Finished(result));
            return Ok(self.approved_events.pop_front());
        }
        event
    }
}

fn invalid(message: &str) -> LoopError {
    LoopError::InvalidState(message.to_owned())
}

type ProjectedPart = agenthooksprotocol::inputs::PartInput<agenthooksprotocol::Attachment>;

struct OriginalMessage<'a> {
    item: &'a Item,
    groups: Vec<Vec<ProjectedPart>>,
    part_groups: HashMap<String, usize>,
}

impl<'a> OriginalMessage<'a> {
    fn new(item: &'a Item, id: &str) -> Self {
        let mut groups = Vec::with_capacity(item.parts.len());
        let mut part_groups = HashMap::new();
        for (index, part) in item.parts.iter().enumerate() {
            let projected = super::content_parts(&format!("{id}/{index}"), part);
            for part in &projected {
                let id = match part {
                    agenthooksprotocol::inputs::PartInput::Text(body) => &body.id,
                    agenthooksprotocol::inputs::PartInput::Attachment { metadata, .. } => {
                        &metadata.id
                    }
                    agenthooksprotocol::inputs::PartInput::Wire(part) => content_id(part),
                };
                part_groups.insert(id.to_owned(), index);
            }
            groups.push(projected);
        }
        Self {
            item,
            groups,
            part_groups,
        }
    }

    fn unchanged(&self, message: &CanonicalMessage) -> bool {
        super::role(self.item.kind) == message.role
            && self.groups.iter().map(Vec::len).sum::<usize>() == message.parts.len()
            && self
                .groups
                .iter()
                .flatten()
                .zip(&message.parts)
                .all(|(before, after)| projected_part_matches(before, after))
    }
}

/// Project each original once; index identities rather than searching the
/// transcript or re-projecting native parts for every edited part.
fn canonical_items<'a>(
    messages: impl IntoIterator<Item = &'a CanonicalMessage>,
    originals: &[Item],
) -> Result<Vec<Item>, LoopError> {
    let mut by_id = HashMap::with_capacity(originals.len());
    for (index, item) in originals.iter().enumerate() {
        let id = super::item_id(item, index);
        let original = OriginalMessage::new(item, &id);
        if by_id.insert(id, original).is_some() {
            return Err(invalid("duplicate original model message identity"));
        }
    }
    let mut seen = HashSet::new();
    messages
        .into_iter()
        .map(|message| {
            if !seen.insert(&message.id) {
                return Err(invalid("duplicate edited model message identity"));
            }
            canonical_item(message, by_id.get(&message.id))
        })
        .collect()
}

fn canonical_item(
    message: &CanonicalMessage,
    original: Option<&OriginalMessage<'_>>,
) -> Result<Item, LoopError> {
    if let Some(original) = original {
        if original.unchanged(message) {
            // Includes all provider state that intentionally has no projection.
            return Ok(original.item.clone());
        }
        if original.groups.iter().any(Vec::is_empty) {
            return Err(invalid(
                "cannot partially edit a model message containing opaque unprojected provider content",
            ));
        }
    }
    let kind = if message.role == CanonicalMessageRole::Tool
        && original.is_some_and(|original| original.item.kind == ItemKind::Tool)
    {
        ItemKind::Tool
    } else {
        super::message_kind(&message.role).map_err(LoopError::InvalidState)?
    };
    let mut parts = Vec::new();
    let mut remaining = message.parts.as_slice();
    let mut next_group = 0;
    while let Some(part) = remaining.first() {
        if let Some(original) = original
            && let Some(&index) = original.part_groups.get(content_id(part))
        {
            if index < next_group {
                return Err(invalid("model hook cannot reorder or repeat native parts"));
            }
            next_group = index + 1;
            let projected = &original.groups[index];
            if remaining.len() >= projected.len()
                && projected
                    .iter()
                    .zip(remaining)
                    .all(|(before, after)| projected_part_matches(before, after))
            {
                parts.push(original.item.parts[index].clone());
                remaining = &remaining[projected.len()..];
                continue;
            }
            // A multipart native value must be retained as a whole. Descriptor
            // text alone is not interchangeable with the native value it labels.
            if !matches!(original.item.parts[index], Part::Text(_)) || projected.len() != 1 {
                return Err(invalid(
                    "model hook cannot partially edit a native non-text part",
                ));
            }
        }
        parts.push(plain_text(part)?);
        remaining = &remaining[1..];
    }
    if let Some(original) = original {
        let mut item = original.item.clone();
        if super::role(item.kind) != message.role {
            item.kind = kind;
        }
        item.parts = parts;
        Ok(item)
    } else {
        Ok(Item::new(kind, parts))
    }
}

fn plain_text(part: &ContentItem) -> Result<Part, LoopError> {
    match part {
        ContentItem::TextBodyPart(body)
            if matches!(&body.category, Presence::Missing)
                || body.category == Presence::Present("text".into()) =>
        {
            Ok(Part::text(body.text.clone()))
        }
        _ => Err(invalid(
            "model hook replacement must contain plain text; opaque parts can only be retained unchanged",
        )),
    }
}

fn content_id(part: &ContentItem) -> &str {
    match part {
        ContentItem::TextBodyPart(part) => &part.id,
        ContentItem::TextGapPart(part) => &part.id,
        ContentItem::TextTextPlainMetadata(part) => &part.id,
        ContentItem::TextTextPlainOmit(part) => &part.id,
        ContentItem::AttachmentBodyPart(part) => &part.id,
        ContentItem::AttachmentGapPart(part) => &part.id,
        ContentItem::AttachmentMetadata(part) => &part.id,
        ContentItem::AttachmentOmit(part) => &part.id,
    }
}

fn projected_part_matches(
    before: &agenthooksprotocol::inputs::PartInput<agenthooksprotocol::Attachment>,
    after: &ContentItem,
) -> bool {
    use agenthooksprotocol::inputs::PartInput;
    match (before, after) {
        (PartInput::Text(body), ContentItem::TextBodyPart(other)) => body == other.as_ref(),
        (PartInput::Wire(part), other) => part == other,
        (PartInput::Attachment { metadata, .. }, ContentItem::AttachmentBodyPart(other)) => {
            // SDK 0.8 runtime::validate_parts rejects changed attachment bodies
            // and identities. Its runtime-owned reference replaces host-pending;
            // compare the stable identity, never read or reinterpret the bytes.
            metadata.id == other.id
                && metadata.media_type == other.media_type
                && metadata.category == other.category
                && metadata.synthesized == other.synthesized
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
#[path = "model_effects_tests.rs"]
mod tests;

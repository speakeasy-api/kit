//! Generic loop boundaries. Executor facts and nested-tool lineage are emitted by
//! the tool bridge, not inferred from transcript placeholders.
use super::AhpHost;
use agenthooksprotocol::hooks::Capabilities;
use agentkit_core::{Delta, FinishReason, Item, MessageId, Part, TurnCancellation};
use agentkit_loop::{
    DeliveryError, HookCtx, LoopCtx, LoopError, LoopMutator, ModelRequestPayload,
    ModelResponsePayload, ModelTurnEvent, MutationPoint, NativeDelivery, NativeFact,
    NativeProgress, OutputItemPayload, ResponseDisposition, SessionStartPayload, TranscriptCursor,
    TurnResult,
};
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn supported() -> BTreeMap<String, Capabilities> {
    [
        ("session.start", Capabilities::none()),
        ("session.end", Capabilities::none()),
        (
            "user.message.inbound",
            Capabilities::none().modify("prompt", true, true),
        ),
        (
            "turn.start",
            Capabilities::none().modify("prompt", true, true),
        ),
        (
            "turn.finish.before",
            Capabilities::none().modify("response", true, true),
        ),
        ("turn.end", Capabilities::none()),
        ("turn.progress", Capabilities::none()),
        (
            "model.request.before",
            Capabilities::none().modify("request", true, true),
        ),
        (
            "model.response.after",
            Capabilities::none().modify("response", true, true),
        ),
        ("model.error", Capabilities::none()),
    ]
    .into_iter()
    .map(|(name, caps)| (name.to_owned(), caps))
    .collect()
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    ))
}
fn encoded(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}
fn encode(value: impl Serialize) -> Result<Value, LoopError> {
    encoded(value).map_err(LoopError::Mutator)
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, LoopError> {
    serde_json::from_value(value)
        .map_err(|e| LoopError::Mutator(format!("invalid AHP lifecycle edit: {e}")))
}
fn base(kind: &str, session: &str, turn: Option<String>) -> Value {
    let mut event = object([
        ("type", Value::from(kind)),
        ("session", object([("id", Value::from(session))])),
    ]);
    if let Some(turn) = turn {
        event["turn"] = object([("id", Value::from(turn))]);
    }
    event
}
fn hook_base(kind: &str, ctx: &HookCtx<'_>) -> Value {
    base(
        kind,
        &ctx.session_id.to_string(),
        ctx.turn_id.map(ToString::to_string),
    )
}
fn gap(event: &mut Value, path: &str, reason: &str) {
    if event.get("gaps").is_none() {
        event["gaps"] = Value::Array(Vec::new());
    }
    if let Some(gaps) = event["gaps"].as_array_mut() {
        gaps.push(object([
            ("path", Value::from(path)),
            ("reason", Value::from(reason)),
        ]));
    }
}
fn items(event: &mut Value, value: Value) -> Vec<(String, Value)> {
    // The actor allocates a fresh descriptor identity when it stages the body.
    // Aggregate payloads retain their native item roles inside the body.
    event["items"] = Value::Array(vec![object([("role", Value::from("context"))])]);
    vec![("/items/0".into(), value)]
}
fn tool_calls(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            Part::ToolCall(call) => Some(call.id.to_string()),
            _ => None,
        })
        .collect()
}
/// Kit's own prompt-edit contract. `MutationPoint::TurnStarted` deliberately
/// does not freeze item identity or accounting, so the restrictions a native
/// prompt replacement must respect are enforced here, against the complete
/// candidate, before anything is written to the live transcript.
fn stage_prompt(candidate: &[Item], live: &[Item]) -> Result<(), LoopError> {
    let reject = |message: &str| Err(LoopError::Mutator(format!("AHP prompt edit {message}")));
    if candidate.is_empty() {
        return reject("cannot erase the committed transcript");
    }
    let mut ids: BTreeSet<&MessageId> = BTreeSet::new();
    for item in candidate {
        let Some(id) = item.id.as_ref() else { continue };
        if !ids.insert(id) {
            return reject(&format!("duplicated the identity of item {id}"));
        }
        let Some(original) = live.iter().find(|live| live.id.as_ref() == Some(id)) else {
            continue;
        };
        if original.kind != item.kind {
            return reject(&format!("changed the role of committed item {id}"));
        }
        if original.created_at != item.created_at
            || original.usage != item.usage
            || original.finish_reason != item.finish_reason
        {
            return reject(&format!("changed the accounting of committed item {id}"));
        }
    }
    let mut calls: BTreeSet<String> = BTreeSet::new();
    for call in tool_calls(candidate) {
        if !calls.insert(call.clone()) {
            return reject(&format!("duplicated tool call {call}"));
        }
    }
    for part in candidate.iter().flat_map(|item| &item.parts) {
        if let Part::ToolResult(result) = part
            && !calls.contains(&result.call_id.to_string())
        {
            return reject(&format!(
                "orphaned the result of tool call {}",
                result.call_id
            ));
        }
    }
    Ok(())
}
fn outcome(result: &TurnResult) -> &'static str {
    match result.finish_reason {
        FinishReason::Cancelled => "cancelled",
        FinishReason::Error => "failed",
        _ => "completed",
    }
}
fn terminal(event: &mut Value, outcome: &str) {
    event["outcome"] = Value::from(outcome);
    event["continuationCount"] = Value::from(0);
    // This bridge advertises no continuation capability and implements none.
}
/// The staged item as a hook sees it: `original`'s identity and accounting
/// carrying the editable content.
fn staged_item(original: &Item, parts: &[Part], metadata: &agentkit_core::MetadataMap) -> Item {
    let mut item = original.clone();
    item.parts = parts.to_vec();
    item.metadata = metadata.clone();
    item
}
/// An output edit may rewrite content, never identity or accounting.
fn validate_output_edit(original: &Item, candidate: &Item) -> Result<(), LoopError> {
    if &staged_item(original, &candidate.parts, &candidate.metadata) == candidate {
        return Ok(());
    }
    Err(LoopError::Mutator(
        "AHP output edit changed immutable item fields".into(),
    ))
}
fn output_items(output: &[OutputItemPayload<'_>]) -> Vec<Item> {
    output
        .iter()
        .map(|o| staged_item(o.original, o.parts, o.metadata))
        .collect()
}
fn apply_output(output: &mut [OutputItemPayload<'_>], edited: Vec<Item>) -> Result<(), LoopError> {
    if output.len() != edited.len() {
        return Err(LoopError::Mutator(
            "AHP output edit changed item count".into(),
        ));
    }
    // Validate the complete candidate before changing any staged item.
    for (original, candidate) in output.iter().zip(&edited) {
        validate_output_edit(original.original, candidate)?;
    }
    for (output, candidate) in output.iter_mut().zip(edited) {
        *output.parts = candidate.parts;
        *output.metadata = candidate.metadata;
    }
    Ok(())
}

fn delta_part(delta: &Delta) -> Option<String> {
    match delta {
        Delta::BeginPart { part_id, .. }
        | Delta::AppendText { part_id, .. }
        | Delta::AppendBytes { part_id, .. }
        | Delta::ReplaceStructured { part_id, .. }
        | Delta::SetMetadata { part_id, .. } => Some(part_id.to_string()),
        // CommitPart carries no stable part identity. Do not invent lineage.
        Delta::CommitPart { .. } => None,
    }
}

impl AhpHost {
    fn model_facts(&self, event: &mut Value, session: &str, attempt: u64) {
        // Kit's own session selection, not a loop-reported routing decision.
        let (id, provider) = self.selection_snapshot();
        event["model"] = object([("id", Value::from(id)), ("provider", Value::from(provider))]);
        gap(
            event,
            "/model",
            "Kit session selection snapshot; provider routing and retry attempts are not exposed by the generic loop",
        );
        let attempt = attempt.max(1);
        event["attempt"] = object([
            ("id", Value::from(format!("{session}:model-call:{attempt}"))),
            ("number", Value::from(attempt)),
            ("synthesized", Value::from(true)),
        ]);
        gap(
            event,
            "/attempt",
            "Synthesized model-call identity and session-local call index, not a provider retry number",
        );
    }

    /// Kit owns inbound admission now that the loop no longer intercepts
    /// submitted input. Callers dispatch after `session.start` so the ordering
    /// a hook consumer already relied on is preserved.
    pub async fn inbound(
        &self,
        session: &str,
        input: Vec<Item>,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Vec<Item>, LoopError> {
        let mut event = base("user.message.inbound", session, None);
        event["message"] = object([
            ("channel", Value::from("loop-input")),
            ("sender", Value::from("host")),
            ("messages", Value::Array(vec![])),
        ]);
        gap(
            &mut event,
            "/message/sender",
            "Kit submits host input; prompt admission does not carry an authenticated user identity",
        );
        let settled = self
            .lifecycle_event(
                event,
                vec![("/message/messages".into(), encode(&input)?)],
                vec![("prompt".into(), "/message/messages".into())],
                cancellation,
            )
            .await?;
        let Some(value) = settled.bodies.get("/message/messages") else {
            return Ok(input);
        };
        let candidate: Vec<Item> = decode(value.clone())?;
        stage_prompt(&candidate, &input)?;
        Ok(candidate)
    }

    /// Kit owns session closure: the loop has no close boundary, so the owner
    /// that retired the interrupted turn reports the ending it performed.
    /// `transcript` is `None` only where the owner genuinely has none to
    /// report, which is stated as a gap rather than sent as an empty one.
    pub(crate) async fn session_end(
        &self,
        session: &str,
        transcript: Option<&[Item]>,
        outcome: SessionOutcome,
        reason: &str,
    ) -> Result<(), String> {
        let mut event = base("session.end", session, None);
        event["outcome"] = Value::from(outcome.as_str());
        event["reason"] = Value::from(reason);
        gap(
            &mut event,
            "/items/role",
            "The SDK session.end schema carries parts rather than messages; scoped part identities are retained, but native message roles have no canonical field here",
        );
        let bodies = match transcript {
            Some(transcript) => items(&mut event, encoded(transcript)?),
            None => {
                gap(
                    &mut event,
                    "/items",
                    "Kit closed this session before a driver existed, so it has no transcript to report",
                );
                vec![]
            }
        };
        self.event(event, bodies, vec![]).await.map(|_| ())
    }

    /// Model failures are classified at Kit's own adapter boundaries. The loop
    /// no longer reports them, and cancellation was never a model error.
    pub(crate) async fn model_error(
        &self,
        session: &str,
        failure: ModelFailure<'_>,
    ) -> Result<(), String> {
        let mut event = base("model.error", session, None);
        self.model_facts(&mut event, session, self.model_call_index());
        gap(
            &mut event,
            "/turn",
            "Kit classifies model failures at the adapter boundary, which does not carry the loop's logical turn identity",
        );
        event["execution"] = object([("status", Value::from("executed"))]);
        let mut error = object([
            ("message", Value::from(failure.message)),
            ("class", Value::from(failure.class)),
        ]);
        // Only facts the provider actually reported are carried through.
        if let Some(code) = failure.code.filter(|code| !code.is_empty()) {
            error["code"] = Value::from(code);
        }
        if let Some(status) = failure.status {
            error["status"] = Value::from(status);
        }
        event["error"] = error;
        self.event(event, vec![], vec![]).await.map(|_| ())
    }

    async fn deliver_progress(
        &self,
        progress: NativeProgress<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<(), String> {
        // Usage updates and retry invalidation are not model-visible content
        // deltas. Do not invent item identity for them.
        let mut event = hook_base("turn.progress", ctx);
        let body = match progress {
            NativeProgress::Model(ModelTurnEvent::Delta(delta)) => {
                let Some(id) = delta_part(delta) else {
                    return Ok(());
                };
                event["item"] = object([("id", Value::from(id.clone()))]);
                event["delta"] =
                    object([("id", Value::from(id)), ("role", Value::from("assistant"))]);
                event["final"] = Value::from(false);
                gap(
                    &mut event,
                    "/final",
                    "The generic commit delta does not expose a stable part identity for final correlation",
                );
                encoded(delta)?
            }
            NativeProgress::ToolDetached(result) => {
                let id = result.call_id.to_string();
                event["item"] = object([("id", Value::from(id.clone()))]);
                event["delta"] = object([("id", Value::from(id)), ("role", Value::from("tool"))]);
                // The placeholder is the complete synthetic result for the call.
                event["final"] = Value::from(true);
                gap(
                    &mut event,
                    "/delta",
                    "Loop-authored detach placeholder for a tool call that moved to the background; no model produced it",
                );
                encoded(result)?
            }
            _ => return Ok(()),
        };
        // The active turn's handle is present for progress, so a slow delivery
        // cannot outlive the turn it describes. Terminal facts are delivered
        // uncancelled; `HookCtx::cancellation` is `None` for them.
        self.event_with_cancellation(
            event,
            vec![("/delta".into(), body)],
            vec![],
            ctx.cancellation.clone(),
        )
        .await
        .map(|_| ())
    }

    async fn deliver_prefinish(
        &self,
        result: &TurnResult,
        ctx: &HookCtx<'_>,
    ) -> Result<(), String> {
        let turn = ctx.turn_id.map(ToString::to_string);
        if turn
            .as_deref()
            .is_some_and(|turn| self.take_settled_prefinish(turn))
        {
            // The normal finish candidate already settled this turn's editable
            // prefinish over the same staged output; one per logical turn.
            return Ok(());
        }
        let mut event = base("turn.finish.before", &ctx.session_id.to_string(), turn);
        terminal(&mut event, outcome(result));
        // Cancellation partials and tool-only endings stay observations: this
        // boundary advertises no response target, so no edit can settle here.
        let bodies = items(&mut event, encoded(&result.items)?);
        event["items"][0]["role"] = Value::from("assistant");
        self.event(event, bodies, vec![]).await.map(|_| ())
    }

    async fn deliver_turn_end(&self, result: &TurnResult, ctx: &HookCtx<'_>) -> Result<(), String> {
        let turn = ctx.turn_id.map(ToString::to_string);
        if let Some(turn) = turn.as_deref() {
            self.take_settled_prefinish(turn);
        }
        let mut event = base("turn.end", &ctx.session_id.to_string(), turn);
        terminal(&mut event, outcome(result));
        let bodies = items(&mut event, encoded(&result.items)?);
        event["items"][0]["role"] = Value::from("assistant");
        self.event(event, bodies, vec![]).await.map(|_| ())
    }
}

/// One real model failure as Kit classified it at its own boundary. `code` and
/// `status` are present only when the provider actually reported them.
pub(crate) struct ModelFailure<'a> {
    pub class: &'static str,
    pub message: &'a str,
    pub code: Option<String>,
    pub status: Option<u16>,
}

/// How a Kit-owned session closure ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// The owner closed the session after its work finished.
    Completed,
    /// The owner closed the session after cancellation.
    Cancelled,
    /// The owner closed the session after a failure.
    Error,
}

impl SessionOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

#[async_trait]
impl NativeDelivery for AhpHost {
    async fn deliver(&self, fact: NativeFact<'_>, ctx: HookCtx<'_>) -> Result<(), DeliveryError> {
        // A delivery failure is a diagnostic the driver drains, never a loop
        // error: terminal facts must not be able to fail an operation.
        match fact {
            NativeFact::Progress(progress) => self.deliver_progress(progress, &ctx).await,
            NativeFact::BeforeFinish(result) => self.deliver_prefinish(result, &ctx).await,
            NativeFact::TurnFinished(result) => self.deliver_turn_end(result, &ctx).await,
            _ => Ok(()),
        }
        .map_err(DeliveryError::new)
    }
}

#[async_trait]
impl LoopMutator for AhpHost {
    async fn on_session_start(
        &self,
        payload: SessionStartPayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        let mut event = hook_base("session.start", &ctx);
        event["trigger"] = Value::from("startup");
        event["harness"] = object([
            ("name", Value::from("kit")),
            ("version", Value::from(env!("CARGO_PKG_VERSION"))),
        ]);
        event["permissionMode"] = Value::from("unknown");
        gap(
            &mut event,
            "/permissionMode",
            "Permission policy is not exposed by the generic loop",
        );
        gap(
            &mut event,
            "/trigger",
            "The generic session-start callback does not distinguish resume, fork, or startup",
        );
        event["items"] = Value::Array(vec![]);
        // Session construction facts are native context, not model-visible
        // messages. Delivery of opaque native context requires SDK includeNative.
        event["native"] = object([
            ("metadata", encode(&*payload.metadata)?),
            ("cache", encode(&*payload.cache)?),
            (
                "consumer_capabilities",
                encode(*payload.consumer_capabilities)?,
            ),
        ]);
        self.lifecycle_event(event, vec![], vec![], ctx.cancellation.clone())
            .await?;
        Ok(())
    }
    async fn on_model_request(
        &self,
        payload: ModelRequestPayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        let session = ctx.session_id.to_string();
        let mut event = hook_base("model.request.before", &ctx);
        self.model_facts(&mut event, &session, self.begin_model_call());
        // SDK 0.3 request edits address canonical model-visible messages.
        // Cache, tools and request metadata are native immutable context here.
        event["params"] = object([
            ("available_tools", encode(&*payload.available_tools)?),
            ("cache", encode(&*payload.cache)?),
            ("metadata", encode(&*payload.metadata)?),
        ]);
        let bodies = items(&mut event, encode(&*payload.transcript)?);
        let settled = self
            .lifecycle_event(
                event,
                bodies,
                vec![("request".into(), "/items/0".into())],
                ctx.cancellation.clone(),
            )
            .await?;
        if let Some(value) = settled.bodies.get("/items/0") {
            let candidate: Vec<Item> = decode(value.clone())?;
            stage_prompt(&candidate, payload.transcript)?;
            *payload.transcript = candidate;
        }
        Ok(())
    }
    async fn on_model_response(
        &self,
        mut payload: ModelResponsePayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        let session = ctx.session_id.to_string();
        let mut event = hook_base("model.response.after", &ctx);
        self.model_facts(&mut event, &session, self.model_call_index());
        event["execution"] = object([("status", Value::from("executed"))]);
        event["finishReason"] = encode(payload.finish_reason)?;
        // FinishReason::Other serializes as an object; canonical finishReason is a string.
        if !event["finishReason"].is_string() {
            event["finishReason"] = Value::from(format!("{:?}", payload.finish_reason));
        }
        let bodies = items(&mut event, encode(output_items(&payload.output))?);
        event["items"][0]["role"] = Value::from("assistant");
        let settled = self
            .lifecycle_event(
                event,
                bodies,
                vec![("response".into(), "/items/0".into())],
                ctx.cancellation.clone(),
            )
            .await?;
        if let Some(value) = settled.bodies.get("/items/0") {
            apply_output(&mut payload.output, decode(value.clone())?)?;
        }
        if payload.disposition != ResponseDisposition::FinishTurnCandidate {
            return Ok(());
        }
        let Some(turn) = ctx.turn_id.map(ToString::to_string) else {
            return Ok(());
        };
        // The normal finish branch keeps its editable prefinish, run over the
        // same staged output so a response edit and a prefinish edit compose.
        let mut event = base("turn.finish.before", &session, Some(turn.clone()));
        terminal(&mut event, "completed");
        let bodies = items(&mut event, encode(output_items(&payload.output))?);
        event["items"][0]["role"] = Value::from("assistant");
        let settled = self
            .lifecycle_event(
                event,
                bodies,
                vec![("response".into(), "/items/0".into())],
                ctx.cancellation.clone(),
            )
            .await?;
        // Claim once the boundary has settled, before the settled output is
        // applied. Invariant: a settled pre-finish is this turn's one
        // pre-finish, because the consumer has already observed and answered
        // it; whether Kit then fails to apply the settled output changes the
        // turn's outcome (reported at `turn.end`), not how many pre-finish
        // boundaries the turn had. A boundary that never settled — a hook
        // error, a cancelled boundary, or a response that was not a finish
        // candidate — claims nothing, so the awaited read-only delivery still
        // emits the turn's only pre-finish.
        self.settle_prefinish(turn);
        if let Some(value) = settled.bodies.get("/items/0") {
            apply_output(&mut payload.output, decode(value.clone())?)?;
        }
        Ok(())
    }
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        // Turn start is the only transcript mutation this host performs. The
        // point covers synthetic and non-dispatching turns and sees admitted
        // input, so the prompt a turn will answer is editable exactly once.
        if ctx.point != MutationPoint::TurnStarted {
            return Ok(());
        }
        let mut event = base(
            "turn.start",
            &ctx.session_id.to_string(),
            ctx.turn_id.map(ToString::to_string),
        );
        event["trigger"] = Value::from("external");
        gap(
            &mut event,
            "/trigger",
            "Input origin is not exposed by the generic loop",
        );
        let bodies = items(&mut event, encode(cursor.as_slice())?);
        let settled = self
            .lifecycle_event(
                event,
                bodies,
                vec![("prompt".into(), "/items/0".into())],
                ctx.cancellation.clone(),
            )
            .await?;
        if let Some(value) = settled.bodies.get("/items/0") {
            let candidate: Vec<Item> = decode(value.clone())?;
            stage_prompt(&candidate, cursor.as_slice())?;
            if candidate.as_slice() != cursor.as_slice() {
                **cursor = candidate;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use agentkit_core::{ItemKind, Part, ToolCallPart, ToolOutput, ToolResultPart, Usage};

    #[test]
    fn output_edits_change_content_not_identity() {
        let original = Item::text(ItemKind::Assistant, "before");
        let mut edited = original.clone();
        edited.parts = vec![Part::text("after")];
        validate_output_edit(&original, &edited).unwrap();
        assert_eq!(
            staged_item(&original, &edited.parts, &edited.metadata),
            edited
        );
        assert_eq!(original.parts, vec![Part::text("before")]);
    }

    #[test]
    fn immutable_output_edits_are_rejected() {
        let original = Item::text(ItemKind::Assistant, "before");
        for mutate in [
            (|item: &mut Item| item.kind = ItemKind::User) as fn(&mut Item),
            |item: &mut Item| item.id = Some("reassigned".into()),
            |item: &mut Item| item.usage = Some(Usage::default()),
            |item: &mut Item| item.finish_reason = Some(FinishReason::Error),
        ] {
            let mut edited = original.clone();
            edited.parts = vec![Part::text("after")];
            mutate(&mut edited);
            assert!(validate_output_edit(&original, &edited).is_err());
        }
    }

    #[test]
    fn output_edit_cannot_change_item_count() {
        // `OutputItemPayload` is non-exhaustive, so the staged slice is only
        // ever the loop's own; an empty one still proves the count rule.
        let mut staged: [OutputItemPayload<'_>; 0] = [];
        assert!(
            apply_output(
                &mut staged,
                vec![Item::text(ItemKind::Assistant, "injected")]
            )
            .is_err()
        );
    }

    fn identified(kind: ItemKind, id: &str, text: &str) -> Item {
        let mut item = Item::text(kind, text);
        item.id = Some(id.into());
        item
    }

    #[test]
    fn prompt_replacement_may_add_and_drop_items() {
        let live = vec![identified(ItemKind::System, "system", "system")];
        let mut candidate = live.clone();
        candidate.push(Item::text(ItemKind::User, "replaced prompt"));
        stage_prompt(&candidate, &live).unwrap();
    }

    #[test]
    fn prompt_replacement_rejects_erasure_and_identity_rewrites() {
        let live = vec![
            identified(ItemKind::System, "system", "system"),
            identified(ItemKind::Assistant, "answer", "answer"),
        ];
        assert!(stage_prompt(&[], &live).is_err());
        let mut rerole = live.clone();
        rerole[1].kind = ItemKind::User;
        assert!(stage_prompt(&rerole, &live).is_err());
        let mut reaccount = live.clone();
        reaccount[1].usage = Some(Usage::default());
        assert!(stage_prompt(&reaccount, &live).is_err());
        let mut duplicated = live.clone();
        duplicated.push(live[0].clone());
        assert!(stage_prompt(&duplicated, &live).is_err());
    }

    #[test]
    fn prompt_replacement_rejects_orphaned_and_duplicated_tool_linkage() {
        let mut call = Item::text(ItemKind::Assistant, "");
        call.parts = vec![Part::ToolCall(ToolCallPart {
            id: "call".into(),
            name: "edit".into(),
            input: Value::Null,
            metadata: agentkit_core::MetadataMap::new(),
        })];
        let mut result = Item::text(ItemKind::Tool, "");
        result.parts = vec![Part::ToolResult(ToolResultPart::success(
            "call",
            ToolOutput::Text("done".into()),
        ))];
        let live = vec![call.clone(), result.clone()];
        stage_prompt(&live, &live).unwrap();
        assert!(stage_prompt(&[result.clone()], &live).is_err());
        assert!(stage_prompt(&[call.clone(), call.clone()], &live).is_err());
    }

    #[test]
    fn capabilities_only_advertise_consumed_modifications() {
        for capabilities in supported().into_values() {
            let effects = capabilities.as_value()["effects"]
                .as_array()
                .unwrap()
                .clone();
            assert!(effects.iter().all(|effect| effect == "modify"));
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used
)]
mod sdk_smoke {
    use super::*;
    use crate::ahp::{AhpConfig, HostDelivery, HostMutator, SessionOutcome, model::HookedAdapter};
    use agentkit_core::{ItemKind, MetadataMap};
    use agentkit_loop::{
        Agent, LoopStep, ModelAdapter, ModelSession, ModelTurn, ModelTurnResult, SessionConfig,
        TurnRequest,
    };
    use std::{collections::VecDeque, sync::Arc};

    struct Model {
        fail: bool,
    }
    struct Session {
        fail: bool,
    }
    struct Stream {
        events: VecDeque<ModelTurnEvent>,
    }
    #[async_trait]
    impl ModelAdapter for Model {
        type Session = Session;
        async fn start_session(&self, _: SessionConfig) -> Result<Session, LoopError> {
            Ok(Session { fail: self.fail })
        }
        fn provider_name(&self) -> Option<&str> {
            Some("test")
        }
    }
    #[async_trait]
    impl ModelSession for Session {
        type Turn = Stream;
        async fn begin_turn(
            &mut self,
            _: TurnRequest,
            _: Option<TurnCancellation>,
        ) -> Result<Stream, LoopError> {
            if self.fail {
                return Err(LoopError::Provider("test failure".into()));
            }
            Ok(Stream {
                events: VecDeque::from([
                    ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: "test-part".into(),
                        chunk: "hello".into(),
                    }),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        finish_reason: FinishReason::Completed,
                        output_items: vec![Item::text(ItemKind::Assistant, "hello")],
                        usage: None,
                        metadata: MetadataMap::new(),
                        model: Some("test-model".into()),
                        response_id: None,
                    }),
                ]),
            })
        }
        fn model_name(&self) -> Option<&str> {
            Some("test-model")
        }
    }
    #[async_trait]
    impl ModelTurn for Stream {
        async fn next_event(
            &mut self,
            _: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    // Kit's real registration wrappers and its real Kit-owned closure. The
    // model adapter is the only fake; every event is authored by AhpHost.
    fn agent(host: &Arc<AhpHost>, fail: bool) -> Agent<HookedAdapter<Model>> {
        Agent::builder()
            .model(HookedAdapter::new(Model { fail }, Some(host.clone())))
            .mutator(HostMutator::new(host.clone()))
            .delivery(HostDelivery::new(host.clone()))
            .build()
            .unwrap()
    }

    async fn drive(host: &Arc<AhpHost>, session: &str, fail: bool) {
        let agent = agent(host, fail);
        let mut driver = agent.start(SessionConfig::new(session)).await.unwrap();
        // Empty prompts are rejected by Kit's own admission, before the driver.
        assert!(host.inbound(session, vec![], None).await.is_err());
        assert!(driver.snapshot().pending_input.is_empty());
        let input = host
            .inbound(session, vec![Item::text(ItemKind::User, "hello")], None)
            .await
            .unwrap();
        driver.submit_input(input).unwrap();
        let result = driver.next().await;
        if fail {
            assert!(result.is_err());
        } else {
            assert!(matches!(result.unwrap(), LoopStep::Finished(_)));
        }
        let outcome = if fail {
            SessionOutcome::Error
        } else {
            SessionOutcome::Completed
        };
        crate::ahp::close_session(&mut driver, Some(host), session, outcome)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn actual_host_agent_lifecycle_validates_every_emitted_sdk_schema() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = axum::Router::new().route(
            "/hooks",
            axum::routing::post(move |axum::Json(message): axum::Json<Value>| {
                let sender = sender.clone();
                async move {
                    sender.send(message).unwrap();
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let names: Vec<_> = supported().into_keys().collect();
        let grants: BTreeMap<String, Value> = names
            .iter()
            .map(|name| (name.clone(), json!({"modes":["observe"]})))
            .collect();
        for fail in [false, true] {
            let host = AhpHost::new_with_options(AhpConfig {
            registration: json!({"protocolVersion":"draft","hooks":[{"id":"org.kit.lifecycle-smoke","transport":{"type":"http","url":endpoint},"subscriptions":[{"events":names,"mode":"observe","content":{"default":"body"}}]}]}),
            grants: grants.clone(),
        }, crate::ahp::SessionFacts { trigger: "startup".into(), permission_mode: "allow-all".into(), resumed_from: None }, agenthooksprotocol::adapters::registered::BackendOptions { allow_loopback_http: true, ..Default::default() }).unwrap();
            drive(
                &host,
                if fail {
                    "error-session"
                } else {
                    "success-session"
                },
                fail,
            )
            .await;
        }
        let mut observed = std::collections::BTreeSet::new();
        while let Ok(message) = receiver.try_recv() {
            assert_eq!(message["method"], "hooks/observe");
            assert!(
                agenthooksprotocol::generated::parse_observe_notification_value(message.clone())
                    .is_ok()
            );
            let event = &message["params"]["event"];
            assert!(
                agenthooksprotocol::generated::parse_execution_event_value(event.clone()).is_ok()
            );
            let messages = match event["type"].as_str().unwrap() {
                "user.message.inbound" => Some(&event["message"]["messages"]),
                "turn.start"
                | "model.request.before"
                | "model.response.after"
                | "turn.finish.before" => Some(&event["items"]),
                _ => None,
            };
            if let Some(messages) = messages {
                let messages = messages.as_array().unwrap();
                // Empty admission and failed-output receipts are valid canonical
                // observations; native admission still rejects erasing a prompt.
                for message in messages {
                    assert!(message["id"].is_string());
                    assert!(matches!(
                        message["role"].as_str(),
                        Some("user" | "assistant")
                    ));
                    assert_eq!(message["parts"][0]["kind"], "text");
                    assert_eq!(message["parts"][0]["text"], "hello");
                    assert!(message.get("nativeItem").is_none());
                    assert!(message.get("nativeVec").is_none());
                }
            }
            observed.insert(event["type"].as_str().unwrap().to_owned());
            assert!(event["id"].is_string());
            assert!(event["source"].is_string());
        }
        assert_eq!(observed, names.into_iter().collect());
        server.abort();
    }

    #[tokio::test]
    async fn canonical_request_response_and_prefinish_edits_reach_native_driver() {
        // Assertions live at the genuine model boundary, not in a production spy.
        struct CheckingModel {
            expected: Item,
            output: Item,
        }
        struct CheckingSession {
            expected: Item,
            output: Item,
        }
        #[async_trait]
        impl ModelAdapter for CheckingModel {
            type Session = CheckingSession;
            async fn start_session(&self, _: SessionConfig) -> Result<Self::Session, LoopError> {
                Ok(CheckingSession {
                    expected: self.expected.clone(),
                    output: self.output.clone(),
                })
            }
        }
        #[async_trait]
        impl ModelSession for CheckingSession {
            type Turn = Stream;
            async fn begin_turn(
                &mut self,
                request: TurnRequest,
                _: Option<TurnCancellation>,
            ) -> Result<Stream, LoopError> {
                assert_eq!(request.transcript.len(), 1);
                let mut expected = self.expected.clone();
                // The driver assigns append accounting before model dispatch.
                expected.created_at = request.transcript[0].created_at;
                assert!(expected.created_at.is_some());
                assert_eq!(request.transcript, vec![expected]);
                Ok(Stream {
                    events: VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                        finish_reason: FinishReason::Completed,
                        output_items: vec![self.output.clone()],
                        usage: None,
                        metadata: MetadataMap::new(),
                        model: Some("test-model".into()),
                        response_id: None,
                    })]),
                })
            }
        }

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = axum::Router::new().route(
            "/hooks",
            axum::routing::post(move |axum::Json(message): axum::Json<Value>| {
                let sender = sender.clone();
                async move {
                    assert!(agenthooksprotocol::generated::parse_intercept_request_value(message.clone()).is_ok());
                    let event = &message["params"]["event"];
                    let (target, role, before, after) = match event["type"].as_str().unwrap() {
                        "model.request.before" => ("request", "user", "original prompt", "edited prompt"),
                        "model.response.after" => ("response", "assistant", "original answer", "response edit"),
                        "turn.finish.before" => ("response", "assistant", "response edit", "prefinish edit"),
                        other => panic!("unexpected intercept: {other}"),
                    };
                    // Copy canonical messages, preserving both native message and part IDs.
                    let mut replacement = event["items"].clone();
                    let item = replacement.as_array_mut().unwrap().iter_mut()
                        .find(|item| item["role"] == role).unwrap();
                    let native_id = if role == "user" { "native-user" } else { "native-answer" };
                    assert_eq!(item["id"], native_id);
                    assert_eq!(item["parts"][0]["id"], format!("{native_id}/part/0"));
                    assert_eq!(item["parts"][0]["text"], before);
                    item["parts"][0]["text"] = json!(after);
                    sender.send(event.clone()).unwrap();
                    axum::Json(json!({"jsonrpc":"2.0","id":message["id"],"result":{
                        "protocolVersion":"draft","effects":[{
                            "type":"modify","target":target,"operation":"replace","value":replacement
                        }]
                    }}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let names = [
            "model.request.before",
            "model.response.after",
            "turn.finish.before",
        ];
        let capabilities = supported();
        let grants = names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    json!({"modes":["intercept"],"capabilities":capabilities[*name].as_value()}),
                )
            })
            .collect();
        let host = AhpHost::new_with_options(
            AhpConfig {
                registration: json!({"protocolVersion":"draft","hooks":[{
                    "id":"org.kit.native-edit-regression","transport":{"type":"http","url":endpoint},
                    "subscriptions":[{"events":names,"mode":"intercept","timeoutMs":5000,
                        "failurePolicy":"fail-closed","content":{"default":"body"}}]
                }]}),
                grants,
            },
            crate::ahp::SessionFacts {
                trigger: "startup".into(), permission_mode: "allow-all".into(), resumed_from: None,
            },
            agenthooksprotocol::adapters::registered::BackendOptions {
                allow_loopback_http: true, ..Default::default()
            },
        ).unwrap();
        let mut input = Item::text(ItemKind::User, "original prompt");
        input.id = Some("native-user".into());
        input
            .metadata
            .insert("native-context".into(), json!("preserved"));
        let mut expected = input.clone();
        expected.parts = vec![agentkit_core::Part::text("edited prompt")];
        let mut output = Item::text(ItemKind::Assistant, "original answer");
        output.id = Some("native-answer".into());
        output.usage = Some(agentkit_core::Usage::default());
        output.finish_reason = Some(FinishReason::Completed);
        output
            .metadata
            .insert("native-accounting".into(), json!("preserved"));
        let mut expected_output = output.clone();
        expected_output.parts = vec![agentkit_core::Part::text("prefinish edit")];
        let agent = Agent::builder()
            .model(HookedAdapter::new(
                CheckingModel { expected, output },
                Some(host.clone()),
            ))
            .mutator(HostMutator::new(host.clone()))
            .delivery(HostDelivery::new(host.clone()))
            .build()
            .unwrap();
        let mut driver = agent
            .start(SessionConfig::new("native-edit-regression"))
            .await
            .unwrap();
        let input = host
            .inbound("native-edit-regression", vec![input], None)
            .await
            .unwrap();
        driver.submit_input(input).unwrap();
        assert!(matches!(
            driver.next().await.unwrap(),
            LoopStep::Finished(_)
        ));
        let snapshot = driver.snapshot();
        let committed = snapshot
            .transcript
            .iter()
            .find(|item| item.kind == ItemKind::Assistant)
            .unwrap();
        // Commit supplies the assistant timestamp after editable hooks settle.
        expected_output.created_at = committed.created_at;
        assert!(expected_output.created_at.is_some());
        assert_eq!(committed, &expected_output);
        let observed: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|event| event["type"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(observed, names);
        crate::ahp::close_session(
            &mut driver,
            Some(&host),
            "native-edit-regression",
            SessionOutcome::Completed,
        )
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn canonical_loader_full_grants_traverse_native_lifecycle() {
        // Isolate the home registration without changing process-global HOME.
        // The only route targets an uninvoked tool, so every lifecycle boundary
        // must validate its full host authority even without a subscriber.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".agents")).unwrap();
        std::fs::write(root.path().join(".agents/hooks.json"), serde_json::to_vec(&json!({
            "protocolVersion":"draft", "hooks":[{
                "id":"org.kit.full-grants", "transport":{"type":"http","url":"https://example.invalid/hooks"},
                "subscriptions":[{"events":["tool.before"],"mode":"intercept","timeoutMs":1000,"failurePolicy":"fail-closed","content":{"default":"metadata"}}]
            }]
        })).unwrap()).unwrap();
        let config = crate::ahp::config::load_with_home(None, root.path())
            .unwrap()
            .unwrap();
        assert!(
            config.grants["turn.start"]["modes"]
                .as_array()
                .unwrap()
                .contains(&json!("intercept"))
        );
        assert_eq!(
            config.grants["turn.start"]["capabilities"]["modify"]["prompt"]["merge"],
            true
        );
        for fail in [false, true] {
            let host = AhpHost::new(config.clone()).unwrap();
            let session = "full-grants-session";
            let agent = agent(&host, fail);
            let mut driver = agent.start(SessionConfig::new(session)).await.unwrap();
            let input = host
                .inbound(session, vec![Item::text(ItemKind::User, "hello")], None)
                .await
                .unwrap();
            driver.submit_input(input).unwrap();
            let result = driver.next().await;
            if fail {
                assert!(
                    matches!(result, Err(LoopError::Provider(ref error)) if error == "test failure"),
                    "unexpected lifecycle result: {result:?}"
                );
            } else {
                assert!(matches!(result.unwrap(), LoopStep::Finished(_)));
                assert!(
                    driver
                        .snapshot()
                        .transcript
                        .iter()
                        .any(|item| item.kind == ItemKind::Assistant)
                );
            }
            crate::ahp::close_session(
                &mut driver,
                Some(&host),
                session,
                if fail {
                    SessionOutcome::Error
                } else {
                    SessionOutcome::Completed
                },
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn native_cancellation_interrupts_pending_sdk_request_and_preserves_terminal_events() {
        use agentkit_core::CancellationController;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = axum::Router::new().route(
            "/hooks",
            axum::routing::post(move |axum::Json(message): axum::Json<Value>| {
                let sender = sender.clone();
                async move {
                    let intercept = message["method"] == "hooks/intercept";
                    sender.send(message).unwrap();
                    if intercept {
                        std::future::pending::<axum::http::StatusCode>().await
                    } else {
                        axum::http::StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let names: Vec<_> = supported().into_keys().collect();
        let mut grants: BTreeMap<_, _> = names
            .iter()
            .map(|name| (name.clone(), json!({"modes":["observe"]})))
            .collect();
        grants.insert("model.request.before".into(),json!({"modes":["observe","intercept"],"capabilities":{"effects":["modify"],"modify":{"request":{"replace":true,"merge":true}}}}));
        let host = AhpHost::new_with_options(AhpConfig {
            registration: json!({"protocolVersion":"draft","hooks":[{"id":"org.kit.lifecycle-cancel","transport":{"type":"http","url":endpoint},"subscriptions":[{"events":names,"mode":"observe","content":{"default":"metadata"}},{"events":["model.request.before"],"mode":"intercept","timeoutMs":30000,"failurePolicy":"fail-closed","content":{"default":"metadata"}}]}]}),
            grants,
        }, crate::ahp::SessionFacts { trigger:"startup".into(),permission_mode:"allow-all".into(),resumed_from:None },agenthooksprotocol::adapters::registered::BackendOptions {allow_loopback_http:true,..Default::default()}).unwrap();
        let controller = CancellationController::new();
        let session = "cancel-session";
        let agent = Agent::builder()
            .model(HookedAdapter::new(
                Model { fail: false },
                Some(host.clone()),
            ))
            .mutator(HostMutator::new(host.clone()))
            .delivery(HostDelivery::new(host.clone()))
            .cancellation(controller.handle())
            .build()
            .unwrap();
        let mut driver = agent.start(SessionConfig::new(session)).await.unwrap();
        let input = host
            .inbound(session, vec![Item::text(ItemKind::User, "hello")], None)
            .await
            .unwrap();
        driver.submit_input(input).unwrap();
        let owner = host.clone();
        let running = tokio::spawn(async move {
            let result = driver.next().await;
            // Cancellation still runs the owner's closure: terminal delivery is
            // uncancelled, so turn.end and session.end are never skipped.
            crate::ahp::close_session(
                &mut driver,
                Some(&owner),
                session,
                SessionOutcome::Cancelled,
            )
            .await
            .unwrap();
            result
        });
        // Synchronize on the actual external hook request, not timing or spies.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if receiver.recv().await.unwrap()["method"] == "hooks/intercept" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        controller.interrupt();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err()
                || matches!(
                    result,
                    Ok(LoopStep::Finished(TurnResult {
                        finish_reason: FinishReason::Cancelled,
                        ..
                    }))
                )
        );
        let mut terminal = std::collections::BTreeSet::new();
        while let Ok(message) = receiver.try_recv() {
            if let Some(name) = message["params"]["event"]["type"].as_str() {
                terminal.insert(name.to_owned());
            }
        }
        assert!(terminal.contains("turn.end"));
        assert!(terminal.contains("session.end"));
        server.abort();
    }

    /// A suspended native boundary must not be escaped by dropping the driver
    /// call. Cancellation is signalled through the loop's own handle, the call
    /// is awaited, and the turn's terminal boundaries still arrive exactly
    /// once over committed output that is neither lost nor re-delivered.
    #[tokio::test]
    async fn cooperative_cancellation_completes_cleanup_without_duplicate_terminals() {
        use agentkit_core::CancellationController;
        for suspended in [None, Some("turn.progress"), Some("turn.finish.before")] {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let hold = Arc::new(std::sync::atomic::AtomicBool::new(suspended.is_some()));
            let target = suspended.unwrap_or_default().to_owned();
            let gate = hold.clone();
            let app = axum::Router::new().route(
                "/hooks",
                axum::routing::post(move |axum::Json(message): axum::Json<Value>| {
                    let sender = sender.clone();
                    let gate = gate.clone();
                    let target = target.clone();
                    async move {
                        let kind = message["params"]["event"]["type"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        sender.send(message).unwrap();
                        // Suspend the first delivery of the target boundary
                        // only, so the terminal boundaries stay bounded.
                        if kind == target && gate.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            std::future::pending::<axum::http::StatusCode>().await
                        } else {
                            axum::http::StatusCode::NO_CONTENT
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/hooks", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let names: Vec<_> = supported().into_keys().collect();
            // The editable pre-finish needs intercept authority to suspend; the
            // read-only delivery of the same event settles through it too.
            let intercepted = suspended == Some("turn.finish.before");
            let observed: Vec<_> = names
                .iter()
                .filter(|name| !(intercepted && name.as_str() == "turn.finish.before"))
                .cloned()
                .collect();
            let mut grants: BTreeMap<_, _> = names
                .iter()
                .map(|name| (name.clone(), json!({"modes":["observe"]})))
                .collect();
            let mut subscriptions =
                vec![json!({"events":observed,"mode":"observe","content":{"default":"metadata"}})];
            if intercepted {
                grants.insert("turn.finish.before".into(), json!({"modes":["observe","intercept"],"capabilities":{"effects":["modify"],"modify":{"response":{"replace":true,"merge":true}}}}));
                subscriptions.push(json!({"events":["turn.finish.before"],"mode":"intercept","timeoutMs":30000,"failurePolicy":"fail-closed","content":{"default":"metadata"}}));
            }
            let host = AhpHost::new_with_options(
                AhpConfig {
                    registration: json!({"protocolVersion":"draft","hooks":[{"id":"org.kit.cooperative-cancel","transport":{"type":"http","url":endpoint},"subscriptions":subscriptions}]}),
                    grants,
                },
                crate::ahp::SessionFacts {
                    trigger: "startup".into(),
                    permission_mode: "allow-all".into(),
                    resumed_from: None,
                },
                agenthooksprotocol::adapters::registered::BackendOptions {
                    allow_loopback_http: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let controller = CancellationController::new();
            let session = "cooperative-cancel-session";
            let agent = Agent::builder()
                .model(HookedAdapter::new(
                    Model { fail: false },
                    Some(host.clone()),
                ))
                .mutator(HostMutator::new(host.clone()))
                .delivery(HostDelivery::new(host.clone()))
                .cancellation(controller.handle())
                .build()
                .unwrap();
            let mut driver = agent.start(SessionConfig::new(session)).await.unwrap();
            let input = host
                .inbound(session, vec![Item::text(ItemKind::User, "hello")], None)
                .await
                .unwrap();
            driver.submit_input(input).unwrap();
            let owner = host.clone();
            let running = tokio::spawn(async move {
                // The driver call is awaited, never dropped: the loop's own
                // cancellation ends the turn through its terminal funnel.
                let step = driver.next().await;
                let committed = driver.snapshot().transcript;
                crate::ahp::close_session(
                    &mut driver,
                    Some(&owner),
                    session,
                    SessionOutcome::Cancelled,
                )
                .await
                .unwrap();
                (step, committed)
            });
            let mut seen = Vec::new();
            if let Some(target) = suspended {
                // Interrupt only once the real boundary is actually suspended.
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let message = receiver.recv().await.unwrap();
                        let kind = message["params"]["event"]["type"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        seen.push(kind.clone());
                        if kind == target {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
                controller.interrupt();
            }
            let (step, committed) =
                tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("cooperative cancellation must finish its own cleanup")
                    .unwrap();
            while let Ok(message) = receiver.try_recv() {
                if let Some(kind) = message["params"]["event"]["type"].as_str() {
                    seen.push(kind.to_owned());
                }
            }
            let count = |name: &str| seen.iter().filter(|kind| *kind == name).count();
            assert_eq!(count("turn.end"), 1, "turn.end for {suspended:?}: {seen:?}");
            assert_eq!(
                count("session.end"),
                1,
                "session.end for {suspended:?}: {seen:?}"
            );
            if suspended.is_none() {
                // The finish candidate settled the editable pre-finish, so the
                // awaited read-only delivery must not emit a second one.
                assert!(matches!(step, Ok(LoopStep::Finished(_))));
                assert_eq!(
                    count("turn.finish.before"),
                    1,
                    "pre-finish for {suspended:?}: {seen:?}"
                );
            }
            // Committed output is neither lost nor re-delivered: the admitted
            // prompt appears once and no assistant item is duplicated.
            let text = |item: &Item| {
                item.parts
                    .iter()
                    .filter_map(|part| match part {
                        agentkit_core::Part::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>()
            };
            assert_eq!(
                committed
                    .iter()
                    .filter(|item| item.kind == ItemKind::User && text(item) == "hello")
                    .count(),
                1,
                "admitted prompt for {suspended:?}"
            );
            assert!(
                committed
                    .iter()
                    .filter(|item| item.kind == ItemKind::Assistant)
                    .count()
                    <= 1,
                "duplicated committed output for {suspended:?}: {committed:?}"
            );
            server.abort();
        }
    }
}

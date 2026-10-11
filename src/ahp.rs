//! Kit's native AHP consumer. The SDK owns registration, transports,
//! reduction, attachment verification and shutdown. Native session callbacks await
//! the SDK directly on their owning runtime.
mod config;
pub(crate) mod domain;
mod lifecycle;
pub mod model;
mod projection;
#[cfg(test)]
mod tests;

pub use lifecycle::SessionOutcome;

use agenthooksprotocol::{
    Attachment,
    client::Decision,
    hooks::{Capabilities, EventGrant, Hooks, HooksOptions},
    inputs::{MessageInput, PartInput},
};
use std::{collections::BTreeMap, sync::Arc};

use crate::tools::ahp::{ToolBeforeOutcome, ToolEventIdentity, ToolHooks};
use agentkit_core::TurnCancellation;
use agentkit_loop::{
    DeliveryError, HookCtx, LoopCtx, LoopDriver, LoopError, LoopMutator, ModelRequestPayload,
    ModelResponsePayload, ModelSession, NativeDelivery, NativeFact, SessionStartPayload,
    TranscriptCursor,
};
use agentkit_tools_core::{
    PermissionCode, PermissionDenial, ToolError, ToolExecutionOutcome, ToolRequest,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

const BODY_LIMIT: usize = 4 * 1024 * 1024;
const ATTACHMENT_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const ATTACHMENT_OWNER_LIMIT: usize = 4096;

/// Canonical inline text slots for specialized compaction and elicitation.
const SPECIALIZED_REPLACEMENTS: [&str; 3] = ["/instructions", "/summary", "/elicitation/result"];

/// Canonical SDK registration and host-owned event grants.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AhpConfig {
    pub registration: Value,
    #[serde(default)]
    pub grants: BTreeMap<String, Value>,
}

fn fresh_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |v| v.as_nanos()),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

pub struct AhpHost {
    hooks: Arc<Hooks>,
    grants: BTreeMap<String, EventGrant>,
    facts: SessionFacts,
    source: String,
    selection: Option<Arc<dyn SessionSelection>>,
    model_calls: std::sync::atomic::AtomicU64,
    settled_prefinish: std::sync::Mutex<Option<String>>,
}

/// Kit's own session selection snapshot. The loop no longer names the model or
/// the provider at its hook boundaries, so these facts come from the Kit
/// adapter that actually owns the selection.
pub trait SessionSelection: Send + Sync {
    /// Identifier of the currently selected model, when one is known.
    fn model_id(&self) -> Option<String>;
    /// Identifier of the currently selected provider, when one is known.
    fn provider(&self) -> Option<String>;
}

pub(crate) struct Settled {
    pub event: Value,
    pub bodies: BTreeMap<String, Value>,
}

/// Why a native boundary did not settle. Cancellation is kept distinct from
/// failure: a boundary the SDK cancelled must finish its turn cancelled, not
/// report the turn as failed.
pub(crate) enum EventError {
    Cancelled,
    Failed(String),
}

impl EventError {
    fn rendered(self) -> String {
        match self {
            Self::Cancelled => "AHP boundary cancelled".to_owned(),
            Self::Failed(message) => message,
        }
    }

    fn into_loop_error(self) -> LoopError {
        match self {
            Self::Cancelled => LoopError::Cancelled,
            Self::Failed(message) => LoopError::Mutator(message),
        }
    }
}

impl From<String> for EventError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for EventError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("AHP boundary cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// Facts owned by Kit's native session construction, not inferred by the SDK.
#[derive(Clone, Debug)]
pub struct SessionFacts {
    pub trigger: String,
    pub permission_mode: String,
    pub resumed_from: Option<String>,
}

fn supported() -> BTreeMap<String, Capabilities> {
    let mut caps = lifecycle::supported();
    caps.extend(crate::compaction::supported_hooks());
    caps.extend(domain::supported_hooks());
    caps.insert(
        "tool.before".into(),
        Capabilities::none().allow().deny().modify_input(),
    );
    caps.insert(
        "tool.after".into(),
        Capabilities::none().modify("output", true, true),
    );
    caps
}

// Host-authority admission only: the SDK still validates the complete canonical
// declaration and applies its event-specific capability and effect contracts.
fn subset(requested: &Value, supported: &Value) -> bool {
    match requested {
        Value::Object(map) => map
            .iter()
            .all(|(key, value)| supported.get(key).is_some_and(|allow| subset(value, allow))),
        Value::Array(values) => supported
            .as_array()
            .is_some_and(|allow| values.iter().all(|v| allow.contains(v))),
        Value::Bool(false) => supported.is_boolean(),
        _ => requested == supported,
    }
}

impl AhpHost {
    pub fn new(config: AhpConfig) -> Result<Arc<Self>, String> {
        Self::new_with_facts(
            config,
            SessionFacts {
                trigger: "startup".into(),
                permission_mode: "unknown".into(),
                resumed_from: None,
            },
        )
    }
    pub fn new_with_facts(config: AhpConfig, facts: SessionFacts) -> Result<Arc<Self>, String> {
        Self::new_with_options(
            config,
            facts,
            agenthooksprotocol::adapters::registered::BackendOptions::default(),
        )
    }

    pub fn new_with_options(
        config: AhpConfig,
        facts: SessionFacts,
        backend: agenthooksprotocol::adapters::registered::BackendOptions,
    ) -> Result<Arc<Self>, String> {
        Self::new_with_selection(config, facts, None, backend)
    }

    pub fn new_with_selection(
        config: AhpConfig,
        facts: SessionFacts,
        selection: Option<Arc<dyn SessionSelection>>,
        backend: agenthooksprotocol::adapters::registered::BackendOptions,
    ) -> Result<Arc<Self>, String> {
        let supported = supported();
        let mut grants = BTreeMap::new();
        for (name, declaration) in config.grants {
            let supported = supported
                .get(&name)
                .ok_or_else(|| format!("Kit does not support AHP event {name}"))?;
            let grant = EventGrant::from_declaration(&declaration).map_err(|e| e.to_string())?;
            if let Some(caps) = declaration.get("capabilities")
                && !subset(caps, supported.as_value())
            {
                return Err(format!(
                    "Kit does not consume the requested AHP capabilities for {name}"
                ));
            }
            grants.insert(name, grant);
        }
        let source = format!("urn:kit:ahp:{}", fresh_id());
        let mut options = HooksOptions::new(source.clone(), grants.clone());
        options.backend = backend;
        options.backend.max_frame_bytes = BODY_LIMIT;
        // Kit's own attachment bounds, declared rather than inherited. The body
        // limit applies to binary originals and every source read; inline text is
        // bounded by the same host ceiling without attachment staging. The active
        // byte and owner budgets are shared by concurrent dispatches.
        options.max_body_bytes = BODY_LIMIT;
        options.max_stored_bytes = ATTACHMENT_BYTE_LIMIT;
        options.max_stored_entries = ATTACHMENT_OWNER_LIMIT;
        let hooks = Hooks::new(config.registration, options).map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            hooks: Arc::new(hooks),
            grants,
            facts,
            source,
            selection,
            model_calls: std::sync::atomic::AtomicU64::new(0),
            settled_prefinish: std::sync::Mutex::new(None),
        }))
    }

    fn selection_snapshot(&self) -> (String, String) {
        let unknown = || "unknown".to_owned();
        let model = self
            .selection
            .as_ref()
            .and_then(|selection| selection.model_id())
            .unwrap_or_else(unknown);
        let provider = self
            .selection
            .as_ref()
            .and_then(|selection| selection.provider())
            .unwrap_or_else(unknown);
        (model, provider)
    }

    /// Opens a new session-local model call and returns its index.
    fn begin_model_call(&self) -> u64 {
        self.model_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    fn model_call_index(&self) -> u64 {
        self.model_calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Records that a turn's editable pre-finish boundary already settled, so
    /// the awaited read-only delivery does not emit a second one.
    fn settle_prefinish(&self, turn: String) {
        if let Ok(mut settled) = self.settled_prefinish.lock() {
            *settled = Some(turn);
        }
    }

    fn take_settled_prefinish(&self, turn: &str) -> bool {
        let Ok(mut settled) = self.settled_prefinish.lock() else {
            return false;
        };
        if settled.as_deref() == Some(turn) {
            *settled = None;
            return true;
        }
        false
    }

    pub(crate) async fn event(
        &self,
        event: Value,
        bodies: Vec<(String, Value)>,
        targets: Vec<(String, String)>,
    ) -> Result<Settled, String> {
        self.event_with_cancellation(event, bodies, targets, None)
            .await
    }

    pub(crate) async fn event_with_cancellation(
        &self,
        event: Value,
        bodies: Vec<(String, Value)>,
        targets: Vec<(String, String)>,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Settled, String> {
        self.settle(event, bodies, targets, cancellation)
            .await
            .map_err(EventError::rendered)
    }

    /// Settles a boundary that belongs to a loop operation, so a cancelled
    /// boundary ends its turn through the loop's own cancellation path instead
    /// of being reported as a mutator failure.
    pub(crate) async fn lifecycle_event(
        &self,
        event: Value,
        bodies: Vec<(String, Value)>,
        targets: Vec<(String, String)>,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Settled, LoopError> {
        self.settle(event, bodies, targets, cancellation)
            .await
            .map_err(EventError::into_loop_error)
    }

    async fn settle(
        &self,
        mut event: Value,
        bodies: Vec<(String, Value)>,
        targets: Vec<(String, String)>,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Settled, EventError> {
        if event["type"] == "session.start" {
            event["trigger"] = Value::from((self.facts.trigger).to_owned());
            event["permissionMode"] = Value::from((self.facts.permission_mode).to_owned());
            if let Some(id) = &self.facts.resumed_from {
                event["resumedFrom"] = Value::Object(serde_json::Map::from_iter([(
                    "id".to_owned(),
                    Value::from((id).to_owned()),
                )]));
            }
            if let Some(gaps) = event.get_mut("gaps").and_then(Value::as_array_mut) {
                gaps.retain(|gap| gap["path"] != "/trigger" && gap["path"] != "/permissionMode");
            }
        }
        dispatch_event(
            &self.hooks,
            &self.grants,
            event,
            bodies,
            targets,
            cancellation,
        )
        .await
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.hooks
            .shutdown()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Native snapshot and its canonical inline message or text-parts slot.
struct ContentSlot {
    pointer: String,
    canonical: String,
    original: Value,
    text: bool,
}

/// Native binary source waiting to enter a schema-owned SDK slot.
struct BinarySource {
    path: Vec<String>,
    source: Attachment,
}

fn binding(pointer: &str, source: Attachment) -> Result<BinarySource, String> {
    let path = pointer
        .strip_prefix('/')
        .ok_or_else(|| format!("AHP content slot {pointer} is not a JSON pointer"))?
        .split('/')
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect();
    Ok(BinarySource { path, source })
}

/// Bound inline content without encoding it into an attachment or allocating a
/// second serialized body. Binary bytes are bounded separately by the SDK.
fn inline_bytes(value: &Value) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .fold(0usize, |sum, value| sum.saturating_add(inline_bytes(value))),
        Value::Object(fields) if fields.get("kind").is_some_and(|kind| kind == "text") => fields
            .get("text")
            .and_then(Value::as_str)
            .map_or(0, str::len),
        Value::Object(fields) => fields
            .values()
            .fold(0usize, |sum, value| sum.saturating_add(inline_bytes(value))),
        _ => 0,
    }
}
fn check_inline_limit(value: &Value) -> Result<(), String> {
    if inline_bytes(value) > BODY_LIMIT {
        Err("AHP inline content exceeds 4 MiB".into())
    } else {
        Ok(())
    }
}

fn owned_parts(
    parts: impl IntoIterator<Item = Box<agenthooksprotocol::ContentItem>>,
    path: &[String],
    sources: &mut Vec<BinarySource>,
) -> Result<Vec<PartInput<Attachment>>, String> {
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let mut child = path.to_vec();
            child.push(index.to_string());
            if let Some(position) = sources.iter().position(|source| source.path == child) {
                let source = sources.remove(position).source;
                let metadata = match *part {
                    agenthooksprotocol::ContentItem::AttachmentMetadata(metadata) => {
                        let mut body = agenthooksprotocol::AttachmentBodyPart::new(
                            agenthooksprotocol::ContentReference::new("ahp:host-pending"),
                            metadata.id,
                            metadata.media_type,
                        );
                        body.category = metadata.category;
                        body.synthesized = metadata.synthesized;
                        body.additional_properties = metadata.additional_properties;
                        // SDK-owned body references carry the verified size and digest.
                        // Metadata selection's top-level size/hash are not body fields.
                        body
                    }
                    agenthooksprotocol::ContentItem::AttachmentBodyPart(metadata) => *metadata,
                    _ => return Err("binary source does not address an attachment".into()),
                };
                Ok(PartInput::attachment(metadata, source))
            } else {
                Ok(PartInput::Wire(*part))
            }
        })
        .collect()
}

fn owned_messages(
    messages: impl IntoIterator<Item = Box<agenthooksprotocol::ModelVisibleItem>>,
    path: &[String],
    sources: &mut Vec<BinarySource>,
) -> Result<Vec<MessageInput<Attachment>>, String> {
    messages
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            let message = message.0;
            let mut child = path.to_vec();
            child.extend([index.to_string(), "parts".to_owned()]);
            let parts = owned_parts(message.parts.clone(), &child, sources)?;
            Ok(MessageInput {
                metadata: *message,
                parts,
            })
        })
        .collect()
}

fn optional_parts(
    parts: Option<impl IntoIterator<Item = Box<agenthooksprotocol::ContentItem>>>,
    path: &[String],
    sources: &mut Vec<BinarySource>,
) -> Result<Vec<PartInput<Attachment>>, String> {
    owned_parts(parts.into_iter().flatten(), path, sources)
}

fn optional_messages(
    messages: Option<impl IntoIterator<Item = Box<agenthooksprotocol::ModelVisibleItem>>>,
    path: &[String],
    sources: &mut Vec<BinarySource>,
) -> Result<Vec<MessageInput<Attachment>>, String> {
    owned_messages(messages.into_iter().flatten(), path, sources)
}

/// SDK-owned canonical settlement is the authority at native projection.
/// Named inputs and raw receipts have different caller decode types; Kit does
/// not decode their raw views or rebuild a public SDK EventOutcome.
struct CanonicalSettlement {
    event: agenthooksprotocol::Event,
    diagnostics: Vec<agenthooksprotocol::client::DeliveryDiagnostic>,
    outcome: agenthooksprotocol::client::ConsumerProtocolOutcome,
}

async fn dispatch_typed(
    hooks: &Hooks,
    name: &str,
    event: Value,
    mut sources: Vec<BinarySource>,
    caps: Capabilities,
    cancellation: Option<TurnCancellation>,
) -> Result<CanonicalSettlement, EventError> {
    macro_rules! settle {
        ($boundary:expr) => {{
            let mut boundary = $boundary.initial_state(Decision::Allow).capabilities(caps);
            if let Some(cancellation) = cancellation {
                boundary = boundary.cancel_when(async move { cancellation.cancelled().await });
            }
            let settled = boundary.await.map_err(|error| {
                if error.code() == agenthooksprotocol::DeliveryDiagnosticCode::Cancelled {
                    EventError::Cancelled
                } else {
                    EventError::Failed(error.to_string())
                }
            })?;
            CanonicalSettlement {
                event: settled.settled_event,
                diagnostics: settled.diagnostics,
                outcome: settled.outcome,
            }
        }};
    }
    macro_rules! input {
        ($ty:ident, $method:ident, $kind:ident) => {{
            let facts: agenthooksprotocol::inputs::$ty =
                serde_json::from_value(event).map_err(|e| e.to_string())?;
            let items = facts.items.clone();
            let items = $kind(items, &["items".to_owned()], &mut sources)?;
            let input = facts.with_sources().with_items(items);
            if !sources.is_empty() {
                return Err("unbound native binary attachment".into());
            }
            settle!(hooks.$method(input))
        }};
    }
    let settled = match name {
        "config.change.after" => {
            input!(ConfigChangeAfterInput, config_change_after, optional_parts)
        }
        "config.change.before" => input!(
            ConfigChangeBeforeInput,
            config_change_before,
            optional_parts
        ),
        "context.compact.after" => input!(
            ContextCompactAfterInput,
            context_compact_after,
            optional_messages
        ),
        "context.compact.before" => input!(
            ContextCompactBeforeInput,
            context_compact_before,
            owned_messages
        ),
        "file.changed" => input!(FileChangedInput, file_changed, optional_parts),
        "model.error" => input!(ModelErrorInput, model_error, optional_parts),
        "model.request.before" => input!(
            ModelRequestBeforeInput,
            model_request_before,
            owned_messages
        ),
        "model.response.after" => input!(
            ModelResponseAfterInput,
            model_response_after,
            owned_messages
        ),
        "model.switch.after" => input!(ModelSwitchAfterInput, model_switch_after, optional_parts),
        "model.switch.before" => {
            input!(ModelSwitchBeforeInput, model_switch_before, optional_parts)
        }
        "session.end" => input!(SessionEndInput, session_end, optional_parts),
        "session.start" => input!(SessionStartInput, session_start, owned_messages),
        "turn.end" => input!(TurnEndInput, turn_end, owned_messages),
        "turn.finish.before" => input!(TurnFinishBeforeInput, turn_finish_before, owned_messages),
        "turn.start" => input!(TurnStartInput, turn_start, owned_messages),
        "user.message.inbound" => {
            let facts: agenthooksprotocol::inputs::UserMessageInboundInput =
                serde_json::from_value(event).map_err(|e| e.to_string())?;
            let messages = facts.message.messages.iter().map(|message| {
                let mut canonical = agenthooksprotocol::CanonicalMessage::new(
                    message.id.clone(),
                    message.parts.clone(),
                    agenthooksprotocol::CanonicalMessageRole::User,
                );
                canonical.synthesized = message.synthesized.clone();
                canonical.additional_properties = message.additional_properties.clone();
                Box::new(agenthooksprotocol::ModelVisibleItem(Box::new(canonical)))
            });
            let messages = owned_messages(
                messages,
                &["message".into(), "messages".into()],
                &mut sources,
            )?;
            let input = facts.with_sources().with_message_messages(messages);
            if !sources.is_empty() {
                return Err("unbound native binary attachment".into());
            }
            settle!(hooks.user_message_inbound(input))
        }
        "turn.progress" => {
            let facts: agenthooksprotocol::inputs::TurnProgressInput =
                serde_json::from_value(event).map_err(|e| e.to_string())?;
            let metadata = facts.delta.0.clone();
            let parts = owned_parts(
                metadata.parts.clone(),
                &["delta".into(), "parts".into()],
                &mut sources,
            )?;
            let input = facts.with_sources().with_delta(MessageInput {
                metadata: *metadata,
                parts,
            });
            if !sources.is_empty() {
                return Err("unbound native binary attachment".into());
            }
            settle!(hooks.turn_progress(input))
        }
        "tool.before" | "tool.after" => {
            if !sources.is_empty() {
                return Err("unbound native binary attachment".into());
            }
            // There is no named receipt boundary on Hooks. Its public complete-
            // event boundary hydrates SDK-owned id/source/time before canonical
            // effective-event decoding, just as named inputs do.
            settle!(hooks.event(event))
        }
        _ => return Err("unsupported native AHP event".into()),
    };
    Ok(settled)
}

async fn dispatch_event(
    hooks: &Hooks,
    grants: &BTreeMap<String, EventGrant>,
    mut event: Value,
    bodies: Vec<(String, Value)>,
    targets: Vec<(String, String)>,
    cancellation: Option<TurnCancellation>,
) -> Result<Settled, EventError> {
    let name = event["type"].as_str().ok_or("missing AHP event type")?;
    if !grants.contains_key(name) {
        return Ok(Settled {
            event,
            bodies: bodies.into_iter().collect(),
        });
    }
    let name = name.to_owned();
    // Ordinary native values become canonical inline messages, never attachment
    // envelopes. Keep the native snapshot only for validated effective writeback.
    let mut slots = Vec::with_capacity(bodies.len());
    let mut bindings = Vec::new();
    for (pointer, value) in bodies {
        let text = SPECIALIZED_REPLACEMENTS.contains(&pointer.as_str());
        let canonical = if pointer == "/items/0" {
            "/items".to_owned()
        } else {
            pointer.clone()
        };
        let role = event
            .pointer(&pointer)
            .and_then(|v| v.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("user");
        let projected = if pointer == "/delta" {
            let native = event
                .pointer(&pointer)
                .ok_or("missing native delta identity")?;
            let parts = if let Some(text) = value
                .get("AppendText")
                .and_then(|delta| delta.get("chunk"))
                .and_then(Value::as_str)
            {
                projection::text_parts(text)
            } else if let Some(structured) = value
                .get("ReplaceStructured")
                .and_then(|delta| delta.get("value"))
            {
                projection::text_parts(
                    &serde_json::to_string(structured).map_err(|e| e.to_string())?,
                )
            } else if let Some(output) = value.get("output") {
                let messages = projection::messages(output, "tool")?;
                messages[0]["parts"].clone()
            } else {
                Value::Array(vec![Value::Object(serde_json::Map::from_iter([
                    ("id".into(), Value::from("kit-delta-part")),
                    ("kind".into(), Value::from("text")),
                    ("mediaType".into(), Value::from("text/plain")),
                    ("selection".into(), Value::from("body")),
                    (
                        "gap".into(),
                        Value::Object(serde_json::Map::from_iter([(
                            "reason".into(),
                            Value::from(
                                "This native structural or binary delta has no complete inline content or resolved file owner",
                            ),
                        )])),
                    ),
                ]))])
            };
            Value::Object(serde_json::Map::from_iter([
                ("id".into(), native["id"].clone()),
                ("role".into(), native["role"].clone()),
                ("parts".into(), parts),
            ]))
        } else if text {
            projection::text_parts(value.as_str().ok_or("AHP text body must be a string")?)
        } else if value.is_string() {
            Value::Array(vec![Value::Object(serde_json::Map::from_iter([
                ("id".into(), Value::from("kit-message-0")),
                ("role".into(), Value::from("user")),
                (
                    "parts".into(),
                    projection::text_parts(value.as_str().ok_or("invalid text")?),
                ),
            ]))])
        } else {
            let projected = projection::messages(&value, role)?;
            // The published session.end schema retains a part list, unlike
            // other model-visible boundaries. Flatten only this read-only receipt.
            let messages = projected.as_array().ok_or("invalid canonical messages")?;
            for (relative, attachment) in projection::attachments(&value, role)? {
                let relative = if name == "session.end" {
                    let mut components = relative.trim_start_matches('/').split('/');
                    let message_index: usize = components
                        .next()
                        .ok_or("missing attachment message index")?
                        .parse()
                        .map_err(|_| "invalid attachment message index")?;
                    let _parts = components.next();
                    let part_index: usize = components
                        .next()
                        .ok_or("missing attachment part index")?
                        .parse()
                        .map_err(|_| "invalid attachment part index")?;
                    let offset = messages.iter().take(message_index).try_fold(
                        0usize,
                        |offset, message| {
                            message["parts"]
                                .as_array()
                                .map(|parts| offset.saturating_add(parts.len()))
                                .ok_or("invalid canonical parts")
                        },
                    )?;
                    format!("/{}", offset.saturating_add(part_index))
                } else {
                    relative
                };
                bindings.push(binding(&format!("{canonical}{relative}"), attachment)?);
            }
            if name == "session.end" {
                let mut parts = Vec::new();
                for message in messages {
                    parts.extend(
                        message["parts"]
                            .as_array()
                            .ok_or("invalid canonical parts")?
                            .iter()
                            .cloned(),
                    );
                }
                Value::Array(parts)
            } else {
                projected
            }
        };
        check_inline_limit(&projected)?;
        *event
            .pointer_mut(&canonical)
            .ok_or_else(|| format!("missing AHP inline slot {canonical}"))? = projected;
        slots.push(ContentSlot {
            pointer,
            canonical,
            original: value,
            text,
        });
    }
    // Static grants are ceilings. Receipts without editable output must not
    // advertise body operations; the SDK validates this per-boundary narrowing.
    let manifest = hooks.manifest();
    let mut caps = manifest["events"]
        .as_array()
        .and_then(|events| events.iter().find(|entry| entry["event"] == event["type"]))
        .and_then(|entry| entry.get("capabilities"))
        .cloned()
        .unwrap_or_else(|| {
            Value::Object(serde_json::Map::from_iter([(
                "effects".to_owned(),
                Value::Array(vec![]),
            )]))
        });
    if let Some(modify) = caps.get_mut("modify").and_then(Value::as_object_mut) {
        for target in ["prompt", "request", "response", "output", "content"] {
            if let Some((_, pointer)) = targets.iter().find(|(name, _)| name == target) {
                // Native item arrays and text are replacement-only bodies.
                // The configured grant is a ceiling, not evidence that this
                // particular native value supports an object merge. The SDK
                // still validates the body and settles all requested effects.
                if slots.iter().any(|slot| {
                    &slot.pointer == pointer
                        && !slot.original.is_object()
                        && !slot.original.is_array()
                }) && let Some(operations) = modify.get_mut(target)
                {
                    operations["merge"] = Value::Bool(false);
                    if operations["replace"] != Value::Bool(true) {
                        modify.remove(target);
                    }
                }
            } else {
                modify.remove(target);
            }
        }
        if modify.is_empty() {
            caps.as_object_mut()
                .ok_or("invalid AHP capability object")?
                .remove("modify");
            if let Some(effects) = caps["effects"].as_array_mut() {
                effects.retain(|effect| effect != "modify");
            }
        }
    }
    let caps = Capabilities::from_value(caps).map_err(|e| e.to_string())?;
    let settled = dispatch_typed(hooks, &name, event, bindings, caps, cancellation).await?;
    // Observation preparation and delivery diagnostics, including the deadline
    // and cancellation entries the SDK records before a notification is ever
    // delivered, are reported rather than swallowed by a settled dispatch.
    for diagnostic in &settled.diagnostics {
        tracing::warn!(
            event = %name,
            code = ?diagnostic.code,
            stage = ?diagnostic.stage,
            subscription = %diagnostic.subscription_id,
            backend = ?diagnostic.backend_id,
            policy = ?diagnostic.policy,
            synthetic_denial = diagnostic.synthetic_denial,
            "AHP delivery diagnostic"
        );
    }
    if settled.outcome.is_denied() || settled.outcome.stopped {
        return Err("AHP denied the lifecycle operation".into());
    }
    // Serialize only at the native projection boundary, not to assemble SDK inputs.
    let event = serde_json::to_value(settled.event).map_err(|e| e.to_string())?;
    let mut bodies = BTreeMap::new();
    for slot in slots {
        let effective = event
            .pointer(&slot.canonical)
            .ok_or_else(|| format!("effective AHP event removed {}", slot.canonical))?;
        check_inline_limit(effective)?;
        let value = if name == "session.end" || slot.pointer == "/delta" {
            slot.original
        } else if slot.text {
            Value::String(projection::restore_text(effective)?)
        } else if slot.original.is_string() {
            Value::String(projection::restore_text(&effective[0]["parts"])?)
        } else {
            projection::restore(&slot.original, effective)?
        };
        bodies.insert(slot.pointer, value);
    }
    Ok(Settled { event, bodies })
}

fn tool_event(kind: &str, request: &ToolRequest) -> Value {
    let mut tool = Value::Object(serde_json::Map::from_iter([
        (
            "name".to_owned(),
            Value::from((request.tool_name.to_string()).to_owned()),
        ),
        ("input".to_owned(), request.input.clone()),
        ("origin".to_owned(), Value::from(("native").to_owned())),
    ]));
    if let Some(mcp) = request.metadata.get("kit.ahp.mcp") {
        tool["origin"] = Value::from(("mcp").to_owned());
        tool["mcp"] = mcp.clone();
    }
    Value::Object(serde_json::Map::from_iter([
        ("type".to_owned(), Value::from((kind).to_owned())),
        (
            "session".to_owned(),
            Value::Object(serde_json::Map::from_iter([(
                "id".to_owned(),
                Value::from((request.session_id.to_string()).to_owned()),
            )])),
        ),
        (
            "turn".to_owned(),
            Value::Object(serde_json::Map::from_iter([(
                "id".to_owned(),
                Value::from((request.turn_id.to_string()).to_owned()),
            )])),
        ),
        (
            "call".to_owned(),
            Value::Object(serde_json::Map::from_iter([(
                "id".to_owned(),
                Value::from((request.call_id.to_string()).to_owned()),
            )])),
        ),
        ("tool".to_owned(), tool),
        (
            "path".to_owned(),
            Value::from(("native-executor").to_owned()),
        ),
    ]))
}

fn failed(message: impl Into<String>) -> ToolExecutionOutcome {
    ToolExecutionOutcome::FailedBeforeInvocation(ToolError::ExecutionFailed(message.into()))
}

async fn dispatch_before(
    hooks: &Hooks,
    grants: &BTreeMap<String, EventGrant>,
    source: &str,
    mut request: ToolRequest,
    parent: Option<ToolEventIdentity>,
    cancellation: Option<TurnCancellation>,
    contract: Option<&crate::tools::ahp::ToolArgumentContract>,
) -> ToolBeforeOutcome {
    let mut event = tool_event("tool.before", &request);
    let fallback = ToolEventIdentity {
        source: source.to_owned(),
        id: format!("kit-event-{}", fresh_id()),
    };
    event["id"] = Value::from((fallback.id).to_owned());
    event["source"] = Value::from((source).to_owned());
    if let Some(parent) = &parent
        && parent.source == source
        && !parent.id.is_empty()
    {
        event["parentEventId"] = Value::from(parent.id.clone());
    }
    if !grants.contains_key("tool.before") {
        return ToolBeforeOutcome::Invoke {
            request,
            event: parent.unwrap_or(ToolEventIdentity {
                source: source.to_owned(),
                id: String::new(),
            }),
        };
    }
    let fallback_contract = crate::tools::ahp::ToolArgumentContract::new::<Value>();
    let result = contract
        .unwrap_or(&fallback_contract)
        .settle(hooks, request.input.clone(), event, cancellation)
        .await;
    match result {
        Err(error) => ToolBeforeOutcome::Complete(
            if error.code() == agenthooksprotocol::generated::DeliveryDiagnosticCode::Cancelled {
                ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Cancelled)
            } else {
                failed(error.to_string())
            },
        ),
        Ok(settled) => {
            for diagnostic in &settled.diagnostics {
                tracing::warn!(
                    event = "tool.before",
                    code = ?diagnostic.code,
                    stage = ?diagnostic.stage,
                    subscription = %diagnostic.subscription_id,
                    backend = ?diagnostic.backend_id,
                    policy = ?diagnostic.policy,
                    synthetic_denial = diagnostic.synthetic_denial,
                    "AHP delivery diagnostic"
                );
            }
            let agenthooksprotocol::Event::ToolBefore(event) = settled.settled_event else {
                return ToolBeforeOutcome::Complete(failed("AHP settled a non-tool event"));
            };
            let identity = ToolEventIdentity {
                source: event.source,
                id: event.id,
            };
            if settled.outcome.is_denied() || settled.outcome.stopped {
                request.input = settled.effective_input;
                let mut receipt = tool_event("tool.after", &request);
                receipt["parentEventId"] = Value::from(identity.id);
                receipt["items"] = Value::Array(vec![]);
                receipt["outcome"] = Value::from(("denied").to_owned());
                receipt["execution"] = Value::Object(serde_json::Map::from_iter([
                    ("status".to_owned(), Value::from(("skipped").to_owned())),
                    ("reason".to_owned(), Value::from(("policy").to_owned())),
                    (
                        "detail".to_owned(),
                        Value::from(("AHP denied tool invocation").to_owned()),
                    ),
                ]));
                // Settlement denial is a real skipped invocation. Preparation
                // errors above do not fabricate a policy/execution receipt.
                if let Err(error) =
                    dispatch_event(hooks, grants, receipt, vec![], vec![], None).await
                {
                    tracing::warn!(%error,"AHP skipped tool receipt failed");
                }
                return ToolBeforeOutcome::Complete(ToolExecutionOutcome::FailedBeforeInvocation(
                    ToolError::PermissionDenied(PermissionDenial {
                        code: PermissionCode::CustomPolicyDenied,
                        message: "AHP denied tool invocation".into(),
                        metadata: request.metadata.clone(),
                    }),
                ));
            }
            match settled.input {
                Ok(input) => {
                    request.input = input;
                    ToolBeforeOutcome::Invoke {
                        request,
                        event: identity,
                    }
                }
                Err(error) => ToolBeforeOutcome::Complete(failed(error.to_string())),
            }
        }
    }
}

#[async_trait]
impl ToolHooks for AhpHost {
    async fn before(
        &self,
        request: ToolRequest,
        parent: Option<ToolEventIdentity>,
        cancellation: Option<TurnCancellation>,
    ) -> ToolBeforeOutcome {
        dispatch_before(
            &self.hooks,
            &self.grants,
            &self.source,
            request,
            parent,
            cancellation,
            None,
        )
        .await
    }

    async fn before_with_contract(
        &self,
        request: ToolRequest,
        parent: Option<ToolEventIdentity>,
        cancellation: Option<TurnCancellation>,
        contract: Option<&crate::tools::ahp::ToolArgumentContract>,
    ) -> ToolBeforeOutcome {
        dispatch_before(
            &self.hooks,
            &self.grants,
            &self.source,
            request,
            parent,
            cancellation,
            contract,
        )
        .await
    }

    async fn after(
        &self,
        request: &ToolRequest,
        event: ToolEventIdentity,
        mut outcome: ToolExecutionOutcome,
        _cancellation: Option<TurnCancellation>,
    ) -> ToolExecutionOutcome {
        let mut after = tool_event("tool.after", request);
        if event.source == self.source && !event.id.is_empty() {
            after["parentEventId"] = Value::from((event.id).to_owned());
        }
        after["items"] = Value::Array(vec![]);
        let mut bodies = vec![];
        let mut targets = vec![];
        match &outcome {
            ToolExecutionOutcome::Completed(result) => {
                after["outcome"] = Value::from(
                    (if result.result.is_error {
                        "error"
                    } else {
                        "ok"
                    })
                    .to_owned(),
                );
                after["execution"] = Value::Object(serde_json::Map::from_iter([(
                    "status".to_owned(),
                    Value::from(("executed").to_owned()),
                )]));
                if result.result.is_error {
                    after["error"] = Value::Object(serde_json::Map::from_iter([
                        (
                            "message".to_owned(),
                            Value::from(("tool returned an error result").to_owned()),
                        ),
                        ("class".to_owned(), Value::from(("tool").to_owned())),
                    ]));
                }
                after["items"] = Value::Array(vec![Value::Object(serde_json::Map::from_iter([(
                    "role".to_owned(),
                    Value::from(("tool").to_owned()),
                )]))]);
                match serde_json::to_value(&result.result.output) {
                    Ok(output) => bodies.push(("/items/0".into(), output)),
                    Err(e) => {
                        return ToolExecutionOutcome::Failed(ToolError::Internal(e.to_string()));
                    }
                }
                targets.push(("output".into(), "/items/0".into()));
            }
            ToolExecutionOutcome::FailedBeforeInvocation(ToolError::PermissionDenied(_)) => {
                after["outcome"] = Value::from("denied");
                after["execution"] = Value::Object(serde_json::Map::from_iter([
                    ("status".into(), Value::from("skipped")),
                    ("reason".into(), Value::from("policy")),
                ]));
            }
            ToolExecutionOutcome::FailedBeforeInvocation(ToolError::Cancelled) => {
                after["outcome"] = Value::from("cancelled");
                after["execution"] = Value::Object(serde_json::Map::from_iter([
                    ("status".into(), Value::from("skipped")),
                    ("reason".into(), Value::from("cancelled")),
                ]));
            }
            ToolExecutionOutcome::Failed(error)
            | ToolExecutionOutcome::FailedBeforeInvocation(error) => {
                after["outcome"] = Value::from(
                    (if matches!(error, ToolError::Cancelled) {
                        "cancelled"
                    } else {
                        "error"
                    })
                    .to_owned(),
                );
                after["execution"] =
                    if matches!(outcome, ToolExecutionOutcome::FailedBeforeInvocation(_)) {
                        Value::Object(serde_json::Map::from_iter([
                            ("status".to_owned(), Value::from(("skipped").to_owned())),
                            ("reason".to_owned(), Value::from(("other").to_owned())),
                            (
                                "detail".to_owned(),
                                Value::from((error.to_string()).to_owned()),
                            ),
                        ]))
                    } else {
                        Value::Object(serde_json::Map::from_iter([(
                            "status".to_owned(),
                            Value::from(("executed").to_owned()),
                        )]))
                    };
                after["error"] = Value::Object(serde_json::Map::from_iter([
                    (
                        "message".to_owned(),
                        Value::from((error.to_string()).to_owned()),
                    ),
                    ("class".to_owned(), Value::from(("tool").to_owned())),
                ]));
            }
            ToolExecutionOutcome::Interrupted(_) => {
                // Interruption is an execution receipt, not completed tool work.
                after["outcome"] = Value::from(("denied").to_owned());
                after["execution"] = Value::Object(serde_json::Map::from_iter([
                    ("status".to_owned(), Value::from(("skipped").to_owned())),
                    ("reason".to_owned(), Value::from(("policy").to_owned())),
                    (
                        "detail".to_owned(),
                        Value::from(("native tool invocation requires user input").to_owned()),
                    ),
                ]));
            }
        }
        match self.event(after, bodies, targets).await {
            Ok(settled) => {
                if let (ToolExecutionOutcome::Completed(result), Some(output)) =
                    (&mut outcome, settled.bodies.get("/items/0"))
                {
                    match serde_json::from_value(output.clone()) {
                        Ok(output) => result.result.output = output,
                        Err(e) => {
                            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                                e.to_string(),
                            ));
                        }
                    }
                }
                outcome
            }
            Err(error) => ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(error)),
        }
    }
}

/// Owned mutator registration for a shared host. `AgentBuilder` takes owned
/// implementations, and one driver owns one hook session, so this wrapper is
/// also where an aborted startup releases the SDK transports.
pub struct HostMutator(Arc<AhpHost>);

impl HostMutator {
    pub fn new(host: Arc<AhpHost>) -> Self {
        Self(host)
    }
}

impl Drop for HostMutator {
    fn drop(&mut self) {
        // Ordinary completion awaits `close_session`. Rejected startup and
        // aborted actors still release SDK transports while the Tokio runtime
        // remains alive.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let host = self.0.clone();
            runtime.spawn(async move {
                if let Err(error) = host.shutdown().await {
                    tracing::warn!(%error, "AHP session shutdown failed");
                }
            });
        }
    }
}

#[async_trait]
impl LoopMutator for HostMutator {
    async fn on_session_start(
        &self,
        payload: SessionStartPayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        LoopMutator::on_session_start(self.0.as_ref(), payload, ctx).await
    }
    async fn on_model_request(
        &self,
        payload: ModelRequestPayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        LoopMutator::on_model_request(self.0.as_ref(), payload, ctx).await
    }
    async fn on_model_response(
        &self,
        payload: ModelResponsePayload<'_>,
        ctx: HookCtx<'_>,
    ) -> Result<(), LoopError> {
        LoopMutator::on_model_response(self.0.as_ref(), payload, ctx).await
    }
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        LoopMutator::mutate(self.0.as_ref(), cursor, ctx).await
    }
}

/// Owned awaited-fact registration for a shared host.
pub struct HostDelivery(Arc<AhpHost>);

impl HostDelivery {
    pub fn new(host: Arc<AhpHost>) -> Self {
        Self(host)
    }
}

#[async_trait]
impl NativeDelivery for HostDelivery {
    async fn deliver(&self, fact: NativeFact<'_>, ctx: HookCtx<'_>) -> Result<(), DeliveryError> {
        NativeDelivery::deliver(self.0.as_ref(), fact, ctx).await
    }
}

/// Drains the driver's read-only delivery diagnostics. They describe failed
/// observation, never a failed operation, so they are reported separately from
/// whatever the driver call itself returned.
pub fn drain_delivery_diagnostics<S: ModelSession>(driver: &mut LoopDriver<S>) {
    let failures = driver.take_delivery_errors();
    if failures.is_empty() {
        return;
    }
    for failure in &failures.failures {
        tracing::warn!(
            session = %failure.session_id,
            turn = ?failure.turn_id,
            fact = ?failure.fact,
            error = %failure.error,
            "AHP native fact delivery failed"
        );
    }
    if failures.dropped > 0 {
        tracing::warn!(
            dropped = failures.dropped,
            "AHP native fact delivery diagnostics were discarded by the driver bound"
        );
    }
}

fn closure_reason(outcome: SessionOutcome) -> &'static str {
    match outcome {
        SessionOutcome::Completed => "Kit closed the session after its last turn",
        SessionOutcome::Cancelled => "Kit closed the session after cancellation",
        SessionOutcome::Error => "Kit closed the session after a failure",
    }
}

/// Kit-owned session closure. The loop has no close boundary, so the owner
/// retires any interrupted turn first — that ending is delivered through the
/// loop's terminal funnel — and only then reports `session.end` and shuts the
/// SDK down.
pub async fn close_session<S: ModelSession>(
    driver: &mut LoopDriver<S>,
    host: Option<&Arc<AhpHost>>,
    session: &str,
    outcome: SessionOutcome,
) -> Result<(), LoopError> {
    let retired = driver.retire_interrupted_turn().await;
    drain_delivery_diagnostics(driver);
    // Report the closure Kit performed. Retiring a turn that was still live
    // means the session ended with work in flight, whatever the caller
    // expected; a failed retirement is an error regardless. A turn that had
    // already ended cancelled reported that at its own `turn.end`, so the
    // caller's own outcome stands for the session.
    let outcome = match &retired {
        Err(_) => SessionOutcome::Error,
        Ok(Some(_)) => SessionOutcome::Cancelled,
        Ok(None) => outcome,
    };
    let Some(host) = host else {
        return retired.map(|_| ());
    };
    let transcript = driver.snapshot().transcript;
    let ended = host
        .session_end(session, Some(&transcript), outcome, closure_reason(outcome))
        .await
        .map_err(LoopError::Mutator);
    let shutdown = host.shutdown().await.map_err(LoopError::InvalidState);
    retired.map(|_| ()).and(ended).and(shutdown)
}

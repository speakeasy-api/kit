//! ACP identities travel with persisted parts, not process-local counters.
use agentkit_core::{Delta, ItemKind, Part, PartId, PartKind, TurnCancellation};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, RetryObserver, SessionConfig,
    TurnRequest,
};
use async_trait::async_trait;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
};
pub(super) const KEY: &str = "dev.kit.acp.message_id";
const PROMPT_KEY: &str = "dev.kit.acp.prompt_key";
pub(super) const ORDER_KEY: &str = "dev.kit.acp.presentation_order";
pub(super) const SECTIONS_KEY: &str = "dev.kit.acp.thought_sections";
#[derive(serde::Deserialize)]
pub(super) struct ThoughtSection {
    pub(super) id: String,
    pub(super) text: String,
    #[serde(default)]
    pub(super) group: String,
    #[serde(default)]
    pub(super) order: usize,
}
pub(super) fn stamp_user(items: &mut [agentkit_core::Item]) -> super::wire::MessageId {
    let id = super::wire::MessageId::new(format!("kit-user-{}", crate::session::new_id()));
    for item in items {
        if item.kind == ItemKind::User {
            item.metadata.insert(KEY.into(), id.to_string().into());
        }
    }
    id
}

/// Record the retry key on the same item as its canonical wire identity.
/// The session ID prevents inherited entries from claiming keys in a fork.
/// Replay projects content and message ID, not this private item metadata.
pub(super) fn stamp_prompt(
    items: &mut [agentkit_core::Item],
    request: &super::wire::PromptRequest,
) -> super::wire::MessageId {
    let id = stamp_user(items);
    if let Some(key) = &request.idempotency_key {
        let metadata = serde_json::Value::Object(serde_json::Map::from_iter([
            (
                "sessionId".into(),
                serde_json::Value::String(request.session_id.to_string()),
            ),
            (
                "key".into(),
                serde_json::Value::String(key.as_str().to_owned()),
            ),
        ]));
        for item in items.iter_mut().filter(|item| item.kind == ItemKind::User) {
            item.metadata.insert(PROMPT_KEY.into(), metadata.clone());
        }
    }
    id
}

pub(super) fn restored_prompt_keys(
    session_id: &super::wire::SessionId,
    transcript: &[agentkit_core::Item],
) -> impl Iterator<Item = (super::wire::IdempotencyKey, super::wire::MessageId)> {
    let session_id = session_id.to_string();
    transcript.iter().filter_map(move |item| {
        if item.kind != ItemKind::User {
            return None;
        }
        let private = item.metadata.get(PROMPT_KEY)?;
        if private.get("sessionId")?.as_str()? != session_id {
            return None;
        }
        let key = super::wire::IdempotencyKey::new(private.get("key")?.as_str()?).ok()?;
        let id = item.metadata.get(KEY)?.as_str()?;
        if id.is_empty() {
            return None;
        }
        Some((key, super::wire::MessageId::new(id)))
    })
}

/// Index of committed user messages for the lifetime of one attachment. The
/// actor is the only writer; receive handlers only clone a known message ID.
#[derive(Default)]
pub(super) struct PromptKeys(RwLock<HashMap<super::wire::IdempotencyKey, super::wire::MessageId>>);

impl PromptKeys {
    pub(super) fn restore(
        session_id: &super::wire::SessionId,
        transcript: &[agentkit_core::Item],
    ) -> Self {
        let mut keys = HashMap::new();
        for (key, id) in restored_prompt_keys(session_id, transcript) {
            keys.entry(key).or_insert(id);
        }
        Self(RwLock::new(keys))
    }

    pub(super) fn get(&self, key: &super::wire::IdempotencyKey) -> Option<super::wire::MessageId> {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .cloned()
    }

    pub(super) fn record(&self, key: super::wire::IdempotencyKey, id: super::wire::MessageId) {
        // Recovery is safe: the only guarded mutation inserts a complete pair
        // of inert strings. Driver commitment precedes the lock; no callbacks,
        // I/O, await, or response completion occur while holding it.
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key)
            .or_insert(id);
    }
}

pub(crate) struct Adapter<A> {
    pub(crate) inner: A,
    pub(crate) enabled: bool,
}
pub(crate) struct Session<S> {
    inner: S,
    enabled: bool,
}
pub(crate) struct Turn<T> {
    inner: T,
    identity: Option<Identity>,
    pending: VecDeque<ModelTurnEvent>,
}
struct Identity {
    agent: String,
    thoughts: Vec<String>,
    parts: HashMap<PartId, String>,
    committed_thoughts: usize,
    sections: Vec<ThoughtSection>,
    agent_order: Option<usize>,
    next_order: usize,
}
impl Identity {
    fn new() -> Self {
        Self {
            agent: format!("kit-agent-{}", crate::session::new_id()),
            thoughts: Vec::new(),
            parts: HashMap::new(),
            committed_thoughts: 0,
            sections: Vec::new(),
            agent_order: None,
            next_order: 0,
        }
    }
    fn thought(&mut self, index: usize) -> String {
        while self.thoughts.len() <= index {
            self.thoughts
                .push(format!("kit-thought-{}", crate::session::new_id()));
        }
        self.thoughts[index].clone()
    }
    fn stamp(&mut self, part: &mut Part, thought: &mut usize) {
        match part {
            Part::Text(text) => {
                text.metadata.insert(KEY.into(), self.agent.clone().into());
                if let Some(order) = self.agent_order {
                    text.metadata.insert(ORDER_KEY.into(), order.into());
                }
            }
            Part::Reasoning(reasoning) => {
                if self.sections.is_empty() {
                    reasoning
                        .metadata
                        .insert(KEY.into(), self.thought(*thought).into());
                    *thought += 1;
                } else {
                    // Providers may aggregate several live sections into one final
                    // reasoning part. Preserve their presentation independently of
                    // the provider summary, encrypted data and continuation metadata.
                    let sections = &self.sections[(*thought).min(self.sections.len())..];
                    reasoning.metadata.insert(
                        SECTIONS_KEY.into(),
                        serde_json::Value::Array(
                            sections
                                .iter()
                                .map(|section| {
                                    serde_json::Value::Object(
                                        [
                                            ("id".into(), section.id.clone().into()),
                                            ("text".into(), section.text.clone().into()),
                                            ("group".into(), section.group.clone().into()),
                                            ("order".into(), section.order.into()),
                                        ]
                                        .into_iter()
                                        .collect(),
                                    )
                                })
                                .collect(),
                        ),
                    );
                    *thought = self.sections.len();
                }
            }
            _ => {}
        }
    }
}
#[async_trait]
impl<A: ModelAdapter> ModelAdapter for Adapter<A> {
    type Session = Session<A::Session>;
    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(Session {
            inner: self.inner.start_session(config).await?,
            enabled: self.enabled,
        })
    }
    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}
#[async_trait]
impl<S: ModelSession> ModelSession for Session<S> {
    type Turn = Turn<S::Turn>;
    fn set_retry_observer(&mut self, observer: Option<Arc<dyn RetryObserver>>) {
        self.inner.set_retry_observer(observer);
    }
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        Ok(Turn {
            inner: self.inner.begin_turn(request, cancellation).await?,
            identity: self.enabled.then(Identity::new),
            pending: VecDeque::new(),
        })
    }
    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }
    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}
#[async_trait]
impl<T: ModelTurn> ModelTurn for Turn<T> {
    fn on_cancelled(&mut self) {
        self.pending.clear();
        self.inner.on_cancelled();
    }
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        let mut event = self.inner.next_event(cancellation).await?;
        let Some(identity) = &mut self.identity else {
            return Ok(event);
        };
        match &mut event {
            Some(ModelTurnEvent::ResponseAttemptSuperseded) => *identity = Identity::new(),
            Some(ModelTurnEvent::Delta(Delta::BeginPart { part_id, kind })) => {
                let id = match kind {
                    PartKind::Text => Some(identity.agent.clone()),
                    PartKind::Reasoning => Some(identity.thought(identity.thoughts.len())),
                    _ => None,
                };
                if let Some(id) = id {
                    identity.parts.insert(part_id.clone(), id.clone());
                    self.pending
                        .push_back(ModelTurnEvent::Delta(Delta::SetMetadata {
                            part_id: part_id.clone(),
                            metadata: [(KEY.into(), id.into())].into(),
                        }));
                }
            }
            Some(ModelTurnEvent::Delta(Delta::AppendText { part_id, chunk })) => {
                if identity.parts.get(part_id) == Some(&identity.agent)
                    && identity.agent_order.is_none()
                {
                    identity.agent_order = Some(identity.next_order);
                    identity.next_order += 1;
                }
                if let Some(id) = identity.parts.get(part_id)
                    && identity.thoughts.contains(id)
                {
                    if let Some(section) = identity
                        .sections
                        .iter_mut()
                        .find(|section| section.id == *id)
                    {
                        section.text.push_str(chunk);
                    } else {
                        identity.sections.push(ThoughtSection {
                            id: id.clone(),
                            text: chunk.clone(),
                            group: identity.agent.clone(),
                            order: identity.next_order,
                        });
                        identity.next_order += 1;
                    }
                }
            }
            Some(ModelTurnEvent::Delta(Delta::SetMetadata { part_id, metadata })) => {
                if let Some(id) = identity.parts.get(part_id) {
                    metadata.insert(KEY.into(), id.clone().into());
                }
            }
            Some(ModelTurnEvent::Delta(Delta::CommitPart { part })) => {
                let mut thought = identity.committed_thoughts;
                identity.stamp(part, &mut thought);
                identity.committed_thoughts = thought;
            }
            Some(ModelTurnEvent::Finished(result)) => {
                let mut thought = 0;
                for item in &mut result.output_items {
                    if item.kind == ItemKind::Assistant {
                        for part in &mut item.parts {
                            identity.stamp(part, &mut thought);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(event)
    }
}
